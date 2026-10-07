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

/// The spaces of a manifest that hold the pattern: (name, pattern size).
fn pattern_spaces(m: &Value) -> Vec<(String, Option<u64>)> {
    let mut v = Vec::new();
    // NTFS pools hold files instead (checked on the Linux VM).
    if m["pattern"] != false {
        v.push((
            m["space"]["name"].as_str().unwrap().to_string(),
            m["pattern_size"].as_u64(),
        ));
    }
    for e in m["extra_spaces"].as_array().into_iter().flatten() {
        v.push((e["name"].as_str().unwrap().to_string(), e["pattern_size"].as_u64()));
    }
    v
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
        for (name, pattern) in pattern_spaces(&m) {
            let name = name.as_str();
            let reader = pool.open_space(pool.find_space(name).unwrap().id()).unwrap();
            let pattern = pattern.unwrap_or(reader.size());
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
}

/// Reads the space the way a device-mapper table built from its segments
/// would, straight from the member images.
#[test]
fn segments_match_pattern() {
    for dir in corpus() {
        let m = manifest(&dir);
        let pool = open(&dir, &m);
        for (name, pattern) in pattern_spaces(&m) {
            let name = name.as_str();
            let reader = pool.open_space(pool.find_space(name).unwrap().id()).unwrap();
            let Ok(segments) = reader.segments() else {
                continue; // parity, or data in the cache
            };
            let files: Vec<File> = (0..m["disks"].as_array().unwrap().len())
                .map(|i| File::open(dir.join(format!("disk{i}.img"))).unwrap())
                .collect();
            let pattern = pattern.unwrap_or(reader.size());
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
    let base = [
        "mirror2", "mirror3", "parity3", "parity4", "parity5", "dual7", "mapar", "lrc11", "lrc12",
    ];
    // The same configurations created by Windows 11 24H2.
    let names = base.iter().flat_map(|n| [n.to_string(), format!("{n}_26100")]);
    for name in names {
        let name = name.as_str();
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
            let pattern = m["pattern_size"].as_u64().unwrap_or(reader.size());
            for offset in (0..pattern).step_by(0x40000 + 0x1000) {
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

#[test]
fn dual_parity_survives_any_two_failed_disks() {
    for name in [
        "dual7",
        "dual7_26100",
        "lrc11",
        "lrc12",
        "lrc13",
        "lrc14",
        "lrc15",
        "lrc16",
    ] {
        survives_two_failed_disks(name);
    }
}

fn survives_two_failed_disks(name: &str) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/pools")
        .join(name);
    if !is_complete_pool(&dir) {
        return;
    }
    let m = manifest(&dir);
    let n = m["disks"].as_array().unwrap().len();
    for a in 0..n {
        for b in a + 1..n {
            let flags: Vec<_> = (0..n)
                .map(|_| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)))
                .collect();
            let devices: Vec<Flaky> = (0..n)
                .map(|i| Flaky {
                    file: File::open(dir.join(format!("disk{i}.img"))).unwrap(),
                    broken: flags[i].clone(),
                })
                .collect();
            let pool = Pool::open(devices).unwrap();
            let reader = pool.open_space(pool.find_space(name).unwrap().id()).unwrap();
            flags[a].store(true, std::sync::atomic::Ordering::Relaxed);
            flags[b].store(true, std::sync::atomic::Ordering::Relaxed);
            let mut block = vec![0u8; testpattern::BLOCK];
            // About 2000 blocks per pair, spread over all stripe positions.
            let step = 0x3f000 * (reader.size() / 2000 / 0x3f000).max(1);
            let pattern = m["pattern_size"].as_u64().unwrap_or(reader.size());
            for offset in (0..pattern).step_by(step as usize) {
                reader
                    .read_exact_at(&mut block, offset)
                    .unwrap_or_else(|e| panic!("{name}: disks {a}+{b} at {offset:#x}: {e}"));
                assert_eq!(
                    testpattern::verify(&block, offset, name),
                    None,
                    "{name}: disks {a}+{b} at {offset:#x}"
                );
            }
        }
    }
}

/// Every space that carries a GPT (the NTFS pools, `wc4k`) reads with a
/// valid backup header at its last LBA, describing the partitions of the
/// primary: the end of a space reads as Windows wrote it, through the
/// write-back cache too (`wc4k` keeps its backup GPT in the cache only, in
/// a partly valid chunk whose runs count 4 KiB sectors).
#[test]
fn backup_gpt_matches_the_primary() {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    let le32 = |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
    let le64 = |b: &[u8], at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
    let mut checked = 0;
    for dir in corpus() {
        let m = manifest(&dir);
        let pool = open(&dir, &m);
        let sector = pool.logical_sector_size as u64;
        for space in pool.user_spaces() {
            let reader = pool.open_space(space.id()).unwrap();
            let read = |offset: u64, len: u64| {
                let mut b = vec![0u8; len as usize];
                reader.read_exact_at(&mut b, offset).unwrap();
                b
            };
            let primary = read(sector, sector);
            if &primary[..8] != b"EFI PART" {
                continue;
            }
            let what = format!("{} {}", dir.display(), space.name());
            let last = reader.size() / sector - 1;
            assert_eq!(le64(&primary, 32), last, "{what}: primary names the backup at");
            let backup = read(last * sector, sector);
            assert_eq!(&backup[..8], b"EFI PART", "{what}: backup signature");
            let size = le32(&backup, 12) as usize;
            let mut header = backup[..size].to_vec();
            header[16..20].fill(0);
            assert_eq!(crc32(&header), le32(&backup, 16), "{what}: backup header CRC");
            assert_eq!(le64(&backup, 24), last, "{what}: backup my_lba");
            let entries = |h: &[u8]| read(le64(h, 72) * sector, u64::from(le32(h, 80) * le32(h, 84)));
            let theirs = entries(&backup);
            assert_eq!(crc32(&theirs), le32(&backup, 88), "{what}: backup entries CRC");
            assert_eq!(theirs, entries(&primary), "{what}: partitions");
            checked += 1;
        }
    }
    eprintln!("{checked} spaces with a GPT checked");
}

/// A disk taken out of a pool with Remove-PhysicalDisk (pool removed, image
/// removed0.img) keeps its old SPACEDB header and database, but its
/// partition entry is gone, so it is no longer found as a member; the pool
/// reads from the remaining disks.
#[test]
fn removed_disks_are_no_members() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/pools/removed");
    let Ok(removed) = File::open(dir.join("removed0.img")) else {
        return;
    };
    assert_eq!(storage_spaces::gpt::find_spaces_partition(&removed).unwrap(), None);
    let m = manifest(&dir);
    let pool = open(&dir, &m);
    assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    let mut devices: Vec<File> = (0..m["disks"].as_array().unwrap().len())
        .map(|i| File::open(dir.join(format!("disk{i}.img"))).unwrap())
        .collect();
    devices.push(removed);
    let err = Pool::open(devices).err().expect("a removed disk is no member");
    assert!(err.to_string().contains("no Storage Spaces partition"), "{err}");
}
