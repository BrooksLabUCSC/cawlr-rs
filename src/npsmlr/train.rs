use std::{
    collections::HashMap,
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
};

use eyre::Result;
use linfa::{
    traits::{Fit, Transformer},
    DatasetBase, ParamGuard,
};
use linfa_clustering::{Dbscan, GaussianMixtureModel};
use ndarray::Array;
use rusqlite::{named_params, Connection};
use rv::prelude::{Gaussian, Mixture};

use crate::{
    arrow::{arrow_utils::load_read_arrow_measured, eventalign::Eventalign, metadata::MetadataExt},
    motif::{all_bases, Motif},
    train::{mix_to_mix, Model},
    utils::CawlrIO,
    validated::{self, ValidSampleData},
};

#[derive(Debug)]
pub struct TrainOptions {
    n_samples: usize,
    single: bool,
    dbscan: bool,
    motifs: Vec<Motif>,
    db_path: Option<PathBuf>,
}

impl Default for TrainOptions {
    fn default() -> Self {
        TrainOptions {
            n_samples: 50000,
            single: false,
            dbscan: false,
            motifs: all_bases(),
            db_path: None,
        }
    }
}

fn all_kmers() -> Vec<String> {
    let mut kmers: Vec<String> = vec![String::new()];
    let bases = ["A", "C", "G", "T"];
    for _ in 0..6 {
        let mut acc = Vec::new();
        for base in bases {
            for s in kmers.iter() {
                let mut xs = s.clone();
                xs.push_str(base);
                acc.push(xs);
            }
        }
        kmers = acc;
    }
    kmers
}

impl TrainOptions {
    pub fn n_samples(mut self, n_samples: usize) -> Self {
        self.n_samples = n_samples;
        self
    }

    pub fn single(mut self, single: bool) -> Self {
        self.single = single;
        self
    }

    pub fn dbscan(mut self, dbscan: bool) -> Self {
        self.dbscan = dbscan;
        self
    }

    pub fn motifs(mut self, motifs: Vec<Motif>) -> Self {
        self.motifs = motifs;
        self
    }

    pub fn db_path(mut self, db_path: Option<PathBuf>) -> Self {
        self.db_path = db_path;
        self
    }

    pub fn run<R, W>(self, input: R, mut writer: W) -> Result<()>
    where
        R: Read + Seek,
        W: Write,
    {
        let model = self.run_model(input)?;
        model.save(&mut writer)?;
        Ok(())
    }

    pub fn run_model<R>(self, input: R) -> Result<Model>
    where
        R: Read + Seek,
    {
        log::info!("{self:?}");
        let db_path = {
            match &self.db_path {
                Some(db_path) => db_path.clone(),
                None => std::env::temp_dir().join("npsmlr.db"),
            }
        };
        let mut db = Db::open(db_path)?;
        log::debug!("Database: {db:?}");
        load_read_arrow_measured(input, |eventaligns: Vec<Eventalign>| {
            db.add_reads(eventaligns, &self.motifs)?;
            Ok(())
        })?;

        self.train_gmms(db)
    }

    fn train_gmms(&self, db: Db) -> Result<Model> {
        let mut model = Model::default();
        for kmer in all_kmers() {
            log::info!("Training on kmer {kmer}");
            let samples = db.get_kmer_samples(&kmer, self.n_samples)?;
            log::info!("n samples: {}", samples.len());
            if let Some(validated) = validated::ValidSampleData::validated(samples) {
                match self.train_gmm(validated) {
                    Ok(gmm) => {
                        log::info!("Training successful!");
                        model.insert_gmm(kmer, gmm);
                    }
                    Err(e) => {
                        log::warn!("kmer {kmer} failed to train with error {e}");
                    }
                }
            }
        }
        if model.gmms().is_empty() {
            Err(eyre::eyre!("Not gmms trained due to error. Check logs"))
        } else {
            Ok(model)
        }
    }

