//! Checks against pools created by Windows (see tools/gen-corpus.sh and
//! tools/fetch-corpus.sh). Each pool directory holds the member images and
//! a manifest with Windows' own view of the layout. Pools that are not
//! present locally are skipped.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use serde_json::Value;
use storage_spaces::format::{Resiliency, SLAB_SIZE};
use storage_spaces::io::ReadAt;
use storage_spaces::segments::SegmentKind;
use storage_spaces::{Pool, Space, testpattern};

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

fn manifest(dir: &Path) -> Value {
    let text = fs::read_to_string(dir.join("manifest.json")).unwrap();
    serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap()
}

fn open(dir: &Path, m: &Value) -> Pool<File> {
    let n = m["disks"].as_array().unwrap().len();
    let files = (0..n)
        .map(|i| File::open(dir.join(format!("disk{i}.img"))).unwrap())
        .collect();
    Pool::open(files).unwrap()
}

fn family<'p>(pool: &'p Pool<File>, root: &'p Space) -> Vec<&'p Space> {
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        out.extend(pool.children(out[i].id()));
        i += 1;
    }
    out
}

#[test]
fn metadata_matches_windows() {
    for dir in corpus() {
        let m = manifest(&dir);
        let pool = open(&dir, &m);
        assert!(pool.warnings.is_empty(), "{}: {:?}", dir.display(), pool.warnings);
        assert_eq!(pool.guid.to_string(), m["pool"]["guid"].as_str().unwrap());
        assert_eq!(
            format!("Version {}", pool.version),
            m["pool"]["version"].as_str().unwrap()
        );
        // Spaces inherit the pool's sector size.
        assert_eq!(
            Some(pool.logical_sector_size as u64),
            m["space"]["logical_sector"].as_u64()
        );
        assert_eq!(
            Some(pool.physical_sector_size as u64),
            m["space"]["physical_sector"].as_u64()
        );
        let s = &m["space"];
        let space = pool.find_space(s["name"].as_str().unwrap()).expect("space not found");
        // Manifests from the first generator version recorded the pool GUID here.
        if s["guid"] != m["pool"]["guid"] {
            assert_eq!(space.info.guid.to_string(), s["guid"].as_str().unwrap());
        }
        assert_eq!(space.info.size, s["size"].as_u64());
        let p = space.info.policy.unwrap();
        let resiliency = match p.resiliency {
            Resiliency::Simple => "Simple",
            Resiliency::Mirror => "Mirror",
            Resiliency::Parity => "Parity",
            Resiliency::Other(_) => "?",
        };
        assert_eq!(resiliency, s["resiliency"].as_str().unwrap());
        assert_eq!(Some(p.columns), s["columns"].as_u64());
        assert_eq!(Some(p.copies), s["copies"].as_u64());
        assert_eq!(Some(p.redundancy), s["redundancy"].as_u64());
        assert_eq!(Some(p.interleave), s["interleave"].as_u64());

        // Windows lists the extents of the space and of its hidden children.
        let guid_of = |unique_id: &str| {
            m["disks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["unique_id"] == unique_id)
                .unwrap()["spaces_guid"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let expected: BTreeSet<_> = m["extents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                let n = |k: &str| e[k].as_u64().unwrap();
                (
                    n("column"),
                    n("copy"),
                    n("virtual_offset"),
                    n("size"),
                    n("physical_offset"),
                    guid_of(e["disk_unique_id"].as_str().unwrap()),
                )
            })
            .collect();
        let actual: BTreeSet<_> = family(&pool, space)
            .iter()
            .flat_map(|sp| &sp.extents)
            .map(|e| {
                let disk = pool.disks[&e.disk_id].guid.to_string();
                (
                    e.column,
                    e.copy,
                    e.virtual_slab * SLAB_SIZE,
                    e.slab_count * SLAB_SIZE,
                    e.physical_slab * SLAB_SIZE,
                    disk,
                )
            })
            .collect();
        assert_eq!(actual, expected, "{}", dir.display());
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
    for name in ["mirror2", "mirror3", "parity3", "parity4"] {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/pools").join(name);
        if !is_complete_pool(&dir) {
            continue;
        }
        let m = manifest(&dir);
        for victim in 0..m["disks"].as_array().unwrap().len() {
            let broken = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let devices: Vec<Flaky> = (0..m["disks"].as_array().unwrap().len())
                .map(|i| Flaky {
                    file: File::open(dir.join(format!("disk{i}.img"))).unwrap(),
                    broken: if i == victim { broken.clone() } else { Default::default() },
                })
                .collect();
            let pool = Pool::open(devices).unwrap();
            let reader = pool.open_space(pool.find_space(name).unwrap().id()).unwrap();
            broken.store(true, std::sync::atomic::Ordering::Relaxed);
            let mut block = vec![0u8; testpattern::BLOCK];
            for offset in (0..reader.size()).step_by(0x40000 + 0x1000) {
                let offset = offset / 4096 * 4096;
                reader.read_exact_at(&mut block, offset).unwrap();
                assert_eq!(testpattern::verify(&block, offset, name), None, "{name} without disk {victim}");
            }
        }
    }
}
