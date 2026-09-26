//! Checks against pools created by Windows (see tools/gen-corpus.sh and
//! tools/fetch-corpus.sh). Each pool directory holds the member images and
//! a manifest with Windows' own view of the layout. Pools that are not
//! present locally are skipped.

mod common;

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use common::manifest;
use serde_json::Value;
use storage_spaces::io::ReadAt;
use storage_spaces::segments::SegmentKind;
use storage_spaces::{Pool, testpattern};

fn corpus() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/pools");
    let mut dirs: Vec<_> = fs::read_dir(&root)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| is_complete_pool(p))
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    if dirs.is_empty() {
        eprintln!("no test pools in {}, skipping", root.display());
    }
    dirs
}

/// Pool directories being downloaded are hidden (".name.partial").
fn is_complete_pool(dir: &Path) -> bool {
    let hidden = dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_none_or(|n| n.starts_with('.'));
    !hidden && dir.join("manifest.json").exists()
}

fn open(dir: &Path, m: &Value) -> Pool<File> {
    let n = m["disks"].as_array().unwrap().len();
    let files = (0..n)
        .map(|i| File::open(dir.join(format!("disk{i}.img"))).unwrap())
        .collect();
    Pool::open(files).unwrap()
}

#[test]
fn metadata_matches_windows() {
    for dir in corpus() {
        let m = manifest(&dir);
        let pool = open(&dir, &m);
        common::check_metadata(&pool, &m, &dir);
    }
}

#[test]
fn contents_match_pattern() {
    for dir in corpus() {
        let m = manifest(&dir);
        let pool = open(&dir, &m);
        let name = m["space"]["name"].as_str().unwrap();
        let reader = pool.open_space(pool.find_space(name).unwrap().id()).unwrap();
        let pattern = m["pattern_size"].as_u64().unwrap_or(reader.size());
        // Check one block per 64 KiB (every interleave unit) plus the tail.
        let mut block = vec![0u8; testpattern::BLOCK];
        let step = 0x10000;
        for offset in (0..reader.size()).step_by(step) {
            reader.read_exact_at(&mut block, offset).unwrap();
            if offset < pattern {
                assert_eq!(
                    testpattern::verify(&block, offset, name),
                    None,
                    "{} at {offset:#x}",
                    dir.display()
                );
            } else {
                assert!(
                    block.iter().all(|&b| b == 0),
                    "{}: data beyond the pattern at {offset:#x}",
                    dir.display()
                );
            }
        }
    }
}

/// Reads the space the way a device-mapper table built from its segments
/// would, straight from the member images.
#[test]
fn segments_match_pattern() {
    for dir in corpus() {
        let m = manifest(&dir);
        let pool = open(&dir, &m);
        let name = m["space"]["name"].as_str().unwrap();
        let reader = pool.open_space(pool.find_space(name).unwrap().id()).unwrap();
        let Ok(segments) = reader.segments() else {
            continue; // parity, or data in the cache
        };
        let files: Vec<File> = (0..m["disks"].as_array().unwrap().len())
            .map(|i| File::open(dir.join(format!("disk{i}.img"))).unwrap())
            .collect();
        let pattern = m["pattern_size"].as_u64().unwrap_or(reader.size());
        assert_eq!(segments.last().map(|s| s.start + s.length), Some(reader.size()));
        let mut block = vec![0u8; testpattern::BLOCK];
        for seg in &segments {
            for offset in (seg.start..seg.start + seg.length).step_by(0x10000) {
                match &seg.kind {
                    SegmentKind::Zero => block.fill(0),
                    SegmentKind::Striped { chunk, stripes } => {
                        let rel = offset - seg.start;
                        let unit = rel / chunk;
                        let n = stripes.len() as u64;
                        let s = stripes[(unit % n) as usize];
                        files[s.device]
                            .read_exact_at(&mut block, s.offset + unit / n * chunk + rel % chunk)
                            .unwrap();
                    }
                }
                if offset < pattern {
                    assert_eq!(
                        testpattern::verify(&block, offset, name),
                        None,
                        "{} at {offset:#x}",
                        dir.display()
                    );
                } else {
                    assert!(block.iter().all(|&b| b == 0), "{} at {offset:#x}", dir.display());
                }
            }
        }
    }
}

/// A member device that starts failing all reads once `broken` is set,
/// like a disk that disappears while the pool is in use.
struct Flaky {
    file: File,
    broken: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ReadAt for Flaky {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        if self.broken.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(std::io::Error::other("device gone"));
        }
        self.file.read_exact_at(buf, offset)
    }

    fn size(&self) -> std::io::Result<u64> {
        self.file.size()
    }
}

#[test]
fn reads_fail_over_when_a_disk_disappears() {
    for name in ["mirror2", "mirror3", "parity3", "parity4", "parity5", "dual7"] {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/pools")
            .join(name);
        if !is_complete_pool(&dir) {
            continue;
        }
        let m = manifest(&dir);
        for victim in 0..m["disks"].as_array().unwrap().len() {
            let broken = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let devices: Vec<Flaky> = (0..m["disks"].as_array().unwrap().len())
                .map(|i| Flaky {
                    file: File::open(dir.join(format!("disk{i}.img"))).unwrap(),
                    broken: if i == victim {
                        broken.clone()
                    } else {
                        Default::default()
                    },
                })
                .collect();
            let pool = Pool::open(devices).unwrap();
            let reader = pool.open_space(pool.find_space(name).unwrap().id()).unwrap();
            broken.store(true, std::sync::atomic::Ordering::Relaxed);
            let mut block = vec![0u8; testpattern::BLOCK];
            for offset in (0..reader.size()).step_by(0x40000 + 0x1000) {
                let offset = offset / 4096 * 4096;
                reader.read_exact_at(&mut block, offset).unwrap();
                assert_eq!(
                    testpattern::verify(&block, offset, name),
                    None,
                    "{name} without disk {victim}"
                );
            }
        }
    }
}
