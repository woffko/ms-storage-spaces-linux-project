//! The quick checks of `refs::health` (`fs.refs.*`): every corpus volume
//! passes them as Windows left it, and each check finds what is changed
//! in a fixture volume, with the evidence.

use std::fs::File;
use std::path::Path;

use refs::checksum::{crc32c, crc64, sha256};
use refs::health::quick_checks;
use storage_spaces::io::{ReadAt, SparseImage};
use storage_spaces::report::{Check, Report, Status, Verdict};

/// Volumes whose log holds records past the checkpoint (they were detached
/// without being dismounted first: tools/vm/New-RefsVolume.ps1 took the
/// disk offline first only later), so they are suspect.
const LOG_PAST_THE_CHECKPOINT: &[&str] = &["r314integ", "r314mirror"];

fn manifest(dir: &Path) -> serde_json::Value {
    serde_json::from_str(
        std::fs::read_to_string(dir.join("manifest.json"))
            .unwrap()
            .trim_start_matches('\u{feff}'),
    )
    .unwrap()
}

fn verdict(checks: &[Check]) -> Verdict {
    let mut r = Report::new("volume");
    r.checks = checks.to_vec();
    r.verdict()
}

fn status<'c>(checks: &'c [Check], id: &str) -> &'c Check {
    checks
        .iter()
        .find(|c| c.id == id)
        .unwrap_or_else(|| panic!("no {id} in {checks:#?}"))
}

/// Every volume of the corpus (testdata/refs; skipped when missing),
/// plain or inside a space, passes the quick checks: healthy, but the
/// volumes whose log Windows would replay. ReFS 1.x and 3.1 keep logs
/// refs does not read.
#[test]
fn corpus_volumes_pass_as_windows_left_them() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/refs");
    let Ok(entries) = std::fs::read_dir(&root) else {
        eprintln!("no ReFS corpus in {}, skipping", root.display());
        return;
    };
    let mut dirs: Vec<_> = entries
        .map(|e| e.unwrap().path())
        .filter(|p| p.join("manifest.json").exists())
        .collect();
    dirs.sort();
    for dir in dirs {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let m = manifest(&dir);
        let checks = if dir.join("disk.img").exists() {
            let f = File::open(dir.join("disk.img")).unwrap();
            quick_checks(&f, m["partition_offset"].as_u64().unwrap(), "p2", &|o| {
                format!("{o:#x}")
            })
        } else {
            let files: Vec<File> = (0..)
                .map_while(|i| File::open(dir.join(format!("disk{i}.img"))).ok())
                .collect();
            let pool = storage_spaces::Pool::open(files).unwrap();
            let space = pool.user_spaces().next().unwrap();
            let reader = pool.open_space(space.id()).unwrap();
            quick_checks(&reader, m["partition_offset"].as_u64().unwrap(), "p2", &|o| {
                reader.describe(o, &[])
            })
        };
        let ids: Vec<&str> = checks.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "fs.refs.boot",
                "fs.refs.superblock",
                "fs.refs.checkpoint",
                "fs.refs.log"
            ],
            "{name}"
        );
        for id in ["fs.refs.boot", "fs.refs.superblock", "fs.refs.checkpoint"] {
            assert_eq!(status(&checks, id).status, Status::Ok, "{name}: {checks:#?}");
        }
        let log = status(&checks, "fs.refs.log");
        let version = m["refs_version"].as_str().unwrap();
        let expected = if LOG_PAST_THE_CHECKPOINT.contains(&name.as_str()) {
            Status::Suspect
        } else if version.starts_with("1.") || version == "3.1" {
            Status::Skipped
        } else {
            Status::Ok
        };
        assert_eq!(log.status, expected, "{name}: {log:#?}");
        if expected == Status::Suspect {
            assert_eq!(verdict(&checks), Verdict::Suspect);
            assert!(log.evidence[1].expected.as_deref().unwrap().starts_with("older than"));
        } else {
            assert_eq!(verdict(&checks), Verdict::Healthy, "{name}");
        }
    }
}

fn fixture(name: &str) -> (SparseImage, u64) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let image = SparseImage::read_from(File::open(dir.join("disk.fixture")).unwrap()).unwrap();
    (image, manifest(&dir)["partition_offset"].as_u64().unwrap())
}

