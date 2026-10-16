//! The first sector of each copy of a hidden structure (write-back cache,
//! parity journal, dirty region log) of every corpus pool zeroed, one copy
//! at a time: the structure is read through another copy and the space
//! degraded, reading as before; or the space is refused. It never vanishes:
//! in 1.2.1 a zeroed copy 0 made the cache or journal look absent, the
//! space stayed healthy, and data only the cache held was gone
//! (testdata/pools; skipped when missing).

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use storage_spaces::format::SpaceRole;
use storage_spaces::gpt::Partition;
use storage_spaces::guard::{self, Options};
use storage_spaces::io::{Overlay, ReadAt, WriteAt};
use storage_spaces::layout::Layout;
use storage_spaces::report::{Check, Report, Status, Verdict};
use storage_spaces::{Pool, SpaceReader};

type Dev = Overlay<Arc<File>>;

fn no_fs<D: ReadAt>(_: &SpaceReader<'_, D>, _: &Partition, _: &[String]) -> Vec<Check> {
    Vec::new()
}

fn open(files: &[Arc<File>], zero: Option<(usize, u64)>) -> Option<Pool<Dev>> {
    let devs: Vec<Dev> = files.iter().map(|f| Overlay::new(Arc::clone(f))).collect();
    if let Some((device, at)) = zero {
        devs[device].write_all_at(&[0u8; 512], at).unwrap();
    }
    Pool::open(devs).ok()
}

fn report(pool: &Pool<Dev>, id: u64) -> Report {
    let options = Options {
        journal_budget: Some(16 << 20),
    };
    guard::space_report(pool, &pool.spaces[&id], &[], &options, &mut no_fs)
}

fn status(r: &Report, id: &str) -> Status {
    r.checks
        .iter()
        .find(|c| c.id == id)
        .map_or(Status::Skipped, |c| c.status)
}

/// Where the space reads: the chunks its cache holds and 64 places across.
fn samples(r: &SpaceReader<'_, Dev>) -> Vec<u64> {
    let size = r.size();
    let mut at: Vec<u64> = r
        .cache()
        .map_or(Vec::new(), |c| c.mappings().iter().map(|m| m.0).collect());
    let step = (size / 64).max(1 << 16) & !0xffff;
    at.extend((0..64).map(|i| i * step).filter(|&o| o + (1 << 16) <= size));
    at
}

/// A copy of a structure: its number, its device, the byte where its first
/// sector is.
type Copy = (u64, usize, u64);

/// The hidden structures of user space `user`: (check id, the space holding
/// it, its copies).
fn structures(pool: &Pool<Dev>, user: u64) -> Vec<(&'static str, u64, Vec<Copy>)> {
    let mut out = Vec::new();
    for (id, role) in [
        ("space.cache", SpaceRole::Cache),
        ("space.journal", SpaceRole::Other(0x0a)),
        ("space.drl", SpaceRole::Other(0x06)),
    ] {
        for container in pool.children(user).filter(|c| c.info.role == role) {
            for child in pool.children(container.id()).filter(|c| !c.extents.is_empty()) {
                let Some(policy) = child.info.policy else { continue };
                let base = child.info.range.map_or(0, |(start, _)| start);
                let Ok(l) = Layout::with_base(&policy, &child.extents, base) else {
                    continue;
                };
                let loc = l.locate(base);
                let copies = l
                    .copies_of(loc.column)
                    .into_iter()
                    .filter_map(|copy| {
                        let (disk, slab) = l.physical(loc.column, copy, loc.row)?;
                        let (device, start) = pool.slab_location(disk, slab).ok()??;
                        Some((copy, device, start + loc.offset_in_slab))
                    })
                    .collect();
                out.push((id, child.id(), copies));
            }
        }
    }
    out
}

#[test]
fn zeroed_headers_of_hidden_structures_never_make_them_vanish() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/pools");
    let Ok(entries) = std::fs::read_dir(&root) else {
        eprintln!("{} missing, skipped", root.display());
        return;
    };
    let mut dirs: Vec<_> = entries.map(|e| e.unwrap().path()).collect();
    dirs.sort();
    let mut cases = 0;
    for dir in dirs {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let files: Vec<Arc<File>> = (0..)
            .map_while(|i| File::open(dir.join(format!("disk{i}.img"))).ok())
            .map(Arc::new)
            .collect();
        let Some(base) = open(&files, None) else { continue };
        for user in base.user_spaces().map(|s| s.id()).collect::<Vec<_>>() {
            let before = report(&base, user);
            for (id, _, copies) in structures(&base, user) {
                for &(copy, device, at) in &copies {
                    let case = format!("{name}: {id}, copy {copy} of {} zeroed", copies.len());
                    cases += 1;
                    let Some(pool) = open(&files, Some((device, at))) else {
                        continue; // refused whole
                    };
                    let after = report(&pool, user);
                    let verdict = after.verdict();
                    // Never gone unseen: absent only if it was before.
                    if status(&after, id) == Status::Skipped {
                        assert_eq!(status(&before, id), Status::Skipped, "{case}\n{}", after.to_text());
                    }
                    // Damage is seen, but in the one copy of a header whose
                    // structure holds nothing yet: that is a header never
                    // written (as Windows leaves new spaces), and reads as
                    // before.
                    let unwritten = after
                        .checks
                        .iter()
                        .any(|c| c.id == id && c.status == Status::Info && c.summary.contains("never written"));
                    if unwritten {
                        assert_eq!(copies.len(), 1, "{case}\n{}", after.to_text());
                    } else if before.verdict() == Verdict::Healthy {
                        assert!(verdict >= Verdict::Degraded, "{case}\n{}", after.to_text());
                    }
                    if verdict <= Verdict::Degraded {
                        let (a, b) = (base.open_space(user).unwrap(), pool.open_space(user).unwrap());
                        for o in samples(&a) {
                            let len = (1usize << 16).min((a.size() - o) as usize);
                            let (mut x, mut y) = (vec![0u8; len], vec![0u8; len]);
                            a.read_exact_at(&mut x, o).unwrap();
                            b.read_exact_at(&mut y, o).unwrap();
                            assert!(x == y, "{case}: data differs at {o:#x}\n{}", after.to_text());
                        }
                    }
                }
            }
        }
    }
    eprintln!("{cases} copies zeroed");
}
