use std::{
    collections::HashMap,
    fs::File,
    hash::BuildHasher,
    io::{Read, Seek},
    ops::RangeInclusive,
    path::Path,
};

use anyhow::Result;
use arrow2::io::ipc::write::FileWriter;
use bio::{alphabets::dna, io::fasta::IndexedReader};
use fnv::FnvHashMap;
use rstats::Stats;
use rv::{
    prelude::{Gaussian, Mixture},
    traits::{KlDivergence, Rv},
};

use crate::{
    arrow::{load_apply, save, wrap_writer, Eventalign, Score, ScoredRead, Signal},
    train::Model,
    utils::CawlrIO,
};

pub(crate) struct ScoreOptions {
    pos_ctrl: Model,
    neg_ctrl: Model,
    genome: IndexedReader<File>,
    chrom_lens: FnvHashMap<String, u64>,
    rank: FnvHashMap<String, f64>,
    writer: FileWriter<File>,
    cutoff: f64,
    motifs: Option<Vec<String>>,
}

impl ScoreOptions {
    pub(crate) fn try_new<P>(
        // input: &str,
        pos_ctrl_filepath: &str,
        neg_ctrl_filepath: &str,
        genome_filepath: &str,
        rank_filepath: &str,
        output: P,
        cutoff: f64,
        motifs: Option<Vec<String>>,
    ) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        let schema = ScoredRead::schema();
        let writer = File::create(output)?;
        let writer = wrap_writer(writer, &schema)?;
        let kmer_ranks = FnvHashMap::load(rank_filepath)?;
        let genome = IndexedReader::from_file(&genome_filepath)?;
        let chrom_lens = chrom_lens(&genome);
        let pos_ctrl_db = Model::load(&pos_ctrl_filepath)?;
        let neg_ctrl_db = Model::load(&neg_ctrl_filepath)?;
        Ok(ScoreOptions {
            // input: input.to_owned(),
            pos_ctrl: pos_ctrl_db,
            neg_ctrl: neg_ctrl_db,
            genome,
            chrom_lens,
            rank: kmer_ranks,
            writer,
            cutoff,
            motifs,
        })
    }

    fn close(mut self) -> Result<()> {
        self.writer.finish()?;
        Ok(())
    }

    pub(crate) fn run<P>(mut self, input: P) -> Result<()>
    where
        P: AsRef<Path>,
    {
        let file = File::open(input)?;
        load_apply(file, |eventaligns| {
            let scored = eventaligns
                .into_iter()
                .flat_map(|e| self.score_eventalign(e))
                .collect();
            self.save(scored)
        })?;
        self.close()
    }

    pub(crate) fn save(&mut self, scored: Vec<ScoredRead>) -> Result<()> {
        save(&mut self.writer, &scored)
    }

    fn score_eventalign(&mut self, read: Eventalign) -> Result<ScoredRead> {
        let mut acc = Vec::new();
        let context = Context::from_read(&mut self.genome, &self.chrom_lens, &read)?;
        let data_pos = pos_with_data(&read);
        for pos in read.start_ob()..=read.stop_ob() {
            // Get kmer and check if kmer matches the motifs, if there are any supplied
            let pos_kmer = context.sixmer_at(pos).filter(|k| {
                if let Some(motifs) = &self.motifs {
                    motifs.iter().any(|m| {
                        let m = m.as_bytes();
                        k.starts_with(m)
                    })
                } else {
                    true
                }
            });
            if let Some(kmer) = pos_kmer {
                let kmer = std::str::from_utf8(kmer).unwrap().to_string();
                let signal_score = self.calc_signal_score(pos, &data_pos);
                let skipping_score = self.calc_skipping_score(pos, &data_pos, &context);
                let final_score = signal_score.map_or(skipping_score, |x| x.max(skipping_score));
                let score = Score::new(
                    pos,
                    kmer,
                    signal_score.is_none(),
                    signal_score,
                    skipping_score,
                    final_score,
                );
                acc.push(score)
            }
        }
        let scored_read = ScoredRead::from_read_with_scores(read, acc);
        Ok(scored_read)
    }

    fn calc_skipping_score(
        &self,
        pos: u64,
        data_pos: &FnvHashMap<u64, &Signal>,
        context: &Context,
    ) -> f64 {
        let sur_kmers = context.surrounding(pos);
        let sur_has_data = surround_has_data(pos, data_pos);
        let skipping_scores = sur_kmers
            .into_iter()
            .zip(sur_has_data.into_iter())
            .flat_map(|(kmer, has_data)| {
                let kmer = std::str::from_utf8(kmer).expect("Invalid kmer");
                let pos_presence = self.pos_ctrl.skips().get(kmer);
                let neg_presence = self.neg_ctrl.skips().get(kmer);
                match (pos_presence, neg_presence) {
                    (Some(&pos_presence), Some(&neg_presence)) => {
                        if has_data {
                            Some(pos_presence / (pos_presence + neg_presence))
                        } else {
                            let pos_absent = 1. - pos_presence;
                            let neg_absent = 1. - neg_presence;
                            Some(pos_absent / (pos_absent + neg_absent))
                        }
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>();

        skipping_scores.median().expect("No skipping scores").median
    }

    fn calc_signal_score(&self, pos: u64, data_pos: &FnvHashMap<u64, &Signal>) -> Option<f64> {
        let sur_signals = surrounding_signal(pos, data_pos);
        let best_signal = best_surrounding_signal(sur_signals, &self.rank);

        best_signal.and_then(|sig| {
            let mean = sig.mean();
            let kmer = sig.kmer();
            let pos_mix = self.pos_ctrl.gmms().get(kmer);
            let neg_mix = self.neg_ctrl.gmms().get(kmer);
            match (pos_mix, neg_mix) {
                (Some(pos_gmm), Some(neg_gmm)) => score_signal(mean, pos_gmm, neg_gmm, self.cutoff),
                _ => None,
            }
        })
    }
}

fn surrounding_pos(pos: u64) -> RangeInclusive<u64> {
    let start = if pos < 5 { 0 } else { pos - 5 };
    start..=pos
}

fn surround_has_data<S>(pos: u64, signal_map: &HashMap<u64, &Signal, S>) -> Vec<bool>
where
    S: BuildHasher,
{
    let positions = surrounding_pos(pos);
    positions.map(|p| signal_map.get(&p).is_some()).collect()
}

fn surrounding_signal<'a, S>(
    pos: u64,
    signal_map: &HashMap<u64, &'a Signal, S>,
) -> Option<Vec<&'a Signal>>
where
    S: BuildHasher,
{
    let positions = surrounding_pos(pos);
    let acc = positions
        .flat_map(|p| signal_map.get(&p))
        .cloned()
        .collect::<Vec<_>>();
    if acc.is_empty() {
        None
    } else {
        Some(acc)
    }
}

fn best_surrounding_signal<'a, S>(
    surrounding: Option<Vec<&'a Signal>>,
    ranks: &HashMap<String, f64, S>,
) -> Option<&'a Signal>
where
    S: BuildHasher,
{
    surrounding.and_then(|signals| {
        signals.into_iter().reduce(|x, y| {
            let x_rank = ranks.get(x.kmer());
            let y_rank = ranks.get(y.kmer());
            match (x_rank, y_rank) {
                (None, _) => y,
                (_, None) => x,
                (Some(a), Some(b)) => {
                    if a > b {
                        x
                    } else {
                        y
                    }
                }
            }
        })
    })
}

/// Returns HashMap mapping positions as u64 to the respective signal data
/// Useful for iterating through each base pair position and computing results
/// based on if there is data or not
fn pos_with_data(read: &Eventalign) -> FnvHashMap<u64, &Signal> {
    let mut avail_pos = FnvHashMap::default();
    for signal in read.signal_iter() {
        avail_pos.insert(signal.pos(), signal);
    }
    avail_pos
}

/// Get the size of each chromosome in the genome fasta file. Later used if
/// fetching sequences and want to avoid trying to pull sequence past the end of
/// the chromosome.
fn chrom_lens<R>(genome: &IndexedReader<R>) -> FnvHashMap<String, u64>
where
    R: Read + Seek,
{
    let mut chrom_lens = FnvHashMap::default();
    genome.index.sequences().into_iter().for_each(|sequence| {
        chrom_lens.insert(sequence.name, sequence.len);
    });
    chrom_lens
}

/// Return the Gaussian with the highest component weight. This is a heuristic
/// that expects that the highest weight component in the negative control
/// should represent the data from the true negative control distribution.
pub(crate) fn choose_model(neg_mix: &Mixture<Gaussian>) -> &Gaussian {
    let true_neg = rv::misc::argmax(neg_mix.weights());
    let true_neg = true_neg[0];
    let true_neg = &neg_mix.components()[true_neg];
    true_neg
}

/// Given a Gaussian, and a mixture model containing two gaussians, find the
/// Gaussian in the mixture model that is most disimilar based on KL divergence
/// and return it.
/// Should not fail because train should always produce a Mixture model
/// containing two gaussians
pub(crate) fn choose_pos_model<'a>(
    neg_comp: &Gaussian,
    pos_mix: &'a Mixture<Gaussian>,
) -> &'a Gaussian {
    pos_mix
        .components()
        .iter()
        .reduce(|g1, g2| {
            let g1_kl = g1.kl(neg_comp);
            let g2_kl = g2.kl(neg_comp);
            if g1_kl > g2_kl {
                g1
            } else {
                g2
            }
        })
        .unwrap()
}

