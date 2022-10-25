use std::{
    ffi::OsStr,
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

#[path = "agg_blocks.rs"]
mod agg_blocks;

use cawlr::{
    collapse::CollapseOptions,
    filter::Region,
    motif::{all_bases, Motif},
    score::ScoreOptions,
    sma::SmaOptions,
    utils,
};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use log::LevelFilter;

#[derive(Debug, Parser)]
struct Args {
    // /// Used to name output bed file
    // #[clap(short, long)]
    // name: String,
    /// Region of interested {chromosome}:{start}-{stop}
    #[clap(short, long)]
    locus: Region,

    /// Where to output results
    #[clap(short, long)]
    output_dir: PathBuf,

    /// Path to bam file to filter on the locus
    #[clap(short, long)]
    bam: PathBuf,

    /// Path to full fastq, doesn't need to be filtered
    #[clap(long)]
    reads: PathBuf,

    /// Path to genome
    #[clap(short, long)]
    genome: PathBuf,

    /// Path to postive control model, from cawlr train
    #[clap(long)]
    pos_model: PathBuf,

    /// Path to postive control scores, from cawlr model-scores
    #[clap(long)]
    pos_scores: PathBuf,

    /// Path to negative control model, from cawlr train
    #[clap(long)]
    neg_model: PathBuf,

    /// Path to negative control scores, from cawlr model-scores
    #[clap(long)]
    neg_scores: PathBuf,

    /// Path to ranks file, from cawlr ranks
    #[clap(long)]
    ranks: PathBuf,

    /// Number of clusters to use for clustering script
    #[clap(long, default_value_t = 3)]
    n_clusters: usize,

    /// Percent of read that should overlap region to be clustered
    #[clap(long)]
    pct: f64,

    /// Motifs to analyze, formatted "2:GC" if second base C is modified
    /// Can have more than one
    #[clap(short, long)]
    motifs: Option<Vec<Motif>>,

    /// Path to nanopolish binary, if not specified will look in $PATH
    #[clap(long)]
    nanopolish_path: Option<PathBuf>,

    /// Path to samtools binary, if not specified will look in $PATH
    #[clap(long)]
    samtools_path: Option<PathBuf>,

    #[clap(long, default_value_t = false)]
    overwrite: bool,
}

pub fn wrap_cmd<F>(msg: &'static str, mut f: F) -> eyre::Result<()>
where
    F: FnMut() -> eyre::Result<()>,
{
    let p = ProgressBar::new_spinner()
        .with_style(
            ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}] {msg}").unwrap(),
        )
        .with_message(msg);
    p.enable_steady_tick(Duration::from_millis(100));
    f()?;
    p.finish_with_message("✅ [{elapsed_precise}] {msg}");
    Ok(())
}

pub fn parse_name_from_output_dir<P: AsRef<Path>>(path: P) -> eyre::Result<String> {
    let name = path
        .as_ref()
        .file_name()
        .ok_or(eyre::eyre!("Invalid input directory"))?
        .to_str()
        .ok_or(eyre::eyre!("Invalid path name"))?;
    Ok(name.to_string())
}

fn cluster_region_cmd<S: AsRef<OsStr>>(
    region: &Region,
    pct: f64,
    n_clusters: usize,
    name: &str,
    sma_path: S,
) -> Command {
    let mut cmd = Command::new("cluster_region.py");
    cmd.arg("-p")
        .arg(pct.to_string())
        .arg("-s")
        .arg(region.start().to_string())
        .arg("-e")
        .arg(region.end().to_string())
        .arg("--suptitle")
        .arg(name)
        .arg("-n")
        .arg(n_clusters.to_string())
        .arg("-i")
        .arg(&sma_path);
    cmd
}

