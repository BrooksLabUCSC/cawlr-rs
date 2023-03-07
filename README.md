# cawlr 𓅨

[![License](https://img.shields.io/badge/license-BSD_3--Clause-informational)](./LICENSE)

**C**hromatin **a**ccesibility **w**ith **l**ong **r**eads (`cawlr`) is a tool for detecting accessible regions of chromatin on single molecules. The tool works with nanopore data via [`nanopolish`](https://github.com/jts/nanopolish).

<!-- TODO or PacBio data via [`kineticsTools`](https://github.com/PacificBiosciences/kineticsTools). -->

`cawlr` is flexible and can work with data that uses various modifications for chromatin accessibility detection. Outputs of all `cawlr` modules are in Apache Arrow format can be manipulated via any programming language that has an [Apache Arrow](https://arrow.apache.org/install/) library. Furthermore, `cawlr` includes various scripts for plotting and evaluating the results.

## Table of Contents

- [cawlr 𓅨](#cawlr-𓅨)
  - [Table of Contents](#table-of-contents)
  - [Quick Start](#quick-start)
  - [Installation](#installation)
    - [Installing rust via rustup](#installing-rust-via-rustup)
    - [Installing system dependencies](#installing-system-dependencies)
      - [Installing on Ubuntu/Debian](#installing-on-ubuntudebian)
      - [Installing on CentOS 7](#installing-on-centos-7)
      - [Installing on MacOS](#installing-on-macos)
      - [Installing on Windows](#installing-on-windows)
    - [Installing cawlr](#installing-cawlr)
      - [Latest from git](#latest-from-git)
      - [(Optional) Run tests](#optional-run-tests)
    - [Python dependencies](#python-dependencies)
  - [Nanopore data preparation](#nanopore-data-preparation)
  - [Models](#models)
  - [Pipelines](#pipelines)
    - [`cawlr pipeline train-ctrls`](#cawlr-pipeline-train-ctrls)
    - [`cawlr pipeline preprocess-sample`](#cawlr-pipeline-preprocess-sample)
    - [`cawlr pipeline analyze-region`](#cawlr-pipeline-analyze-region)
    - [`cawlr` with BAM files with modification data](#cawlr-with-bam-files-with-modification-data)
      - [Requirements](#requirements)
      - [Command usage](#command-usage)
  - [Example `cawlr` vignette](#example-cawlr-vignette)
  - [QC Scripts](#qc-scripts)
  - [Citations](#citations)

## Quick Start

```bash
# Follow preparing data from Nanopore Data Preparation
$ nanopolish eventalign --read sample.fastq \
    --bam sample.bam --genome ref.fa \
    --scale-events --samples \
    --print-read-names --progress \
    -t 4 | cawlr collapse \
    --bam sample.bam --output sample.collapse.arrow
$ cawlr score -g ref.fa --pos-ctrl pos.model.pickle --neg-ctrl neg.model.pickle \
    -i sample.collapse.arrow -o sample.score.arrow
$ cawlr sma --pos-ctrl-scores pos.scores.pickle --neg-ctrl-scores neg.scores.pickle \
    -i sample.score.arrow -o sample.sma.bed
# TODO The rest of the commands
```

## Installation

### Installing rust via rustup

It is recommended to install the Rust compiler is through [rustup.rs](https://rustup.rs/).

### Installing system dependencies

Ensure you have these installed on your system before installing.

- make
- openblas
- perl
- gcc
- gfortran

#### Installing on Ubuntu/Debian

```bash
sudo apt update
sudo apt install cmake libopenblas-dev
```

#### Installing on CentOS 7

```bash
yum install epel-release
yum install perl make gcc gcc-gfortran openblas-devel
```

#### Installing on MacOS

TODO

#### Installing on Windows

TODO

### Installing cawlr

#### Latest from git

```bash
git clone https://github.com/bsaintjo/cawlr-rs.git
cd cawlr-rs
cargo install --path .
```

While the tool was developed to be as portable as possible, you can sacrifice portability for small speed increases by installing with compiling options shown below. The `fast` feature requires GCC version >= 4.9

```bash
RUSTFLAGS="-C target-cpu=native" cargo install --path . --features fast
```

#### (Optional) Run tests

```bash
cargo test
```

### Python dependencies

## Nanopore data preparation

In order to prepare data for `cawlr` you need to install:

- [`samtools`](http://www.htslib.org/)
- [`nanopolish`](https://github.com/jts/nanopolish)
- [`minimap2`](https://github.com/lh3/minimap2)

Example command of running nanopolish to set up inputs

```bash
$ nanopolish index -d path/to/fast5 reads.fasta
$ minimap2 -ax map-ont --sam-hit-only --secondary=no genome.fasta reads.fasta | \
    samtools sort -o aln.sorted.bam -T reads.tmp
$ samtools index aln.sorted.bam
$ nanopolish eventalign --reads reads.fasta \
    --bam aln.sorted.bam \
    --genome genome.fasta \
    --scale-events --samples \
    --print-read-names >eventalign.txt
```

<!-- TODO Confirm that cawlr collapse will fail without `--sam-hit-only` -->

For training the models, its good practice to avoid using multiple alignments for the same read.
To avoid this, you can filter the bam files with the command below before running `nanopolish`. The command filters with the `-f 20` to filter out reads with a MAPQ below 20, and `-F 0x900` removes supplementary and secondary reads as well.

```bash
# After minimap2 command
$ samtools view -b -q 20 -F 0x900 aln.sorted.bam >filtered.aln.sorted.bam
```

Once completed you can confirm that alignments have been filtered correctly with:

```bash
samtools flagstats filtered.aln.sorted.bam
```

## Models

TODO: Point out the models that are provided by `cawlr`

## Pipelines

### `cawlr pipeline train-ctrls`

### `cawlr pipeline preprocess-sample`

### `cawlr pipeline analyze-region`

### `cawlr` with BAM files with modification data

The `cawlr` tool is able to work with BAM files that contain modification data through the MM and ML tags. This is useful if you are using third-party tools such as [`megalodon`](https://github.com/nanoporetech/megalodon) or [Pac-Bio based tools](https://github.com/PacificBiosciences/primrose).

#### Requirements

File names in example representing requirements is shown in parentheses

- BAM files for positive/negative control (`pos.bam`/`neg.bam`)
- BAM file for sample of interest (`sample.bam`)
- Tag for the modification (`C+m` for cytosine methylation)
  - Currently `cawlr` only supports the modification calls on a single strand
  - For more information on the proper format, you can use `samtools view` to look for the specific tag or look at the [SAMtags specification](https://samtools.github.io/hts-specs/SAMtags.pdf)

```bash
cawlr model-scores -i pos.bam -t "C+m" -o pos-model-scores.pickle
cawlr model-scores -i neg.bam -t "C+m" -o neg-model-scores.pickle
cawlr sma \
  -i sample.bam \
  -t "C+m" \
  --pos-ctrl-scores pos-model-scores.pickle \
  --neg-ctrl-scores neg-model-scores.pickle \
  -o sample.bed
```

#### Command usage

## Example `cawlr` vignette

TODO

## QC Scripts

## Citations

Parts of the code have been adapted from [NP-SMLR](https://github.com/imatrm/NP-SMLR) package