    fn train_gmm(&self, samples: ValidSampleData) -> Result<Mixture<Gaussian>> {
        let samples = samples.inner();
        let len = samples.len();
        let shape = (len, 1);
        let means = Array::from_shape_vec(shape, samples).unwrap();
        let mut data = DatasetBase::from(means);
        if self.dbscan {
            let min_points = 3;
            let dataset = Dbscan::params(min_points)
                .tolerance(1e-3)
                .check()
                .unwrap()
                .transform(data);
            let recs = dataset
                .records()
                .as_slice()
                .expect("Getting records failed after DBSCAN");
            let targets = dataset
                .targets()
                .as_slice()
                .expect("Getting targets failed after DBSCAN");

            let filtered: Vec<f64> = recs
                .iter()
                .zip(targets.iter())
                .filter_map(
                    |(&x, cluster)| {
                        if cluster.is_some() {
                            Some(x)
                        } else {
                            None
                        }
                    },
                )
                .collect();
            if filtered.len() < 2 {
                log::warn!("Not enough values left in observations");
                return Err(eyre::eyre!("Not enough values after filtering"));
            }

            let len = filtered.len();
            let shape = (len, 1);
            let filtered_results = Array::from_shape_vec(shape, filtered).unwrap();
            data = DatasetBase::from(filtered_results);
        }

        let n_clusters = if self.single { 1 } else { 2 };
        let n_runs = 10;
        let tolerance = 1e-4f64;
        let gmm = GaussianMixtureModel::params(n_clusters)
            .n_runs(n_runs)
            .tolerance(tolerance)
            .check()?
            .fit(&data)?;
        let mm = mix_to_mix(&gmm);
        Ok(mm)
    }
}

#[derive(Debug)]
struct Db {
    limit: usize,
    connection: Connection,
    counts: HashMap<String, usize>,
}

impl Db {
    fn open<P: AsRef<Path>>(path: P) -> eyre::Result<Self> {
        let path = path.as_ref();
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let db = Db {
            limit: 50000,
            connection: Connection::open(path)?,
            counts: Default::default(),
        };
        db.init()?;
        db.create_idx()?;
        Ok(db)
    }

    fn init(&self) -> eyre::Result<()> {
        self.connection.execute(
            "CREATE TABLE data (
                id      INTEGER PRIMARY KEY,
                kmer    TEXT NOT NULL,
                sample  REAL NOT NULL
            );",
            (),
        )?;
        Ok(())
    }

    fn create_idx(&self) -> eyre::Result<()> {
        self.connection
            .execute("CREATE INDEX kmer_idx on data (kmer)", ())?;
        self.connection.pragma_update(None, "journal_mode", "WAL")?;
        self.connection
            .pragma_update(None, "synchronous", "NORMAL")?;
        self.connection.pragma_update(None, "cache_size", -64000)?;
        Ok(())
    }

    fn add_reads(&mut self, es: Vec<Eventalign>, motifs: &[Motif]) -> eyre::Result<()> {
        let tx = self.connection.transaction()?;
        let mut stmt = tx.prepare("INSERT INTO data (kmer, sample) VALUES (?1, ?2)")?;
        for eventalign in es.into_iter() {
            log::info!("Processing Read: {}", eventalign.name());
            for signal in eventalign.signal_iter() {
                let kmer = &signal.kmer;
                log::debug!("Processing signal kmer: {kmer}");

                // Skip if kmer doesn't match any of the kmers
                if !motifs.iter().any(|m| kmer.starts_with(m.motif())) {
                    log::debug!("Kmer skipped, doesn't match any motifs");
                    continue;
                }

                for sample in signal.samples.iter() {
                    if !(40.0..=170.0).contains(sample) {
                        log::debug!("Uncharacteristic signal measurement {sample}");
                        continue;
                    }
                    if sample.is_finite() {
                        stmt.execute((kmer, sample))?;
                    }
                }
            }
        }
        stmt.finalize()?;

        tx.commit()?;
        Ok(())
    }

    fn get_kmer_samples(&self, kmer: &str, n_samples: usize) -> eyre::Result<Vec<f64>> {
        let mut stmt = self
            .connection
            .prepare("SELECT sample FROM data where kmer = :kmer ORDER BY RANDOM() LIMIT :n")?;
        let rows = stmt.query_map(named_params! {":kmer": kmer, ":n": n_samples}, |row| {
            row.get::<usize, f64>(0)
        })?;
        let mut samples = Vec::new();
        for sample in rows {
            samples.push(sample?)
        }
        Ok(samples)
    }
}

#[cfg(test)]
mod test {
    use assert_fs::TempDir;

    // use quickcheck::quickcheck;
    use super::*;
    use crate::arrow::signal::Signal;

    #[test]
    fn test_empty_model() {
        let tmp_dir = TempDir::new().unwrap();
        let db_path = tmp_dir.join("test.db");
        let db = Db::open(db_path).expect("Failed to open database file");
        let opts = TrainOptions::default();
        assert!(opts.train_gmms(db).is_err());
    }

    #[test]
    fn test_all_kmers() {
        let kmers = all_kmers();
        assert_eq!(kmers.len(), 4096);
    }

