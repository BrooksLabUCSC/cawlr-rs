//! Provides StrandMap struct to get strand information from bam files.
//!
//! Intended to eventually replace PlusStrandMap and eventually add more
//! metadata like alignment info from bam
use std::{path::Path, str::from_utf8};

use bam::BamReader;
use eyre::Result;
use fnv::FnvHashMap;

use crate::arrow::metadata::Strand;

#[derive(Default)]
pub struct StrandMap(FnvHashMap<Vec<u8>, Strand>);

#[allow(dead_code)]
impl StrandMap {
    fn new(db: FnvHashMap<Vec<u8>, Strand>) -> Self {
        Self(db)
    }

    pub fn from_bam_file<P: AsRef<Path>>(bam_file: P) -> Result<Self> {
        let mut acc = FnvHashMap::default();
        let reader = BamReader::from_path(bam_file, 2u16)?;
        for record in reader {
            let record = record?;
            let read_name = record.name();

            log::debug!("ReadName from bam: {:?}", from_utf8(read_name));

            let plus_stranded = !record.flag().is_reverse_strand();
            let strand = if plus_stranded {
                Strand::plus()
            } else {
                Strand::minus()
            };
            let entry = acc.entry(read_name.to_owned()).or_insert(strand);
            if *entry != strand {
                *entry = Strand::unknown();
                log::warn!("Multimapped read has strand swap");
            }
        }
        Ok(StrandMap::new(acc))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_from_bam_file() {
        let filepath = "extra/single_read.bam";
        let psmap = StrandMap::from_bam_file(filepath).unwrap();
        let read_id: &[u8] = b"20d1aac0-29de-43ae-a0ef-aa8a6766eb70";
        assert!(psmap.0.contains_key(read_id));
        assert_eq!(psmap.0.get(read_id), Some(&Strand::plus()));
    }

    #[test]
    fn test_from_bam_file_neg_strand() {
        let filepath = "extra/pos_control.bam";
        let psmap = StrandMap::from_bam_file(filepath).unwrap();
        let read_id: &[u8] = b"ca10c9e3-61d4-439b-abb3-078767d19f8c";
        assert!(psmap.0.contains_key(read_id));
        assert_eq!(psmap.0.get(read_id), Some(&Strand::minus()));
    }
}
