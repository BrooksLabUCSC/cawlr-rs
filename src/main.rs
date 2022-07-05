use std::path::Path;

use anyhow::Result;
use clap::{IntoApp, Parser, Subcommand};
use clap_verbosity_flag::Verbosity;
use collapse::CollapseOptions;
#[cfg(feature = "mimalloc")]
use mimalloc::MiMalloc;

mod arrow;
mod bkde;
mod collapse;
mod context;
mod rank;
mod score;
mod sma;
mod train;
mod utils;

use sma::SmaOptions;
use train::Model;
use utils::CawlrIO;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about=None)]
/// Chromatin accessibility with long reads.
struct Args {
    #[clap(short, long)]
    debug: bool,

    #[clap(flatten)]
    verbose: Verbosity,

    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Collapse {
        #[clap(short, long)]
        /// path to nanopolish eventalign output with samples column
        input: String,

        #[clap(short, long)]
        /// path to output file in Apache Arrow format
        output: String,
        // TODO: Reimplement
        // #[clap(short, long)]
        // /// output only includes data from this chromosome
        // chrom: Option<String>,
        #[clap(short, long, default_value_t = 2048)]
        /// Number of eventalign records to hold in memory.
        capacity: usize, /* #[clap(long)]
                          * /// output only includes data that aligns at or after this position,
                          * /// should be set with --chrom
                          * /// TODO: Throw error if set without --chrom
                          * start: Option<u64>, */

                         /* #[clap(long)]
                          * /// output only includes data that aligns at or before this
                          * position, /// should be set with --chrom
                          * /// TODO: Throw error if set without --chrom
                          * stop: Option<u64> */
    },

    /// For each kmer, train a two-component gaussian mixture model and save
    /// models to a file
    Train {
        #[clap(short, long)]
        /// Positive or negative control output from cawlr collapse
        input: String,

        #[clap(short, long)]
        /// Path to resulting pickle file
        output: String,

        #[clap(short, long)]
        /// Path to genome fasta file
        genome: String,

        #[clap(short, long, default_value_t = 50_000)]
        /// Number of samples per kmer to allow
        samples: usize,
    },

    /// Rank each kmer by the Kulback-Leibler Divergence and between the trained
    /// models
    Rank {
        #[clap(long)]
        /// Positive control output from cawlr train
        pos_ctrl: String,

        #[clap(long)]
        /// Negative control output from cawlr train
        neg_ctrl: String,

        #[clap(short, long)]
        /// Path to output file
        output: String,

        #[clap(long, default_value_t = 2456)]
        /// Ranks are estimated via sampling, so to keep values consistent
        /// between subsequent runs a seed value is used
        seed: u64,

        /// Ranks are estimated via sampling, higher value for samples means it
        /// takes longer for cawlr rank to run but the ranks will be more
        /// accurate
        #[clap(long, default_value_t = 100_000_usize)]
        samples: usize,
    },

    /// Score each kmer with likelihood based on positive and negative controls
    Score {
        #[clap(short, long)]
        /// Path to Apache Arrow file from cawlr collapse
        input: String,

        #[clap(short, long)]
        /// Path to output file
        output: String,

        #[clap(long)]
        /// Positive control file from cawlr train
        pos_ctrl: String,

        #[clap(long)]
        /// Negative control file from cawlr train
        neg_ctrl: String,

        #[clap(short, long)]
        /// Path to rank file from cawlr rank
        ranks: String,

        #[clap(short, long)]
        /// Path to fasta file for organisms genome, must have a .fai file from
        /// samtools faidx
        genome: String,

        #[clap(long, default_value_t = 10.0)]
        cutoff: f64,

        #[clap(short, long)]
        motif: Option<Vec<String>>,
    },
    Sma {
        #[clap(short, long)]
        /// Path to scored data from cawlr score
        input: String,

        // #[clap(short, long)]
        // /// Path to output file
        // output: String,
        #[clap(long)]
        pos_ctrl_scores: String,

        #[clap(long)]
        neg_ctrl_scores: String,

        #[clap(short, long)]
        // Motif context to use
        motifs: Option<Vec<String>>,

        #[clap(long, default_value_t = 10_000_usize)]
        kde_samples: usize,

        #[clap(long, default_value_t = 2456_u64)]
        seed: u64,
    },
}

fn main() -> Result<()> {
    let args = Args::parse();
    env_logger::Builder::new()
        .filter_level(args.verbose.log_level_filter())
        .init();
    match args.command {
        Commands::Collapse {
            input,
            output,
            capacity,
        } => {
            if capacity == 0 {
                let mut cmd = Args::command();
                cmd.error(
                    clap::ErrorKind::InvalidValue,
                    "Capacity must be greater than 0",
                )
                .exit();
            }
            let collapse = CollapseOptions::try_new(&input, &output, capacity)?;
            collapse.run()?;
        }
        Commands::Train {
            input,
            output,
            genome,
            samples,
        } => {
            log::info!("Train command");
            let train = train::Train::try_new(&input, &genome, samples)?;
            let model = train.run()?;
            model.save(output)?;
        }

        Commands::Rank {
            pos_ctrl,
            neg_ctrl,
            output,
            seed,
            samples,
        } => {
            let pos_ctrl_db = Model::load(pos_ctrl)?;
            let neg_ctrl_db = Model::load(neg_ctrl)?;
            let kmer_ranks = rank::RankOptions::new(seed, samples).rank(&pos_ctrl_db, &neg_ctrl_db);
            kmer_ranks.save(output)?;
        }

        Commands::Score {
            input,
            output,
            pos_ctrl,
            neg_ctrl,
            ranks,
            genome,
            cutoff,
            motif,
        } => {
            let fai_file = format!("{}.fai", genome);
            let fai_file_exists = Path::new(&fai_file).exists();
            if !fai_file_exists {
                let mut cmd = Args::command();
                cmd.error(
                    clap::ErrorKind::MissingRequiredArgument,
                    "Missing .fai index file, run samtools faidx on genome file.",
                )
                .exit();
            }
            if motif.is_none() {
                let mut cmd = Args::command();
                cmd.error(clap::ErrorKind::InvalidValue, "Must have motif")
                    .exit();
            }

            for m in motif.as_ref().unwrap().iter() {
                if m.len() > 6 {
                    let mut cmd = Args::command();
                    cmd.error(
                        clap::ErrorKind::InvalidValue,
                        "Length of motif must be less than 6 (size of kmer)",
                    )
                    .exit();
                }
            }

            log::debug!("Motifs parsed: {motif:?}");
            let scoring = score::ScoreOptions::try_new(
                &pos_ctrl, &neg_ctrl, &genome, &ranks, &output, cutoff, motif,
            )?;
            scoring.run(input)?;
        }

        Commands::Sma {
            input,
            // output,
            pos_ctrl_scores: pos_control_scores,
            neg_ctrl_scores: neg_control_scores,
            motifs,
            kde_samples,
            seed,
        } => {
            let sma = SmaOptions::try_new(
                pos_control_scores,
                neg_control_scores,
                motifs,
                kde_samples,
                seed,
            )?;
            sma.run(input)?;
        }
    }
    Ok(())
}
