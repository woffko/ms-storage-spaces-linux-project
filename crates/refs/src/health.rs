//! Quick checks of a ReFS volume for the guard of `spaces` and for `refs
//! mount` (see `storage_spaces::report`): `fs.refs.boot`,
//! `fs.refs.superblock`, `fs.refs.checkpoint` and `fs.refs.log`. They read
//! the boot sector, the superblock and its two copies at the end of the
//! volume, both checkpoints and the log's pages, not the container and
//! object tables.

use storage_spaces::io::ReadAt;
use storage_spaces::report::{Check, Evidence, Status, Truth, hex, signature};

use crate::boot::{BOOT_SECTOR_SIZE, BootSector};
use crate::page::V1_BLOCK;
use crate::util::{le32, le64};
use crate::volume::{SUPERBLOCK_LCN, log_state, self_checksum_ok};

/// A superblock or checkpoint as read: where, and whether it checks out.
struct Copy {
    n: u64,
    page: Option<Vec<u8>>,
    valid: bool,
}

/// The quick checks of the ReFS volume at byte `offset` of `dev`; none
/// when there is no ReFS boot sector there. `label` names the volume in
/// summaries ("p2"), `locate` describes a byte offset of `dev` for
/// evidence.
pub fn quick_checks<D: ReadAt + ?Sized>(
    dev: &D,
    offset: u64,
    label: &str,
    locate: &dyn Fn(u64) -> String,
) -> Vec<Check> {
    let mut sector = vec![0u8; BOOT_SECTOR_SIZE];
    if dev.read_exact_at(&mut sector, offset).is_err() || !BootSector::is_refs(&sector) {
        return Vec::new();
    }
    let boot = match BootSector::parse(&sector) {
        Ok(b) => b,
        Err(crate::Error::Unsupported(what)) => {
            return vec![
                Check::new(
                    "fs.refs.boot",
                    Status::Warning,
                    format!("{label}: not read by refs ({what}), so not checked"),
                )
                .advice("refs neither reads nor mounts this volume."),
            ];
        }
        Err(e) => {
            let mut checks = vec![
                Check::new(
                    "fs.refs.boot",
                    Status::Suspect,
                    format!("{label}: the ReFS boot sector does not check out"),
                )
                .evidence(
                    Evidence::new("the boot sector")
                        .at(locate(offset))
                        .expected("its checksum (0x16) and plausible geometry")
                        .found(format!("{e}: {}", hex(&sector[..0x30], 0x30))),
                )
                .truth(Truth::CrossCheck)
                .advice(
                    "Windows reads the volume from its superblock, but a boot sector that does not \
                     check out is a sign of damage or of a misread.",
                ),
            ];
            for id in ["fs.refs.superblock", "fs.refs.checkpoint", "fs.refs.log"] {
                checks.push(Check::skipped(
                    id,
                    format!("{label}: not checked, the boot sector failed"),
                ));
            }
            return checks;
        }
    };
    let mut checks = vec![
        Check::ok(
            "fs.refs.boot",
            format!(
                "{label}: ReFS {}.{}, {} KiB clusters, {:.1} GiB",
                boot.major,
                boot.minor,
                boot.cluster_size() >> 10,
                boot.volume_size() as f64 / (1u64 << 30) as f64
            ),
        )
        .truth(Truth::CrossCheck),
    ];
    let v1 = boot.major == 1;
    // ReFS 1.x names 16 KiB blocks, 3.x clusters.
    let unit = if v1 { V1_BLOCK } else { boot.cluster_size() };
    let units = boot.volume_size() / unit;
    let at = |n: u64| offset + n * unit;
    let read = |n: u64, len: u64| -> Option<Vec<u8>> {
        let mut b = vec![0u8; len as usize];
        dev.read_exact_at(&mut b, at(n)).ok()?;
        Some(b)
    };
    // The superblock's own reference: (offset, length) in the page.
    let self_ref = |p: &[u8]| {
        if v1 {
            (le32(p, 0x58) as usize, le32(p, 0x5c) as usize)
        } else {
            (le32(p, 0x78) as usize, le32(p, 0x7c) as usize)
        }
    };
    let supb_valid = |n: u64, p: &[u8]| {
        let (r, len) = self_ref(p);
        if v1 {
            le64(p, 0) == n && self_checksum_ok(p, r, len, V1_BLOCK as usize, true)
        } else {
            &p[0..4] == b"SUPB" && self_checksum_ok(p, r, len, unit as usize, false)
        }
    };
    let supers: Vec<Copy> = [SUPERBLOCK_LCN, units.wrapping_sub(2), units.wrapping_sub(3)]
        .into_iter()
        .map(|n| {
            let page = if n < units && units > 3 { read(n, unit) } else { None };
            let valid = page.as_deref().is_some_and(|p| supb_valid(n, p));
            Copy { n, page, valid }
        })
        .collect();
    checks.push(superblock_check(label, v1, unit, &supers, &self_ref, &at, locate));
    let Some(supb) = supers.iter().find(|c| c.valid).and_then(|c| c.page.as_deref()) else {
        checks.push(Check::skipped(
            "fs.refs.checkpoint",
            format!("{label}: not checked, no valid superblock"),
        ));
        checks.push(Check::skipped(
            "fs.refs.log",
            format!("{label}: not checked, no valid superblock"),
        ));
        return checks;
    };
    // The checkpoints the superblock lists.
    let (list, count) = if v1 {
        (le32(supb, 0x50) as usize, le32(supb, 0x54) as usize)
    } else {
        (le32(supb, 0x70) as usize, le32(supb, 0x74) as usize)
    };
    let page_size = if v1 { V1_BLOCK } else { unit.max(16384) };
    let points: Vec<Copy> = (0..count.min(2))
        .filter_map(|i| supb.get(list + 8 * i..list + 8 * i + 8).map(|b| le64(b, 0)))
        .map(|n| {
            let page = if n < units { read(n, page_size) } else { None };
            let valid = page.as_deref().is_some_and(|p| {
                if v1 {
                    le64(p, 0) == n
                        && self_checksum_ok(
                            p,
                            le32(p, 0x38) as usize,
                            le32(p, 0x3c) as usize,
                            V1_BLOCK as usize,
                            true,
                        )
                } else {
                    &p[0..4] == b"CHKP"
                        && self_checksum_ok(p, le32(p, 0x58) as usize, le32(p, 0x5c) as usize, unit as usize, false)
                }
            });
            Copy { n, page, valid }
        })
        .collect();
    let clock = |c: &Copy| c.page.as_deref().map_or(0, |p| le64(p, if v1 { 8 } else { 0x60 }));
    let current = points.iter().filter(|c| c.valid).max_by_key(|c| clock(c));
    let what = if v1 { "block" } else { "cluster" };
    let check = match (points.iter().filter(|c| c.valid).count(), current) {
        (2, Some(c)) => Check::ok(
            "fs.refs.checkpoint",
            format!(
                "{label}: both checkpoints valid, the current one (clock {}) at {what} {:#x}",
                clock(c),
                c.n
            ),
        )
        .truth(Truth::CrossCheck),
        (_, Some(c)) => {
            let mut check = Check::new(
                "fs.refs.checkpoint",
                Status::Warning,
                format!(
                    "{label}: one checkpoint is not valid; the other (clock {}, at {what} {:#x}) is read",
                    clock(c),
                    c.n
                ),
            )
            .truth(Truth::CrossCheck)
            .advice(
                "ReFS writes its two checkpoints in turn; one cut off by a crash leaves the other, \
                 which Windows reads as well.",
            );
            for bad in points.iter().filter(|p| !p.valid) {
                check = check.evidence(
                    Evidence::new(format!("the checkpoint at {what} {:#x}", bad.n))
                        .at(locate(at(bad.n)))
                        .expected("a checkpoint whose checksum matches")
                        .found(bad.page.as_deref().map_or("unreadable".into(), |p| signature(&p[..4]))),
                );
            }
            check
        }
        (_, None) => {
            let mut check = Check::new(
                "fs.refs.checkpoint",
                Status::Failed,
                format!("{label}: no valid checkpoint"),
            )
            .truth(Truth::CrossCheck)
            .advice("Nothing of the volume can be read without a checkpoint.");
            for bad in &points {
                check = check.evidence(
                    Evidence::new(format!("the checkpoint at {what} {:#x}", bad.n))
                        .at(locate(at(bad.n)))
                        .expected("a checkpoint whose checksum matches")
                        .found(bad.page.as_deref().map_or("unreadable".into(), |p| signature(&p[..4]))),
                );
            }
            checks.push(check);
            checks.push(Check::skipped(
                "fs.refs.log",
                format!("{label}: not checked, no checkpoint"),
            ));
            return checks;
        }
    };
    checks.push(check);
    let current = current.unwrap();
    checks.push(if v1 {
        Check::skipped("fs.refs.log", format!("{label}: the log of ReFS 1.x is not read"))
    } else {
        match log_state(dev, offset, unit, boot.volume_size(), current.n) {
            Ok(log) if log.needs_replay() => {
                let newest = log.newest.unwrap();
                Check::new(
                    "fs.refs.log",
                    Status::Suspect,
                    format!(
                        "{label}: the log holds changes the checkpoint lacks: what is read is older than \
                         what Windows shows"
                    ),
                )
                .evidence(
                    Evidence::new("the newest record of the log")
                        .at(locate(at(current.n)))
                        .expected(format!(
                            "older than the checkpoint's log sequence number {:#x}:{:#x}",
                            log.checkpoint.high, log.checkpoint.low
                        ))
                        .found(format!("{:#x}:{:#x}", newest.high, newest.low)),
                )
                .truth(Truth::CrossCheck)
                .advice(
                    "The volume was not dismounted when Windows last let go of it (a disk removed \
                     or a computer turned off with it attached; Windows' fast startup leaves \
                     volumes so). Windows replays the log when it next attaches the volume. Start \
                     Windows and shut it down fully (shutdown /s /t 0), then read it here; or read \
                     the older state anyway.",
                )
            }
            Ok(log) => Check::ok(
                "fs.refs.log",
                format!(
                    "{label}: the checkpoint covers the log (log sequence number {:#x}:{:#x})",
                    log.checkpoint.high, log.checkpoint.low
                ),
            )
            .truth(Truth::CrossCheck),
            Err(_) if boot.major == 3 && boot.minor == 1 => {
                Check::skipped("fs.refs.log", format!("{label}: the log of ReFS 3.1 is not read"))
            }
            Err(e) => Check::new(
                "fs.refs.log",
                Status::Warning,
                format!("{label}: the log cannot be read ({e}): whether the checkpoint is current is not known"),
            )
            .truth(Truth::CrossCheck),
        }
    });
    checks
}