/// Score given signal based on GMM from a positive and negative control.
/// Scoring function based on:
///  Wang, Y. et al. Single-molecule long-read sequencing reveals the chromatin
/// basis of gene expression. Genome Res. 29, 1329–1342 (2019).
/// We don't take the ln(score) for now, only after the probability from the Kde
/// later in cawlr sma
fn score_signal(
    signal: f64,
    pos_mix: &Mixture<Gaussian>,
    neg_mix: &Mixture<Gaussian>,
    cutoff: f64,
) -> Option<f64> {
    let neg_mix = choose_model(neg_mix);
    let pos_mix = choose_pos_model(neg_mix, pos_mix);
    let pos_log_proba = pos_mix.f(&signal);
    let neg_log_proba = neg_mix.f(&signal);
    let score = pos_log_proba / (pos_log_proba + neg_log_proba);

    if (pos_mix.ln_f(&signal) > -cutoff) && (neg_mix.ln_f(&signal) > -cutoff) {
        None
    } else {
        Some(score)
    }
}

struct Context {
    context: Vec<u8>,
    read_start: u64,
    start_slop: u64,
    end_slop: u64,
}

impl Context {
    fn new(context: Vec<u8>, read_start: u64, start_slop: u64, end_slop: u64) -> Self {
        Self {
            context,
            read_start,
            start_slop,
            end_slop,
        }
    }