fn main() -> eyre::Result<()> {
    let args = Args::parse();

    if args.overwrite && args.output_dir.exists() {
        fs::remove_dir_all(&args.output_dir)?;
    }
    fs::create_dir_all(&args.output_dir)?;

    let log_file = args.output_dir.join("log.txt");
    simple_logging::log_to_file(log_file, LevelFilter::Info)?;
    log::info!("{args:?}");

    let name = parse_name_from_output_dir(&args.output_dir)?;
    let motifs = args.motifs.ok_or(eyre::eyre!("Need atleast 1 motif"))?;
    let nanopolish = utils::find_binary("nanopolish", &args.nanopolish_path)?;

    let filtered_bam = args.output_dir.join("filtered.bam");
    wrap_cmd("Running samtools", || {
        let samtools = utils::find_binary("samtools", &args.samtools_path)?;
        let mut cmd = Command::new(samtools);
        cmd.arg("view")
            .arg("-hb")
            .arg("--write-index")
            .arg(&args.bam)
            .arg(format!("{}", args.locus))
            .arg("-o")
            .arg(&filtered_bam);
        log::info!("{cmd:?}");
        cmd.output()?;
        Ok(())
    })?;

    let eventalign_path = args.output_dir.join("eventalign.tsv");
    wrap_cmd("nanopolish eventalign", || {
        let eventalign = File::create(&eventalign_path)?;
        let eventalign_stdout = Stdio::from(eventalign.try_clone()?);

        let mut cmd = Command::new(&nanopolish);
        cmd.arg("eventalign")
            .arg("--reads")
            .arg(&args.reads)
            .arg("--bam")
            .arg(&filtered_bam)
            .arg("--genome")
            .arg(&args.genome)
            .arg("--scale-events")
            .arg("--print-read-names")
            .arg("--samples")
            .args(&["-t", "4"])
            .stdout(eventalign_stdout);
        log::info!("{cmd:?}");
        cmd.output()?;
        Ok(())
    })?;

    let collapse = args.output_dir.join("collapse.arrow");
    wrap_cmd("cawlr collapse", || {
        let eventalign = File::open(&eventalign_path)?;
        CollapseOptions::try_new(&args.bam, &collapse)?.run(eventalign)
    })?;

    let scored = args.output_dir.join("score.arrow");
    wrap_cmd("cawlr score", || {
        let mut scoring = ScoreOptions::try_new(
            &args.pos_model,
            &args.neg_model,
            &args.genome,
            &args.ranks,
            &scored,
        )?;
        scoring.motifs(motifs.clone());
        scoring.run(&collapse)
    })?;

    let track_name = format!("{}.cawlr.sma", name);
    let sma = args.output_dir.join(format!("{}.bed", track_name));
    wrap_cmd("cawlr sma", || {
        let mut sma_opts =
            SmaOptions::try_new(&args.pos_scores, &args.neg_scores, all_bases(), &sma)?;
        sma_opts.track_name(&track_name);
        sma_opts.run(&scored)
    })?;

    let agg_output = args.output_dir.join(format!("{}.tsv", track_name));
    wrap_cmd("Aggregating blocks", || {
        agg_blocks::run(&sma, Some(&agg_output))
    })?;

    wrap_cmd("Splitting by strand", || {
        let mut cmd = Command::new("split_by_strand.py");
        cmd.arg("-i").arg(&sma);
        log::info!("{cmd:?}");
        cmd.output()?;
        Ok(())
    })?;

    let minus_filepath: &Path = sma.file_stem().unwrap().as_ref();
    let minus_filepath = minus_filepath.join(".minus.bed");

    let plus_filepath: &Path = sma.file_stem().unwrap().as_ref();
    let plus_filepath = plus_filepath.join(".plus.bed");

    wrap_cmd("Clustering all reads", || {
        let mut cmd = cluster_region_cmd(&args.locus, args.pct, args.n_clusters, &name, &sma);
        log::info!("{cmd:?}");
        cmd.output()?;
        Ok(())
    })?;

    wrap_cmd("Clustering (+) reads", || {
        let mut cmd = cluster_region_cmd(
            &args.locus,
            args.pct,
            args.n_clusters,
            &name,
            &plus_filepath,
        );
        log::info!("{cmd:?}");
        cmd.output()?;
        Ok(())
    })?;

    wrap_cmd("Clustering (+) reads", || {
        let mut cmd = cluster_region_cmd(
            &args.locus,
            args.pct,
            args.n_clusters,
            &name,
            &minus_filepath,
        );
        log::info!("{cmd:?}");
        cmd.output()?;
        Ok(())
    })?;

    Ok(())
}