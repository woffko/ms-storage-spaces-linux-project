//! The guard in refs: `refs check --quick` reports, the reading commands
//! warn, and mount and the writing commands refuse a volume that is not
//! healthy; on a raw image made from a fixture volume, as Windows left it
//! and with a log record newer than its checkpoint.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use storage_spaces::io::{ReadAt, SparseImage};

fn refs(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_refs")).args(args).output().unwrap()
}

fn text(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read(image: &SparseImage, at: u64, len: usize) -> Vec<u8> {
    let mut b = vec![0u8; len];
    image.read_exact_at(&mut b, at).unwrap();
    b
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// The fixture `name` as a raw (sparse) image file, and the volume's
/// offset in it.
fn raw(name: &str, dir: &Path, change: impl Fn(&mut SparseImage, u64)) -> (PathBuf, u64) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../refs/tests/fixtures")
        .join(name);
    let mut image = SparseImage::read_from(File::open(fixture.join("disk.fixture")).unwrap()).unwrap();
    let manifest: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(fixture.join("manifest.json"))
            .unwrap()
            .trim_start_matches('\u{feff}'),
    )
    .unwrap();
    let offset = manifest["partition_offset"].as_u64().unwrap();
    change(&mut image, offset);
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{name}.img"));
    let file = File::create(&path).unwrap();
    file.set_len(image.size).unwrap();
    for (at, len) in image.ranges() {
        file.write_all_at(&read(&image, at, len), at).unwrap();
    }
    (path, offset)
}

/// Gives the first record page of the log a log sequence number newer
/// than the current checkpoint's, as a volume Windows let go of without
/// dismounting it holds.
fn newer_log(image: &mut SparseImage, offset: u64) {
    let vol = refs::Volume::open(&*image, offset).unwrap();
    let lsn = le64(&read(image, offset + vol.checkpoint.lcn * 4096, 4096), 0x70);
    drop(vol);
    let control = (0..0x400u64)
        .map(|lcn| read(image, offset + lcn * 4096, 4096))
        .find(|c| &c[0..4] == b"MLog" && le64(c, 0x28) == 0)
        .unwrap();
    let start = le64(&control, 0xb8);
    let mut record = read(image, offset + start * 4096, 4096);
    record[0..4].copy_from_slice(b"MLog");
    record[4..8].copy_from_slice(&control[4..8]);
    record[0x20..0x28].copy_from_slice(&control[0x20..0x28]);
    record[0x28..0x30].copy_from_slice(&(lsn + 1).to_le_bytes());
    image.insert(offset + start * 4096, &record);
}

#[test]
fn a_volume_that_is_not_healthy_is_read_with_a_warning_and_not_mounted_or_written() {
    let dir = Scratch(std::env::temp_dir().join(format!("refs-guard-{}", std::process::id())));
    // The fixture holds the volume, not the disk's partition table.
    let (good, offset) = raw("r314small", &dir.0.join("good"), |_, _| {});
    let good = good.to_str().unwrap();
    let at = offset.to_string();
    let at = at.as_str();
    let o = refs(&["check", "--quick", "--offset", at, good]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.starts_with(&format!("the ReFS volume on {good}: HEALTHY")), "{out}");
    for id in [
        "fs.refs.boot",
        "fs.refs.superblock",
        "fs.refs.checkpoint",
        "fs.refs.log",
    ] {
        assert!(
            out.lines().any(|l| l.trim_start().starts_with("ok") && l.contains(id)),
            "{id}: {out}"
        );
    }
    let o = refs(&["ls", "--offset", at, good]);
    assert!(o.status.success() && o.stderr.is_empty(), "{}", text(&o));

    let (bad, _) = raw("r314small", &dir.0.join("bad"), newer_log);
    let bad = bad.to_str().unwrap();
    let o = refs(&["check", "--quick", "--offset", at, bad]);
    let out = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(out.contains("SUSPECT  fs.refs.log"), "{out}");
    assert!(out.contains("what      the newest record of the log"), "{out}");
    // Read, with a warning.
    let o = refs(&["ls", "--offset", at, bad]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(
        String::from_utf8_lossy(&o.stderr).starts_with("warning: SUSPECT: fs.refs.log:"),
        "{}",
        text(&o)
    );
    // Not mounted without --force (refused before anything is mounted).
    let mnt = dir.0.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    let o = refs(&["mount", "--offset", at, bad, mnt.to_str().unwrap()]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(
        out.contains("SUSPECT: fs.refs.log:") && out.contains("-o force"),
        "{out}"
    );
    // Not written.
    let o = refs(&["mkdir", "--offset", at, bad, "--path", "/guarded", "--yes"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(out.contains("writing needs a healthy volume"), "{out}");
    let o = refs(&["ls", "--offset", at, bad, "--path", "/"]);
    assert!(!String::from_utf8_lossy(&o.stdout).contains("guarded"));
}

/// mount.ReFS passes "-o force" on as --force, and then never --rw.
#[test]
fn mount_refs_passes_force_on() {
    // As root it would start a systemd unit.
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    if status
        .lines()
        .any(|l| l.starts_with("Uid:") && l.split_whitespace().nth(2) == Some("0"))
    {
        eprintln!("run as root: skipped");
        return;
    }
    let dir = Scratch(std::env::temp_dir().join(format!("refs-mount-force-{}", std::process::id())));
    std::fs::create_dir_all(dir.0.join("mnt")).unwrap();
    let fake = dir.0.join("refs");
    // Records its arguments and reports a failure at once.
    std::fs::write(
        &fake,
        "#!/bin/sh\necho \"$@\" > \"$ARGS_OUT\"\nwhile [ $# -gt 0 ]; do case $1 in --status-file) s=$2; shift 2 ;; *) shift ;; esac; done\nprintf 'state=failed\\nerror=fake\\n' > \"$s.tmp\" && mv \"$s.tmp\" \"$s\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let helper = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contrib/mount.refs");
    let args_out = dir.0.join("args");
    for (options, has, lacks) in [
        ("force", "--force", "--rw"),
        ("rw,refs.rw", "--rw", "--force"),
        ("rw,refs.rw,force", "--force", "--rw"),
    ] {
        let o = Command::new("sh")
            .arg(&helper)
            .arg("/dev/null")
            .arg(dir.0.join("mnt"))
            .args(["-o", options])
            .env("REFS", &fake)
            .env("ARGS_OUT", &args_out)
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(32), "{options}: {}", text(&o));
        let args = std::fs::read_to_string(&args_out).unwrap();
        assert!(args.contains(has) && !args.contains(lacks), "{options}: {args}");
    }
}
