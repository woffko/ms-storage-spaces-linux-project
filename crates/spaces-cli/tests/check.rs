//! `spaces check` on a pool `spaces pool create` makes in image files, with
//! a GPT and an NTFS boot sector written into its space, as it is and with
//! a disk away or the backup GPT header gone; and on corpus pools where
//! they are present.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use storage_spaces::Pool;

fn spaces(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spaces")).args(args).output().unwrap()
}

fn text(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)
}

/// A scratch directory removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *t = c;
    }
    !data
        .iter()
        .fold(!0u32, |c, &b| table[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8))
}

/// Writes a GPT (both copies) with one basic data partition holding an
/// NTFS boot sector and its copy, into the space of `sectors` sectors of
/// 512 bytes, as `write` stores bytes at an offset.
fn partition(sectors: u64, write: &dyn Fn(u64, &[u8])) {
    let mut mbr = vec![0u8; 512];
    mbr[446 + 4] = 0xee;
    mbr[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
    mbr[446 + 12..446 + 16].copy_from_slice(&(sectors as u32 - 1).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xaa;
    write(0, &mbr);
    let (first, last) = (2048u64, sectors - 34);
    let mut entries = vec![0u8; 128 * 128];
    entries[0..16].copy_from_slice(&[
        0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99, 0xc7,
    ]);
    entries[16..32].fill(0x11);
    entries[32..40].copy_from_slice(&first.to_le_bytes());
    entries[40..48].copy_from_slice(&last.to_le_bytes());
    let header = |my: u64, alternate: u64, table: u64| {
        let mut h = vec![0u8; 512];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&my.to_le_bytes());
        h[32..40].copy_from_slice(&alternate.to_le_bytes());
        h[40..48].copy_from_slice(&34u64.to_le_bytes());
        h[48..56].copy_from_slice(&(sectors - 34).to_le_bytes());
        h[56..72].fill(0x22);
        h[72..80].copy_from_slice(&table.to_le_bytes());
        h[80..84].copy_from_slice(&128u32.to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        h[88..92].copy_from_slice(&crc32(&entries).to_le_bytes());
        let crc = crc32(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    };
    write(512, &header(1, sectors - 1, 2));
    write(1024, &entries);
    write((sectors - 33) * 512, &entries);
    write((sectors - 1) * 512, &header(sectors - 1, 1, sectors - 33));
    let mut boot = vec![0u8; 512];
    boot[0..3].copy_from_slice(&[0xeb, 0x52, 0x90]);
    boot[3..11].copy_from_slice(b"NTFS    ");
    boot[0x0b..0x0d].copy_from_slice(&512u16.to_le_bytes());
    boot[0x0d] = 8;
    boot[0x28..0x30].copy_from_slice(&(last - first).to_le_bytes());
    boot[510] = 0x55;
    boot[511] = 0xaa;
    write(first * 512, &boot);
    write(last * 512, &boot);
}

/// Three image files with a pool and a two-way mirror space of 1 GiB,
/// partitioned.
fn pool(dir: &Path) -> Vec<String> {
    std::fs::create_dir_all(dir).unwrap();
    let disks: Vec<String> = (0..3)
        .map(|i| {
            let p = dir.join(format!("disk{i}.img"));
            File::create(&p).unwrap().set_len(3 << 30).unwrap();
            p.display().to_string()
        })
        .collect();
    let mut args = vec!["pool", "create", "--name", "Storage pool", "--yes"];
    args.extend(disks.iter().map(String::as_str));
    let o = spaces(&args);
    assert!(o.status.success(), "{}", text(&o));
    let mut args = vec![
        "space",
        "create",
        "--name",
        "data",
        "--resiliency",
        "mirror",
        "--size",
        "1G",
        "--yes",
    ];
    args.extend(disks.iter().map(String::as_str));
    let o = spaces(&args);
    assert!(o.status.success(), "{}", text(&o));
    let files: Vec<File> = disks
        .iter()
        .map(|p| OpenOptions::new().read(true).write(true).open(p).unwrap())
        .collect();
    let pool = Pool::open(files).unwrap();
    let space = pool.find_space("data").unwrap();
    let writer = pool.open_space_rw(space.id()).unwrap();
    let sectors = writer.size() / 512;
    partition(sectors, &|at, bytes| writer.write_all_at(bytes, at).unwrap());
    writer.flush().unwrap();
    disks
}

#[test]
fn reports_a_healthy_space_and_what_is_wrong_with_one() {
    let dir = Scratch(std::env::temp_dir().join(format!("spaces-check-{}", std::process::id())));
    let disks = pool(&dir.0);
    let all: Vec<&str> = disks.iter().map(String::as_str).collect();

    let o = spaces(&[&["check"][..], &all].concat());
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.starts_with("space \"data\" ("), "{out}");
    assert!(out.contains("of pool \"Storage pool\""), "{out}");
    assert!(out.lines().next().unwrap().ends_with(": HEALTHY"), "{out}");
    for id in [
        "pool.quorum",
        "pool.members",
        "pool.database",
        "space.layout",
        "space.partitions",
        "fs.ntfs",
    ] {
        assert!(
            out.lines().any(|l| l.trim_start().starts_with("ok") && l.contains(id)),
            "{id}: {out}"
        );
    }

    // JSON with stable ids and statuses.
    let o = spaces(&[&["check", "--json"][..], &all].concat());
    let reports: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(reports[0]["verdict"], "healthy");
    assert_eq!(reports[0]["space"]["name"], "data");
    let checks = reports[0]["checks"].as_array().unwrap();
    assert!(checks.iter().any(|c| c["id"] == "fs.ntfs" && c["status"] == "ok"));

    // The deep checks: the mirror's copies compared.
    let o = spaces(&[&["check", "--deep", "--space", "data"][..], &all].concat());
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("deep.scrub") && out.contains("every copy"), "{out}");

    // A bundle for a report.
    let bundle = dir.0.join("bundle.tar.gz");
    let o = spaces(&[&["check", "--bundle", bundle.to_str().unwrap()][..], &all].concat());
    assert!(o.status.success(), "{}", text(&o));
    if let Ok(list) = Command::new("tar").arg("-tzf").arg(&bundle).output() {
        let list = String::from_utf8_lossy(&list.stdout);
        for name in [
            "README.txt",
            "versions.txt",
            "/dump.txt",
            ".txt",
            ".json",
            "-start.bin",
            "-end.bin",
        ] {
            assert!(list.contains(name), "{name}: {list}");
        }
    }

    // A disk away: degraded, exit 1.
    let o = spaces(&[&["check"][..], &all[..2]].concat());
    let out = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(out.lines().next().unwrap().ends_with(": DEGRADED"), "{out}");
    assert!(out.contains("DEGRADED pool.members"), "{out}");

    // The backup GPT header gone: suspect, with where it is kept.
    {
        let files: Vec<File> = disks
            .iter()
            .map(|p| OpenOptions::new().read(true).write(true).open(p).unwrap())
            .collect();
        let pool = Pool::open(files).unwrap();
        let space = pool.find_space("data").unwrap();
        let writer = pool.open_space_rw(space.id()).unwrap();
        writer.write_all_at(&[0u8; 512], writer.size() - 512).unwrap();
        writer.flush().unwrap();
    }
    let o = spaces(&[&["check"][..], &all].concat());
    let out = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(
        out.contains("SUSPECT  space.partitions  the backup GPT header is not valid"),
        "{out}"
    );
    assert!(out.contains("where     LBA 2097151 (space byte 0x3ffffe00"), "{out}");
    assert!(
        out.contains("disk0.img at 0x") || out.contains("disk1.img at 0x") || out.contains("disk2.img at 0x"),
        "{out}"
    );
}

/// Corpus pools (testdata/pools, testdata/crash; skipped when missing): the
/// user's case (wc4k) is healthy now, a crashed one suspect.
#[test]
fn corpus_pools_get_their_verdicts() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata");
    for (dir, verdict, id) in [
        ("pools/wc4k", "HEALTHY", None),
        ("crash/crashparitywc", "SUSPECT", Some("SUSPECT  space.cache")),
    ] {
        let dir = root.join(dir);
        let disks: Vec<String> = (0..)
            .map(|i| dir.join(format!("disk{i}.img")))
            .take_while(|p| p.exists())
            .map(|p| p.display().to_string())
            .collect();
        if disks.is_empty() {
            eprintln!("{} missing, skipped", dir.display());
            continue;
        }
        let all: Vec<&str> = disks.iter().map(String::as_str).collect();
        let o = spaces(&[&["check"][..], &all].concat());
        let out = text(&o);
        assert!(out.lines().next().unwrap().ends_with(verdict), "{out}");
        assert_eq!(o.status.success(), verdict == "HEALTHY", "{out}");
        if let Some(id) = id {
            assert!(out.contains(id), "{out}");
        }
    }
}
