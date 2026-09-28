//! Metadata checks on fixtures captured from Windows-created pools with
//! `spaces fixture` (only the metadata the parser reads is stored), so they
//! run without the multi-gigabyte corpus.

mod common;

use std::fs::{self, File};
use std::path::Path;

use storage_spaces::Pool;
use storage_spaces::io::SparseImage;

#[test]
fn fixtures_match_windows() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut dirs: Vec<_> = fs::read_dir(&root).unwrap().map(|e| e.unwrap().path()).collect();
    dirs.sort();
    assert!(!dirs.is_empty());
    for dir in dirs {
        let m = common::manifest(&dir);
        let n = m["disks"].as_array().unwrap().len();
        let disks: Vec<SparseImage> = (0..n)
            .map(|i| SparseImage::read_from(File::open(dir.join(format!("disk{i}.fixture"))).unwrap()).unwrap())
            .collect();
        let pool = Pool::open(disks).unwrap();
        common::check_metadata(&pool, &m, &dir);
        for space in pool.user_spaces() {
            pool.open_space(space.id())
                .unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        }
    }
}

/// The mirror dirty region log after the ways a space can end (batch 9 of
/// tools/gen-corpus.sh): the first write adds its extent run in a new
/// generation, and only disconnecting the space empties the log again, while
/// idle time, a read-only pool, detaching the disks and a Windows restart
/// (drtkeep stayed attached through one) keep the run listed.
#[test]
fn dirty_region_log_after_each_ending() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for (name, listed) in [
        ("drtdism", true),
        ("drtidle1", true),
        ("drtidle5", true),
        ("drtidle15", true),
        ("drtro", true),
        ("drtkeep", true),
        ("drtdisc", false),
        ("drtnowrite", false),
    ] {
        let dir = root.join(name);
        let disks: Vec<SparseImage> = (0..2)
            .map(|i| SparseImage::read_from(File::open(dir.join(format!("disk{i}.fixture"))).unwrap()).unwrap())
            .collect();
        let pool = Pool::open(disks).unwrap();
        let space = pool.user_spaces().next().unwrap();
        let reader = pool.open_space(space.id()).unwrap();
        let log = reader.dirty_regions().unwrap();
        let generations: Vec<u64> = log
            .copies()
            .iter()
            .map(|c| c.header.as_ref().unwrap().generation)
            .collect();
        if listed {
            assert_eq!(reader.listed_mirror_runs(), 1, "{name}");
            assert!(log.is_dirty(0), "{name}");
            assert_eq!(generations, [0, 1], "{name}");
        } else {
            assert_eq!(reader.listed_mirror_runs(), 0, "{name}");
            assert_eq!(generations, [0, 0], "{name}");
        }
    }
}
