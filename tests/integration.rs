use std::{error::Error, process::Command};

use assert_cmd::prelude::OutputAssertExt;
use assert_fs::{assert::PathAssert, fixture::PathChild, TempDir};
use escargot::CargoBuild;
use predicates::prelude::predicate;

#[test]
fn integration() -> Result<(), Box<dyn Error>> {
    let temp_dir = TempDir::new()?.into_persistent_if(std::env::var("TEST_PERSIST").is_ok());

    eprintln!("Building release cawlr");
    let run = CargoBuild::new()
        .package("cawlr")
        .release()
        .no_default_features()
        .run()?;
    let cawlr = run.path().as_os_str();
    let genome = "extra/sacCer3.fa";

    eprintln!("Preprocessing positive control");
    let pos_output = temp_dir.path().join("pos_control.output");
    Command::new(cawlr)
        .arg("collapse")
        .arg("-i")
        .arg("extra/pos_control.eventalign.txt")
        .arg("-b")
        .arg("extra/pos_control.bam")
        .arg("-o")
        .arg(&pos_output)
        .env("RUST_BACKTRACE", "full")
        .assert()
        .success();

    eprintln!("Preprocessing negative control");
    let neg_output = temp_dir.path().join("neg_control.output");
    Command::new(cawlr)
        .arg("collapse")
        .arg("-i")
        .arg("extra/neg_control.eventalign.txt")
        .arg("-b")
        .arg("extra/neg_control.bam")
        .arg("-o")
        .arg(&neg_output)
        .env("RUST_BACKTRACE", "full")
        .assert()
        .success();

    eprintln!("Preprocessing single read.");
    let single_read_output = temp_dir.path().join("single_read.output");
    Command::new(cawlr)
        .arg("collapse")
        .arg("-i")
        .arg("extra/single_read.eventalign.txt")
        .arg("-b")
        .arg("extra/single_read.bam")
        .arg("-o")
        .arg(&single_read_output)
        .env("RUST_BACKTRACE", "full")
        .assert()
        .success();

    // Indexing
    Command::new(cawlr)
        .arg("index")
        .arg("-i")
        .arg(&single_read_output)
        .assert()
        .success();
    temp_dir
        .child("single_read.output.idx.bed")
        .assert(predicate::path::exists());

    eprintln!("Training on positive control");
    let pos_train = temp_dir.path().join("pos_control.train");
    Command::new(cawlr)
        .arg("train")
        .arg("-g")
        .arg(genome)
        .arg("-i")
        .arg(&pos_output)
        .arg("-o")
        .arg(&pos_train)
        .env("RUST_BACKTRACE", "full")
        .assert()
        .success();

    eprintln!("Training on negative control");
    let neg_train = temp_dir.path().join("neg_control.train");
    Command::new(cawlr)
        .arg("train")
        .arg("-g")
        .arg(genome)
        .arg("-i")
        .arg(&neg_output)
        .arg("-o")
        .arg(&neg_train)
        .env("RUST_BACKTRACE", "full")
        .assert()
        .success();

    eprintln!("Ranking kmers");
    let ranks = temp_dir.path().join("ranks");
    Command::new(cawlr)
        .arg("rank")
        .arg("--neg-ctrl")
        .arg(&neg_train)
        .arg("--pos-ctrl")
        .arg(&pos_train)
        .arg("-o")
        .arg(&ranks)
        .env("RUST_BACKTRACE", "full")
        .assert()
        .success();

    eprintln!("Scoring single read");
    let scores = temp_dir.path().join("single_scores");
    Command::new(cawlr)
        .arg("score")
        .arg("-m")
        .arg("1:TA")
        .arg("-m")
        .arg("2:AT")
        .arg("--neg-ctrl")
        .arg(&neg_train)
        .arg("--pos-ctrl")
        .arg(&pos_train)
        .arg("-i")
        .arg(&single_read_output)
        .arg("-r")
        .arg(&ranks)
        .arg("-g")
        .arg(genome)
        .arg("-o")
        .arg(&scores)
        .env("RUST_BACKTRACE", "1")
        .assert()
        .success();

    eprintln!("Scoring positive controls");
    let pos_scores = temp_dir.path().join("pos_scores");
    Command::new(cawlr)
        .arg("score")
        .arg("--neg-ctrl")
        .arg(&neg_train)
        .arg("--pos-ctrl")
        .arg(&pos_train)
        .arg("-i")
        .arg(&pos_output)
        .arg("-r")
        .arg(&ranks)
        .arg("-g")
        .arg(genome)
        .arg("-o")
        .arg(&pos_scores)
        .env("RUST_BACKTRACE", "1")
        .assert()
        .success();

    eprintln!("Scoring negative controls");
    let neg_scores = temp_dir.path().join("neg_scores");
    Command::new(cawlr)
        .arg("score")
        .arg("--neg-ctrl")
        .arg(&neg_train)
        .arg("--pos-ctrl")
        .arg(&pos_train)
        .arg("-i")
        .arg(&neg_output)
        .arg("-r")
        .arg(&ranks)
        .arg("-g")
        .arg(genome)
        .arg("-o")
        .arg(&neg_scores)
        .env("RUST_BACKTRACE", "1")
        .assert()
        .success();

    eprintln!("Compute pos ctrl kernel density estimate");
    let pos_bkde_model = temp_dir.path().join("pos_bkde_model");
    Command::new(cawlr)
        .arg("model-scores")
        .arg("-i")
        .arg(&pos_scores)
        .arg("--bins")
        .arg("1000")
        .arg("-o")
        .arg(&pos_bkde_model)
        .env("RUST_BACKTRACE", "1")
        .assert()
        .success();

    eprintln!("Compute neg ctrl kernel density estimate");
    let neg_bkde_model = temp_dir.path().join("neg_bkde_model");
    Command::new(cawlr)
        .arg("model-scores")
        .arg("-i")
        .arg(&neg_scores)
        .arg("--bins")
        .arg("1000")
        .arg("-o")
        .arg(&neg_bkde_model)
        .env("RUST_BACKTRACE", "1")
        .assert()
        .success();

    eprintln!("Single molecule analysis");
    let sma_bed = temp_dir.path().join("sma_bed");
    Command::new(cawlr)
        .arg("sma")
        .arg("--neg-ctrl-scores")
        .arg(&neg_bkde_model)
        .arg("--pos-ctrl-scores")
        .arg(&pos_bkde_model)
        .arg("-i")
        .arg(&scores)
        .arg("-o")
        .arg(&sma_bed)
        .env("RUST_BACKTRACE", "1")
        .assert()
        .success();

    temp_dir.close()?;
    Ok(())
}
