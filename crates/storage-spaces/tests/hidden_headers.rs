//! The first sector of each copy of a hidden structure (write-back cache,
//! parity journal, dirty region log) of every corpus pool zeroed, one copy
//! at a time (testdata/pools; skipped when missing). Where the database
//! lists the structure:
//!
//! * its check is never skipped (in 1.2.1 a zeroed copy 0 made the cache or
//!   journal look absent, the space stayed healthy, and data only the cache
//!   held was gone);
//! * with another copy holding the header, the space is degraded and reads
//!   as before;
//! * with none, it fails, or the pool does not open; suspect when a copy is
//!   on a missing disk; a note when the header is zero on every copy and no
//!   slot is behind it, as Windows leaves a new space's structures before it
//!   writes them (the space reads as before);
//! * writing refuses the space.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use storage_spaces::format::SpaceRole;
use storage_spaces::gpt::Partition;
use storage_spaces::guard::{self, Options};
use storage_spaces::io::{Overlay, ReadAt, WriteAt};
use storage_spaces::layout::Layout;
use storage_spaces::report::{Check, Report, Status, Verdict};
use storage_spaces::{HeaderCopy, HeaderNote, Part, Pool, SpaceReader};

type Dev = Overlay<Arc<File>>;

fn no_fs<D: ReadAt>(_: &SpaceReader<'_, D>, _: &Partition, _: &[String]) -> Vec<Check> {
    Vec::new()
}

fn devices(files: &[Arc<File>]) -> Vec<Dev> {
    files.iter().map(|f| Overlay::new(Arc::clone(f))).collect()
}

fn open(files: &[Arc<File>], zero: Option<(usize, u64)>) -> Option<Pool<Dev>> {
    let devs = devices(files);
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

fn check<'r>(r: &'r Report, id: &str) -> Option<&'r Check> {
    r.checks.iter().find(|c| c.id == id)
}

fn status(r: &Report, id: &str) -> Status {
    check(r, id).map_or(Status::Skipped, |c| c.status)
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

/// Whether user space `user` reads in `pool` as in `base`.
fn reads_alike(base: &Pool<Dev>, pool: &Pool<Dev>, user: u64) -> Result<(), String> {
    let (a, b) = (
        base.open_space(user).unwrap(),
        pool.open_space(user).map_err(|e| e.to_string())?,
    );
    for o in samples(&a) {
        let len = (1usize << 16).min((a.size() - o) as usize);
        let (mut x, mut y) = (vec![0u8; len], vec![0u8; len]);
        a.read_exact_at(&mut x, o).unwrap();
        b.read_exact_at(&mut y, o).map_err(|e| format!("at {o:#x}: {e}"))?;
        if x != y {
            return Err(format!("data differs at {o:#x}"));
        }
    }
    Ok(())
}

/// A copy of a structure: its number, its device, the byte where its first
/// sector is.
type Copy = (u64, usize, u64);

/// A hidden structure of a user space: its check id, its part, the byte of
/// its space where its header is, its copies at hand.
struct Structure {
    id: &'static str,
    part: Part,
    offset: u64,
    copies: Vec<Copy>,
}

/// The hidden structures the database lists for user space `user`.
fn structures(pool: &Pool<Dev>, user: u64) -> Vec<Structure> {
    let mut out = Vec::new();
    for (id, part, role) in [
        ("space.cache", Part::Cache, SpaceRole::Cache),
        ("space.journal", Part::Journal, SpaceRole::Other(0x0a)),
        ("space.drl", Part::DirtyRegions, SpaceRole::Other(0x06)),
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
                out.push(Structure {
                    id,
                    part,
                    offset: base,
                    copies,
                });
            }
        }
    }
    out
}

/// Of the copies of `s` other than `copy`, how many hold a valid header in
/// the undamaged pool: every one, unless its reader has a note on the
/// header that says otherwise. The dirty region log keeps a second header
/// page (8 KiB before its end), which counts as one more.
fn other_valid(notes: &[HeaderNote], s: &Structure, copy: u64) -> usize {
    let copies = match notes.iter().find(|n| n.part == s.part && n.offset == s.offset) {
        Some(n) => n
            .copies
            .iter()
            .filter(|(c, _, state)| *c != copy && *state == HeaderCopy::Valid)
            .count(),
        None => s.copies.len() - 1,
    };
    let second = s.part == Part::DirtyRegions && !notes.iter().any(|n| n.part == s.part && n.offset != s.offset);
    copies + usize::from(second)
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
    let (mut pools, mut listing) = (0, 0);
    let mut total: BTreeMap<&str, u64> = BTreeMap::new();
    for dir in dirs {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let files: Vec<Arc<File>> = (0..)
            .map_while(|i| File::open(dir.join(format!("disk{i}.img"))).ok())
            .map(Arc::new)
            .collect();
        let Some(base) = open(&files, None) else {
            eprintln!("{name}: does not open, skipped");
            continue;
        };
        pools += 1;
        let mut tally: BTreeMap<&str, u64> = BTreeMap::new();
        for user in base.user_spaces().map(|s| s.id()).collect::<Vec<_>>() {
            let before = report(&base, user);
            let notes = base.open_space(user).map_or(Vec::new(), |r| r.header_notes().to_vec());
            for s in structures(&base, user) {
                let id = s.id;
                for &(copy, device, at) in &s.copies {
                    let case = format!("{name}: {id}, copy {copy} of {} zeroed", s.copies.len());
                    let mut sector = [0u8; 512];
                    devices(&files)[device].read_exact_at(&mut sector, at).unwrap();
                    if sector == [0u8; 512] {
                        // Nothing changes.
                        *tally.entry("zero already").or_default() += 1;
                        continue;
                    }
                    let Some(pool) = open(&files, Some((device, at))) else {
                        *tally.entry("pool refused").or_default() += 1;
                        continue;
                    };
                    let after = report(&pool, user);
                    let verdict = after.verdict();
                    let text = after.to_text();
                    assert_ne!(status(&after, id), Status::Skipped, "{case}\n{text}");
                    assert!(
                        !matches!(pool.write_refusal(user), Ok(None)),
                        "{case}: writable\n{text}"
                    );
                    let summary = check(&after, id).map_or("", |c| c.summary.as_str());
                    let outcome = if other_valid(&notes, &s, copy) > 0 {
                        if before.verdict() <= Verdict::Degraded {
                            assert_eq!(verdict, Verdict::Degraded, "{case}\n{text}");
                        }
                        reads_alike(&base, &pool, user).unwrap_or_else(|e| panic!("{case}: {e}\n{text}"));
                        "degraded, reads as before"
                    } else if summary.contains("never written") {
                        assert_eq!(status(&after, id), Status::Info, "{case}\n{text}");
                        reads_alike(&base, &pool, user).unwrap_or_else(|e| panic!("{case}: {e}\n{text}"));
                        "never written (a note), reads as before"
                    } else if summary.contains("not every copy is") {
                        assert_eq!(verdict, Verdict::Suspect, "{case}\n{text}");
                        "suspect, a copy on a missing disk"
                    } else {
                        assert_eq!(verdict, Verdict::Failed, "{case}\n{text}");
                        "failed"
                    };
                    *tally.entry(outcome).or_default() += 1;
                }
            }
        }
        if !tally.is_empty() {
            listing += 1;
            eprintln!("{name}: {tally:?}");
        }
        for (k, v) in tally {
            *total.entry(k).or_default() += v;
        }
    }
    let cases: u64 = total.values().sum();
    eprintln!("{pools} pools, {listing} listing hidden structures: {cases} copies zeroed, {total:?}");
}