/// `fs.refs.superblock`: every copy valid, and the same as the primary
/// apart from where each says it is.
fn superblock_check(
    label: &str,
    v1: bool,
    unit: u64,
    supers: &[Copy],
    self_ref: &dyn Fn(&[u8]) -> (usize, usize),
    at: &dyn Fn(u64) -> u64,
    locate: &dyn Fn(u64) -> String,
) -> Check {
    const ID: &str = "fs.refs.superblock";
    let what = if v1 { "block" } else { "cluster" };
    // What must agree: the page without its own location (the block number
    // of 1.x, the page header's clusters of 3.x) and its own reference.
    let masked = |p: &[u8]| {
        let mut m = p.to_vec();
        if v1 {
            m[0..8].fill(0);
        } else {
            m[0x20..0x40].fill(0);
        }
        let (r, len) = self_ref(p);
        if let Some(d) = m.get_mut(r..r + len) {
            d.fill(0);
        }
        m
    };
    let mut evidence = Vec::new();
    for c in supers {
        if !c.valid {
            let found = match c.page.as_deref() {
                None => "beyond the volume or unreadable".to_string(),
                Some(p) if p.iter().all(|&b| b == 0) => hex(&p[..16], 16),
                Some(p) if v1 => format!("block number {:#x}, or a checksum that does not match", le64(p, 0)),
                Some(p) => format!("{}, or a checksum that does not match", signature(&p[..4])),
            };
            evidence.push(
                Evidence::new(format!("the superblock at {what} {:#x}", c.n))
                    .at(locate(at(c.n)))
                    .expected(if v1 {
                        "its own block number at 0 and a matching checksum".to_string()
                    } else {
                        "\"SUPB\" and a matching checksum".to_string()
                    })
                    .found(found),
            );
        }
    }
    let valid: Vec<&Copy> = supers.iter().filter(|c| c.valid).collect();
    if let Some(first) = valid.first() {
        let reference = masked(first.page.as_deref().unwrap());
        for c in &valid[1..] {
            let m = masked(c.page.as_deref().unwrap());
            if m != reference {
                let i = m.iter().zip(&reference).position(|(a, b)| a != b).unwrap_or(0);
                evidence.push(
                    Evidence::new(format!("the superblock at {what} {:#x}", c.n))
                        .at(locate(at(c.n) + i as u64))
                        .expected(format!(
                            "the same as at {what} {:#x}: {} at byte {i:#x}",
                            first.n,
                            hex(&reference[i..(i + 16).min(reference.len())], 16)
                        ))
                        .found(hex(&m[i..(i + 16).min(m.len())], 16)),
                );
            }
        }
    }
    let _ = unit;
    if evidence.is_empty() {
        return Check::ok(
            ID,
            format!("{label}: the superblock and both copies are valid and agree"),
        )
        .truth(Truth::CrossCheck);
    }
    let (status, advice) = if valid.is_empty() {
        (
            Status::Failed,
            "Nothing of the volume can be read without a superblock.",
        )
    } else {
        (
            Status::Suspect,
            "ReFS keeps three copies of its superblock, and they disagree here (or one is not valid). \
             If Windows shows this volume as healthy, part of it is probably read wrongly: please \
             report it.",
        )
    };
    let mut check = Check::new(
        ID,
        status,
        format!("{label}: the superblock and its copies do not all check out"),
    )
    .truth(Truth::CrossCheck)
    .advice(advice);
    for e in evidence {
        check = check.evidence(e);
    }
    check
}