    fn from_read<R>(
        genome: &mut IndexedReader<R>,
        chrom_lens: &FnvHashMap<String, u64>,
        read: &Eventalign,
    ) -> Result<Self>
    where
        R: Read + Seek,
    {
        let chrom = read.chrom();
        let chrom_len = *chrom_lens
            .get(chrom)
            .expect("chromosome missing in chrom_lens, different genome used?");
        let start_slop = read.start_zb().min(5);

        let start = if read.start_zb() < 5 {
            0
        } else {
            read.start_zb() - 5
        };

        let stop = read.seq_stop_zb();
        let end_slop = if (stop + 1) > chrom_len {
            0
        } else {
            5.min(chrom_len - (stop + 1))
        };
        let stop = if (stop + 1) > chrom_len {
            chrom_len
        } else {
            stop + 1
        };
        genome.fetch(chrom, start, stop)?;
        let mut seq = Vec::new();
        genome.read(&mut seq)?;

        if read.strand().is_minus_strand() {
            seq = dna::revcomp(seq)
        }

        Ok(Context::new(seq, read.start_zb(), start_slop, end_slop))
    }

    fn surrounding(&self, pos: u64) -> Vec<&[u8]> {
        let mut acc = Vec::new();
        let true_pos = (pos - self.read_start) + self.start_slop;

        let true_start = if true_pos < 5 { 0 } else { true_pos - 5 };

        let ctxt_len = self.context.len() as u64;
        for base_pos in true_start..=true_pos {
            if (base_pos + 5) < ctxt_len {
                let base_pos = base_pos as usize;
                acc.push(&self.context[base_pos..=base_pos + 5]);
            }
        }
        acc
    }

    /// Returns None if the position is near the end of the chromosome and it
    /// would return a position with a kmer size less than six
    fn sixmer_at(&self, pos: u64) -> Option<&[u8]> {
        let true_pos = (pos - self.read_start) + self.start_slop;
        let true_pos = true_pos as usize;
        self.context.get(true_pos..=true_pos + 5)
    }
}

#[cfg(test)]
mod test {
    use std::io::Cursor;

    use assert_fs::TempDir;

    use super::*;
    use crate::{
        arrow::{load_iter, Strand},
        collapse::CollapseOptions,
    };