fn read(image: &SparseImage, at: u64, len: usize) -> Vec<u8> {
    let mut b = vec![0u8; len];
    image.read_exact_at(&mut b, at).unwrap();
    b
}

fn le32(b: &[u8], at: usize) -> usize {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// The volume's geometry: (cluster or block of 1.x, units, ReFS 1.x).
fn geometry(image: &SparseImage, offset: u64) -> (u64, u64, bool) {
    let boot = read(image, offset, 512);
    let bytes = le64(&boot, 0x18) * u64::from(u32::from_le_bytes(boot[0x20..0x24].try_into().unwrap()));
    let cluster = u64::from(u32::from_le_bytes(boot[0x20..0x24].try_into().unwrap()))
        * u64::from(u32::from_le_bytes(boot[0x24..0x28].try_into().unwrap()));
    let v1 = boot[0x28] == 1;
    let unit = if v1 { 0x4000 } else { cluster };
    (unit, bytes / unit, v1)
}

/// Seals the reference to itself at `at` (`len` bytes) of a superblock or
/// checkpoint page: the checksum over the first `unit` bytes with the
/// reference zeroed, where the reference says.
fn seal(page: &mut [u8], at: usize, len: usize, unit: usize, v1: bool) {
    let (kind, sum_at, sum_len) = if v1 {
        (
            page[at + 0x0a],
            at + 8 + page[at + 0x0b] as usize,
            page[at + 0x0c] as usize,
        )
    } else {
        (
            page[at + 0x22],
            at + 0x20 + page[at + 0x23] as usize,
            le32(page, at + 0x24),
        )
    };
    let mut copy = page[..unit].to_vec();
    copy[at..at + len].fill(0);
    let sum = match kind {
        1 => crc32c(&copy).to_le_bytes().to_vec(),
        2 => crc64(&copy).to_le_bytes().to_vec(),
        4 => sha256(&copy).to_vec(),
        other => panic!("checksum kind {other}"),
    };
    page[sum_at..sum_at + sum_len].copy_from_slice(&sum[..sum_len]);
}

/// The superblock as Windows writes its copies at units n-2 and n-3 (a
/// fixture holds only what reading needs: the primary). `change` alters
/// a copy's content before it is sealed.
fn add_copies(image: &mut SparseImage, offset: u64, change: impl Fn(u64, &mut Vec<u8>)) {
    let (unit, units, v1) = geometry(image, offset);
    let primary = read(image, offset + 0x1e * unit, unit as usize);
    let (at, len) = if v1 {
        (le32(&primary, 0x58), le32(&primary, 0x5c))
    } else {
        (le32(&primary, 0x78), le32(&primary, 0x7c))
    };
    for n in [units - 2, units - 3] {
        let mut page = primary.clone();
        if v1 {
            page[0..8].copy_from_slice(&n.to_le_bytes());
        } else {
            page[0x20..0x28].copy_from_slice(&n.to_le_bytes());
        }
        page[at..at + 8].copy_from_slice(&n.to_le_bytes());
        change(n, &mut page);
        seal(&mut page, at, len, unit as usize, v1);
        image.insert(offset + n * unit, &page);
    }
}

fn checks(image: &SparseImage, offset: u64) -> Vec<Check> {
    quick_checks(image, offset, "p2", &|o| format!("byte {o:#x}"))
}

#[test]
fn superblock_copies_must_be_valid_and_agree() {
    for name in ["r314small", "r314sha", "r12small"] {
        let (mut image, offset) = fixture(name);
        let (unit, units, v1) = geometry(&image, offset);
        let c = checks(&image, offset);
        assert_eq!(status(&c, "fs.refs.boot").status, Status::Ok, "{name}: {c:#?}");
        assert_eq!(status(&c, "fs.refs.checkpoint").status, Status::Ok, "{name}: {c:#?}");
        let sb = status(&c, "fs.refs.superblock");
        if sb.status == Status::Ok {
            // Copies as this sealing makes them are what Windows wrote.
            let before = read(&image, offset + (units - 2) * unit, unit as usize);
            add_copies(&mut image, offset, |_, _| {});
            assert_eq!(
                read(&image, offset + (units - 2) * unit, unit as usize),
                before,
                "{name}"
            );
        } else {
            // The fixture holds the primary only (reading needs no more).
            assert!(
                sb.evidence
                    .iter()
                    .all(|e| e.found.as_deref().unwrap().contains("(all zero)")),
                "{name}: {sb:#?}"
            );
            add_copies(&mut image, offset, |_, _| {});
            let c = checks(&image, offset);
            assert_eq!(status(&c, "fs.refs.superblock").status, Status::Ok, "{name}: {c:#?}");
        }
        // A copy missing: zeros where Windows keeps it.
        let mut gone = image.clone();
        gone.insert(offset + (units - 2) * unit, &vec![0u8; unit as usize]);
        let c = checks(&gone, offset);
        let sb = status(&c, "fs.refs.superblock");
        assert_eq!(sb.status, Status::Suspect, "{name}: {sb:#?}");
        assert!(
            sb.evidence[0].found.as_deref().unwrap().contains("(all zero)"),
            "{name}: {sb:#?}"
        );
        // One copy says something else: the checkpoints it lists, say.
        let list = if v1 { 0x50 } else { 0x70 };
        add_copies(&mut image, offset, |n, page| {
            if n == units - 3 {
                let at = le32(page, list);
                page[at] ^= 1;
            }
        });
        let c = checks(&image, offset);
        let sb = status(&c, "fs.refs.superblock");
        assert_eq!(sb.status, Status::Suspect, "{name}: {sb:#?}");
        let e = &sb.evidence[0];
        assert!(e.what.ends_with(&format!("{:#x}", units - 3)), "{name}: {e:?}");
        assert!(
            e.expected.as_deref().unwrap().starts_with("the same as at"),
            "{name}: {e:?}"
        );
        // The primary gone: the copies still read, but suspect.
        add_copies(&mut image, offset, |_, _| {});
        image.insert(offset + 0x1e * unit, &vec![0u8; unit as usize]);
        assert!(refs::Volume::open(&image, offset).is_ok() || v1, "{name}");
        let c = checks(&image, offset);
        assert_eq!(
            status(&c, "fs.refs.superblock").status,
            Status::Suspect,
            "{name}: {c:#?}"
        );
        // None left: failed.
        for n in [units - 2, units - 3] {
            image.insert(offset + n * unit, &vec![0u8; unit as usize]);
        }
        let c = checks(&image, offset);
        assert_eq!(
            status(&c, "fs.refs.superblock").status,
            Status::Failed,
            "{name}: {c:#?}"
        );
        assert_eq!(verdict(&c), Verdict::Failed);
    }
}

#[test]
fn a_damaged_checkpoint_leaves_the_other_and_two_fail() {
    for name in ["r314small", "r12small"] {
        let (mut image, offset) = fixture(name);
        add_copies(&mut image, offset, |_, _| {});
        let (unit, _, v1) = geometry(&image, offset);
        let supb = read(&image, offset + 0x1e * unit, unit as usize);
        let list = if v1 { 0x50 } else { 0x70 };
        let points: Vec<u64> = (0..2).map(|i| le64(&supb, le32(&supb, list) + 8 * i)).collect();
        // Break the older one (the other is current).
        let clock = |n: u64| {
            le64(
                &read(&image, offset + n * unit, unit as usize),
                if v1 { 8 } else { 0x60 },
            )
        };
        let older = *points.iter().min_by_key(|&&n| clock(n)).unwrap();
        let mut page = read(&image, offset + older * unit, unit as usize);
        page[0x200] ^= 0xff;
        image.insert(offset + older * unit, &page);
        let c = checks(&image, offset);
        let cp = status(&c, "fs.refs.checkpoint");
        assert_eq!(cp.status, Status::Warning, "{name}: {cp:#?}");
        assert_eq!(cp.evidence.len(), 1);
        assert_eq!(verdict(&c), Verdict::Healthy);
        let newer = *points.iter().find(|&&n| n != older).unwrap();
        let mut page = read(&image, offset + newer * unit, unit as usize);
        page[0x200] ^= 0xff;
        image.insert(offset + newer * unit, &page);
        let c = checks(&image, offset);
        assert_eq!(
            status(&c, "fs.refs.checkpoint").status,
            Status::Failed,
            "{name}: {c:#?}"
        );
        assert_eq!(status(&c, "fs.refs.log").status, Status::Skipped);
    }
}

/// Gives the first record page of the log the log sequence number `lsn`
/// (in the log's current epoch).
fn set_record(image: &mut SparseImage, offset: u64, lsn: u64) {
    let control = (0..0x400u64)
        .map(|lcn| read(image, offset + lcn * 4096, 4096))
        .find(|c| &c[0..4] == b"MLog" && le64(c, 0x28) == 0)
        .expect("a log control page");
    let (epoch, start) = (le64(&control, 0x20), le64(&control, 0xb8));
    let mut record = read(image, offset + start * 4096, 4096);
    record[0..4].copy_from_slice(b"MLog");
    record[4..8].copy_from_slice(&control[4..8]);
    record[0x20..0x28].copy_from_slice(&epoch.to_le_bytes());
    record[0x28..0x30].copy_from_slice(&lsn.to_le_bytes());
    image.insert(offset + start * 4096, &record);
}

#[test]
fn a_log_newer_than_the_checkpoint_is_suspect() {
    let (mut image, offset) = fixture("r314small");
    let c = checks(&image, offset);
    let log = status(&c, "fs.refs.log");
    assert_eq!(log.status, Status::Ok, "{log:#?}");
    // The checkpoint's log sequence number (u64 at 0x70 of the current one).
    let vol = refs::Volume::open(&image, offset).unwrap();
    let lsn = le64(&read(&image, offset + vol.checkpoint.lcn * 4096, 4096), 0x70);
    drop(vol);
    set_record(&mut image, offset, lsn + 1);
    let c = checks(&image, offset);
    let log = status(&c, "fs.refs.log");
    assert_eq!(log.status, Status::Suspect, "{log:#?}");
    let e = &log.evidence[1];
    let newer = format!("{:#x}:{:#x}", (lsn + 1) >> 32, (lsn + 1) as u32);
    assert_eq!(e.found.as_deref(), Some(newer.as_str()));
    assert!(log.advice.as_deref().unwrap().contains("shut it down fully"));
    assert_eq!(verdict(&c), Verdict::Suspect);
    // The log not found: not known, a warning.
    let mut lost = image.clone();
    for lcn in 0..0x400u64 {
        let page = read(&lost, offset + lcn * 4096, 4096);
        if &page[0..4] == b"MLog" && le64(&page, 0x28) == 0 {
            lost.insert(offset + lcn * 4096, &[0u8; 4096]);
        }
    }
    let c = checks(&lost, offset);
    let log = status(&c, "fs.refs.log");
    assert_eq!(log.status, Status::Warning, "{log:#?}");
    assert!(log.summary.contains("no log control page"), "{}", log.summary);
    assert_eq!(verdict(&c), Verdict::Healthy);
}

#[test]
fn a_boot_sector_that_does_not_check_out() {
    let (mut image, offset) = fixture("r314small");
    let mut boot = read(&image, offset, 512);
    boot[0x40] ^= 1;
    image.insert(offset, &boot);
    let c = checks(&image, offset);
    assert_eq!(status(&c, "fs.refs.boot").status, Status::Suspect, "{c:#?}");
    assert_eq!(status(&c, "fs.refs.superblock").status, Status::Skipped);
    // A version refs does not read is left alone: a warning.
    let (mut image, offset) = fixture("r314small");
    let mut boot = read(&image, offset, 512);
    boot[0x28] = 2;
    let sum = refs::checksum::boot_sum(&boot);
    boot[0x16..0x18].copy_from_slice(&sum.to_le_bytes());
    image.insert(offset, &boot);
    let c = checks(&image, offset);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].status, Status::Warning, "{c:#?}");
    assert!(c[0].summary.contains("ReFS 2."), "{}", c[0].summary);
    // Not ReFS: nothing to say.
    assert!(checks(&SparseImage::new(1 << 20), 0).is_empty());
}
