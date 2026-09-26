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