    #[test]
    fn test_context() -> Result<()> {
        const FASTA_FILE: &[u8] =
            b">one\nATGCATGCATGCATGCATGCATGCATGCAT\nGCATGCATGCATGCATGCATGCATGCATGC\nATGCAT";
        const FAI_FILE: &[u8] = b"one\t66\t5\t30\t31";

        let mut genome = IndexedReader::new(Cursor::new(FASTA_FILE), FAI_FILE)?;
        let chrom_lens = chrom_lens(&genome);
        assert_eq!(chrom_lens["one"], 66);

        // Truncated start
        let read = Eventalign::empty(String::new(), "one".to_string(), 0, 10, String::new());
        assert_eq!(read.length(), 10);
        assert_eq!(read.start_zb(), 0);
        assert_eq!(read.stop_zb(), 9);
        assert_eq!(read.start_ob(), 1);
        assert_eq!(read.stop_ob(), 10);
        let ctxt = Context::from_read(&mut genome, &chrom_lens, &read)?;
        assert_eq!(ctxt.context.len(), 15);
        // assert_eq!(
        //     std::str::from_utf8(&ctxt.context).unwrap(),
        //     "ATGCATGCATGCATGCATGC"
        // );
        assert_eq!(ctxt.start_slop, 0);
        assert_eq!(ctxt.end_slop, 5);
        assert_eq!(
            ctxt.surrounding(1)
                .into_iter()
                .flat_map(std::str::from_utf8)
                .collect::<Vec<_>>(),
            vec!["ATGCAT", "TGCATG"]
        );

        // Same but minus strand
        let mut read = Eventalign::empty(String::new(), "one".to_string(), 0, 10, String::new());
        *read.strand_mut() = Strand::minus();
        let ctxt = Context::from_read(&mut genome, &chrom_lens, &read)?;
        assert_eq!(ctxt.context.len(), 15);
        assert_eq!(
            std::str::from_utf8(&ctxt.context).unwrap(),
            "CATGCATGCATGCAT"
        );
        assert_eq!(ctxt.start_slop, 0);
        assert_eq!(ctxt.end_slop, 5);
        // assert_eq!(
        //     ctxt.surrounding(1)
        //         .into_iter()
        //         .flat_map(std::str::from_utf8)
        //         .collect::<Vec<_>>(),
        //     vec!["ATGCAT", "TGCATG", "GCATGC", "CATGCA", "ATGCAT", "TGCATG"]
        // );

        // Partial start
        let mut read = Eventalign::empty(String::new(), "one".to_string(), 2, 10, String::new());
        *read.strand_mut() = Strand::minus();
        let ctxt = Context::from_read(&mut genome, &chrom_lens, &read)?;
        assert_eq!(ctxt.context.len(), 17);
        assert_eq!(ctxt.start_slop, 2);
        assert_eq!(ctxt.end_slop, 5);

        // Truncated end
        // Read not possible due to left-adjusted positioning
        let read = Eventalign::empty(String::new(), "one".to_string(), 60, 6, String::new());
        let ctxt = Context::from_read(&mut genome, &chrom_lens, &read)?;
        assert_eq!(ctxt.context.len(), 11);
        assert_eq!(std::str::from_utf8(&ctxt.context).unwrap(), "CATGCATGCAT");

        assert_eq!(ctxt.start_slop, 5);
        assert_eq!(ctxt.end_slop, 0);
        assert_eq!(
            ctxt.surrounding(61)
                .into_iter()
                .flat_map(std::str::from_utf8)
                .collect::<Vec<_>>(),
            vec!["ATGCAT", "TGCATG", "GCATGC", "CATGCA", "ATGCAT"]
        );

        // Partial end
        let read = Eventalign::empty(String::new(), "one".to_string(), 53, 6, String::new());
        let ctxt = Context::from_read(&mut genome, &chrom_lens, &read)?;
        assert_eq!(ctxt.context.len(), 16);
        assert_eq!(ctxt.start_slop, 5);
        assert_eq!(ctxt.end_slop, 2);

        // Middle
        let read = Eventalign::empty(String::new(), "one".to_string(), 30, 10, String::new());
        let ctxt = Context::from_read(&mut genome, &chrom_lens, &read)?;
        assert_eq!(ctxt.context.len(), 20);
        assert_eq!(ctxt.start_slop, 5);
        assert_eq!(ctxt.end_slop, 5);
        Ok(())
    }

    #[test]
    fn test_single_read() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let filepath = "extra/single_read.eventalign.txt";
        let output = temp_dir.path().join("test");
        let collapse = CollapseOptions::try_new(filepath, &output, 2048)?;
        collapse.run()?;

        let output = File::open(output)?;
        let reads = load_iter(output).next().unwrap().unwrap();
        let read = &reads[0];

        let genome_file = "extra/sacCer3.fa";
        let mut genome = IndexedReader::from_file(&genome_file)?;

        let chrom_lens = chrom_lens(&genome);

        let context = Context::from_read(&mut genome, &chrom_lens, read)?;
        assert_eq!(context.start_slop, 5);
        assert_eq!(context.end_slop, 5);

        assert_eq!(
            context
                .surrounding(182522)
                .into_iter()
                .flat_map(std::str::from_utf8)
                .collect::<Vec<_>>(),
            vec!["AACATA", "ACATAT", "CATATT", "ATATTC", "TATTCA", "ATTCAA"]
        );

        Ok(())
    }

}
