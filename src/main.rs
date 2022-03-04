use std::path::Path;

use anyhow::Result;
use bio::io::fasta::IndexedReader;
use clap::{IntoApp, Parser, Subcommand};
use mimalloc::MiMalloc;

mod preprocess;
mod rank;
mod score;
mod train;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about=None)]
/// Chromatin accessibility with long reads.
struct Args {
    #[clap(short, long)]
    debug: bool,

    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Calculates mean per-read per-position and optionally filters data based
    /// on a given region.
    Preprocess {
        #[clap(short, long)]
        /// path to nanopolish eventalign output with samples column
        input: String,

        #[clap(short, long)]
        /// path to output file in parquet format
        output: String,

        #[clap(short, long)]
        /// output only includes data from this chromosome
        chrom: Option<String>,

        #[clap(long)]
        /// output only includes data that aligns at or after this position,
        /// should be set with --chrom
        /// TODO: Throw error if set without --chrom
        start: Option<u32>,

        #[clap(long)]
        /// output only includes data that aligns at or before this position,
        /// should be set with --chrom
        /// TODO: Throw error if set without --chrom
        stop: Option<u32>,
    },

    /// For each kmer, train a two-component gaussian mixture model and save
    /// models to a file
    Train {
        #[clap(short, long)]
        /// Parquet file of positive or negative control from cawlr preprocess
        input: String,

        #[clap(short, long)]
        /// Path to resulting pickle file
        /// TODO: Move from pickle to parquet
        output: String,
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
        #[clap(long, default_value_t = 10_000_usize)]
        samples: usize,
    },

    /// Score each kmer with likelihood based on positive and negative controls
    Score {
        #[clap(short, long)]
        /// Path to parquet file from cawlr preprocess
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

        #[clap(long)]
        /// TODO: Only score with kmers whose KL score is greater cutoff
        cutoff: f64,
    },
    Sma {
        #[clap(short, long)]
        /// Path to scored data from cawlr score
        input: String,

        #[clap(short, long)]
        /// Path to output file
        output: String,

        #[clap(short, long)]
        /// output only includes data from this chromosome
        chrom: String,

        #[clap(long)]
        /// output only includes data that aligns at or after this position,
        /// should be set with --chrom
        /// TODO: Throw error if set without --chrom
        start: u32,

        #[clap(long)]
        /// output only includes data that aligns at or before this position,
        /// should be set with --chrom
        /// TODO: Throw error if set without --chrom
        stop: u32,
    },
}

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();
    match &args.command {
        Commands::Preprocess {
            input,
            output,
            chrom,
            start,
            stop,
        } => {
            log::info!("Preprocess command");
            let nps = preprocess::preprocess(input, chrom, start, stop)?;
            preprocess::write_records_to_parquet(output, nps)?;
        }
        Commands::Train { input, output } => {
            let model_db = train::train(input)?;
            train::save_gmm(output, model_db)?;
        }

        Commands::Rank {
            pos_ctrl,
            neg_ctrl,
            output,
            seed,
            samples,
        } => {
            let pos_ctrl_db = rank::load_models(pos_ctrl)?;
            let neg_ctrl_db = rank::load_models(neg_ctrl)?;
            let kmer_ranks = rank::RankOptions::new(*seed, *samples).rank(pos_ctrl_db, neg_ctrl_db);
            rank::save_kmer_ranks(output, kmer_ranks)?;
        }

        Commands::Score {
            input,
            output,
            pos_ctrl,
            neg_ctrl,
            ranks,
            genome,
            ..
        } => {
            let fai_file = format!("{}.fai", genome);
            let fai_file_exists = Path::new(&fai_file).exists();
            if fai_file_exists {
                let mut cmd = Args::command();
                cmd.error(
                    clap::ErrorKind::MissingRequiredArgument,
                    "Missing .fai index file, run samtools faidx on genome file.",
                )
                .exit();
            }
            let nprs = score::load_nprs(input)?;
            let pos_ctrl_db = rank::load_models(pos_ctrl)?;
            let neg_ctrl_db = rank::load_models(neg_ctrl)?;
            let kmer_ranks = rank::load_kmer_ranks(ranks)?;
            let genome = IndexedReader::from_file(genome)?;
            let snprs = score::score(nprs, pos_ctrl_db, neg_ctrl_db, kmer_ranks, genome);
            score::save_scored_nprs(output, snprs)?;
        }

        Commands::Sma { .. } => {
            unimplemented!()
        }
    }
    Ok(())
}