    #[test]
    fn test_db_no_kmer() {
        let tmp_dir = TempDir::new().unwrap();
        let db_path = tmp_dir.join("test.db");
        let mut db = Db::open(db_path).expect("Failed to open database file");
        let eventalign = Eventalign::default();
        db.add_reads(vec![eventalign], &all_bases())
            .expect("Unable to add read");
        let samples = db
            .get_kmer_samples("ABCDEF", 5000)
            .expect("Unable to get samples");
        assert!(samples.is_empty());
    }
    #[test]
    fn test_db_motif() {
        let tmp_dir = TempDir::new().unwrap();
        let db_path = tmp_dir.join("test.db");
        let test_cases = vec![
            ("AAAAAA", vec![100.0; 3], true),
            ("AACCCC", vec![100.0; 3], false),
        ];
        let mut db = Db::open(db_path).expect("Failed to open database file");
        let signal_data = test_cases
            .iter()
            .enumerate()
            .map(|(i, (k, xs, _))| Signal::new(i as u64, k.to_string(), 1.0, 0.5, xs.clone()))
            .collect::<Vec<_>>();
        let mut eventalign = Eventalign::default();
        *eventalign.signal_data_mut() = signal_data;
        db.add_reads(vec![eventalign], &[Motif::new("AAA", 2)])
            .expect("Unable to add read");

        for (k, xs, unfiltered) in test_cases.into_iter() {
            let err_msg = format!("Unable to retrieve kmer values for {k}");
            let samples = db.get_kmer_samples(k, 5000).expect(&err_msg);
            if unfiltered {
                assert_eq!(samples, xs);
            } else {
                assert!(samples.is_empty(), "{k}");
            }
        }
    }

    #[test]
    fn test_db_count() {
        let tmp_dir = TempDir::new().unwrap();
        let db_path = tmp_dir.join("test.db");
        let test_cases = vec![
            ("AAAAAA", vec![100.0; 3], true),
            ("GGGGGG", vec![20.0; 4], false),
            ("CCCCCC", vec![300.0; 2], false),
        ];
        let mut db = Db::open(db_path).expect("Failed to open database file");
        let signal_data = test_cases
            .iter()
            .enumerate()
            .map(|(i, (k, xs, _))| Signal::new(i as u64, k.to_string(), 1.0, 0.5, xs.clone()))
            .collect::<Vec<_>>();
        let mut eventalign = Eventalign::default();
        *eventalign.signal_data_mut() = signal_data;
        db.add_reads(vec![eventalign], &all_bases())
            .expect("Unable to add read");
        let mut stmt = db
            .connection
            .prepare("SELECT COUNT(kmer) FROM data where kmer = :kmer")
            .expect("Failed to prepare statement");
        let kmer = "AAAAAA";
        let rows = stmt
            .query_and_then(named_params! {":kmer": kmer}, |row| row.get(0))
            .expect("Failed to get row");
        let res: rusqlite::Result<Vec<usize>> = rows.collect();
        assert_eq!(3, res.unwrap()[0])
    }

    #[test]
    fn test_db() {
        let tmp_dir = TempDir::new().unwrap();
        let db_path = tmp_dir.join("test.db");
        let test_cases = vec![
            ("AAAAAA", vec![100.0; 3], true),
            ("GGGGGG", vec![20.0; 4], false),
            ("CCCCCC", vec![300.0; 2], false),
        ];
        let mut db = Db::open(db_path).expect("Failed to open database file");
        let signal_data = test_cases
            .iter()
            .enumerate()
            .map(|(i, (k, xs, _))| Signal::new(i as u64, k.to_string(), 1.0, 0.5, xs.clone()))
            .collect::<Vec<_>>();
        let mut eventalign = Eventalign::default();
        *eventalign.signal_data_mut() = signal_data;
        db.add_reads(vec![eventalign], &all_bases())
            .expect("Unable to add read");

        for (k, xs, unfiltered) in test_cases.into_iter() {
            let err_msg = format!("Unable to retrieve kmer values for {k}");
            let samples = db.get_kmer_samples(k, 5000).expect(&err_msg);
            if unfiltered {
                assert_eq!(samples, xs);
            } else {
                assert!(samples.is_empty(), "{k}");
            }
        }
    }

    #[test]
    fn test_train() {
        let cases = vec![
            100.0,
            200.0,
            300.0,
            400.0,
            100.2,
            100.3,
            110.0,
            200.3,
            200.2,
            300.3,
            f64::NAN,
            f64::NEG_INFINITY,
            f64::INFINITY,
        ];
        let opts = TrainOptions::default();
        let vs = ValidSampleData::validated(cases).unwrap();
        let xs = opts.train_gmm(vs);
        assert!(xs.is_ok(), "first");

        let case = vec![100.0, 100.0, 0.0, -0.0];
        let vs = ValidSampleData::validated(case).unwrap();
        let xs = opts.train_gmm(vs);
        assert!(xs.is_err(), "not enough different values");
    }
}
