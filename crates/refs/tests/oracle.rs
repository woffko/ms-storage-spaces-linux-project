//! Oracles over the corpus pools (testdata/pools; skipped when missing),
//! with the guard and the file system checks of `spaces attach`:
//!
//! * damage: one place of the bytes the library reads to open a pool and
//!   its spaces changed in memory (a bit, a byte, a sector zeroed, of junk,
//!   or copied from elsewhere): nothing panics, opening the pool, the
//!   guard's checks and opening the spaces take 3 s at most (a mutant over
//!   that is timed again at once, and is slow if it is over it again: a
//!   pause of the machine does not come back), the check of a
//!   write-back cache, parity journal or dirty region log the database
//!   lists is never skipped, and a space the guard calls healthy reads as
//!   before. A pool with fewer than three copies of its database has a
//!   damaged copy nothing outvotes: there such cases are counted, not
//!   failed. Other checks the undamaged pool makes and a mutant's report
//!   skips are counted;
//! * user: the same with what the guard reads of the spaces' contents
//!   (partition tables, boot sectors, ReFS metadata): panics, slow cases
//!   and skipped checks of listed structures count;
//! * missing disks: each disk away, and each pair of a pool of at most 8
//!   disks: no space healthy, and a space the guard lets be read reads as
//!   with every disk.
//!
//! The test run takes a few mutants of a fixed list of pools; a campaign
//! sets ORACLE_MUTANTS (per pool and mode), ORACLE_POOLS ("all" or names,
//! comma-separated), ORACLE_MODES ("damage" or "user"; both without it),
//! ORACLE_SEED, ORACLE_ONLY (the number of one mutant, to replay it alone)
//! and ORACLE_SAMPLES (reads per space and case of the missing-disk
//! oracle). Ported from the oracle of the code audit of 2026-10-16.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use storage_spaces::format::SpaceRole;
use storage_spaces::gpt::Partition;
use storage_spaces::guard::{self, Options};
use storage_spaces::io::{Overlay, ReadAt, Recording, WriteAt};
use storage_spaces::isolate::caught;
use storage_spaces::report::{Check, Report, Status, Verdict};
use storage_spaces::{Pool, SpaceReader};

type Dev = Overlay<Arc<File>>;

/// Single disk, two disks, mirror of five, parity, dual parity, tiers, a
/// write-back cache, 4 KiB sectors.
const POOLS: [&str; 8] = [
    "simple1c", "mirror2", "mirror3", "parity3", "dual7", "tiered", "wc4k", "m4kn",
];
/// Reads compared per mutant: on a grid of this step (up to 8 GiB).
const STEP: u64 = 256 << 10;
const GRID: u64 = 8 << 30;

fn slow() -> Duration {
    // Unoptimized builds take several times as long.
    Duration::from_secs(if cfg!(debug_assertions) { 30 } else { 3 })
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// The file system checks attach makes (`spaces check` without --deep).
fn hook<D: ReadAt>(r: &SpaceReader<'_, D>, p: &Partition, names: &[String]) -> Vec<Check> {
    let label = format!("p{}", p.number);
    let locate = |o: u64| r.describe(o, names);
    if let Some(c) = guard::ntfs_check(r, p.offset, p.length, &label, &locate) {
        return vec![c];
    }
    refs::health::quick_checks_in(r, p.offset, Some(p.length), &label, &locate)
}

fn options() -> Options {
    Options {
        journal_budget: Some(4 << 20),
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}

/// The images of corpus pool `name`, if they are at hand.
fn images(name: &str) -> Option<Vec<Arc<File>>> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/pools")
        .join(name);
    let files: Vec<Arc<File>> = (0..)
        .map_while(|i| File::open(dir.join(format!("disk{i}.img"))).ok())
        .map(Arc::new)
        .collect();
    (!files.is_empty()).then_some(files)
}

/// The pools to take: ORACLE_POOLS, else the fixed list.
fn pools() -> Vec<String> {
    match std::env::var("ORACLE_POOLS").ok().as_deref() {
        Some("all") => {
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/pools");
            let mut all: Vec<String> = std::fs::read_dir(root)
                .into_iter()
                .flatten()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            all.sort();
            all
        }
        Some(list) => list.split(',').map(str::to_owned).collect(),
        None => POOLS.iter().map(|&p| p.to_owned()).collect(),
    }
}

/// The modes to run (`user` false: damage): ORACLE_MODES, else both.
fn modes() -> Vec<bool> {
    match std::env::var("ORACLE_MODES").ok().as_deref() {
        Some("damage") => vec![false],
        Some("user") => vec![true],
        _ => vec![false, true],
    }
}

/// Per check id of a report: how many checks it holds of it, and how many
/// of those are skipped for want of what they check (not those skipped
/// because another check failed).
fn skips(r: &Report) -> BTreeMap<String, (usize, usize)> {
    let mut out = BTreeMap::new();
    for c in &r.checks {
        let e: &mut (usize, usize) = out.entry(c.id.clone()).or_default();
        e.0 += 1;
        if c.status == Status::Skipped && !c.summary.starts_with("not checked:") {
            e.1 += 1;
        }
    }
    out
}

/// Whether the database of `pool` lists the hidden structure that check
/// `id` of space `space` is about (a container of its role with a space
/// that has extents); `None` for the other checks.
fn listed<D: ReadAt>(pool: &Pool<D>, space: u64, id: &str) -> Option<bool> {
    let role = match id {
        "space.cache" => SpaceRole::Cache,
        "space.journal" => SpaceRole::Other(0x0a),
        "space.drl" => SpaceRole::Other(0x06),
        _ => return None,
    };
    Some(
        pool.children(space)
            .filter(|c| c.info.role == role)
            .any(|c| pool.children(c.id()).any(|s| !s.extents.is_empty())),
    )
}

fn devices(files: &[Arc<File>], skip: &[usize]) -> Vec<Dev> {
    files
        .iter()
        .enumerate()
        .filter(|(i, _)| !skip.contains(i))
        .map(|(_, f)| Overlay::new(Arc::clone(f)))
        .collect()
}

/// A byte range a mutation may hit.
struct Region {
    dev: usize,
    start: u64,
    len: u64,
}

/// The ranges the library reads of each device: to open the pool and its
/// spaces (`user` false), or then for the guard's checks (`user` true),
/// merged.
fn regions(files: &[Arc<File>], user: bool) -> Vec<Region> {
    let recorders: Vec<Arc<Recording<Arc<File>>>> =
        files.iter().map(|f| Arc::new(Recording::new(Arc::clone(f)))).collect();
    let pool = Pool::open(recorders.clone()).unwrap();
    for s in pool.user_spaces() {
        let _ = pool.open_space(s.id());
    }
    let opened: Vec<usize> = recorders.iter().map(|r| r.reads().len()).collect();
    for s in pool.user_spaces() {
        let _ = guard::space_report(&pool, s, &[], &options(), &mut hook);
    }
    let mut out = Vec::new();
    for (dev, r) in recorders.iter().enumerate() {
        let reads = r.reads();
        let part = if user {
            &reads[opened[dev]..]
        } else {
            &reads[..opened[dev]]
        };
        let mut ranges: Vec<(u64, u64)> = part.iter().map(|&(o, n)| (o, o + n as u64)).collect();
        ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (a, b) in ranges {
            match merged.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        out.extend(merged.into_iter().map(|(a, b)| Region {
            dev,
            start: a,
            len: b - a,
        }));
    }
    out
}

/// Changes one place of one of `regions` on `devs`; what it did.
fn mutate(devs: &[Dev], regions: &[Region], rng: &mut Rng) -> String {
    let r = &regions[rng.below(regions.len() as u64) as usize];
    let read = |d: usize, at: u64, n: usize| {
        let mut b = vec![0u8; n];
        let _ = devs[d].read_exact_at(&mut b, at);
        b
    };
    let write = |d: usize, at: u64, b: &[u8]| {
        let _ = devs[d].write_all_at(b, at);
    };
    // Mostly where there is something to damage.
    let mut at = r.start + rng.below(r.len.max(1));
    for _ in 0..64 {
        if read(r.dev, at, 1)[0] != 0 || rng.below(4) == 0 {
            break;
        }
        at = r.start + rng.below(r.len.max(1));
    }
    let (dev, sector) = (r.dev, at & !511);
    match rng.below(100) {
        0..35 => {
            let mut b = read(dev, at, 1);
            b[0] ^= 1 << rng.below(8);
            write(dev, at, &b);
            format!("a bit of dev{dev}+{at:#x}")
        }
        35..55 => {
            let v = rng.next() as u8;
            write(dev, at, &[v]);
            format!("byte {v:#x} at dev{dev}+{at:#x}")
        }
        55..65 => {
            write(dev, sector, &[0u8; 512]);
            format!("dev{dev}+{sector:#x} zeroed")
        }
        65..75 => {
            let junk: Vec<u8> = (0..512).map(|_| rng.next() as u8).collect();
            write(dev, sector, &junk);
            format!("junk at dev{dev}+{sector:#x}")
        }
        75..88 => {
            let from = r.start + (rng.below(r.len.max(1)) & !511);
            write(dev, sector, &read(dev, from, 512));
            format!("dev{dev}+{from:#x} copied to {sector:#x}")
        }
        _ if devs.len() > 1 => {
            let other = (dev + 1 + rng.below(devs.len() as u64 - 1) as usize) % devs.len();
            let page = at & !4095;
            write(dev, page, &read(other, page, 4096));
            format!("dev{other}+{page:#x} copied to dev{dev}")
        }
        _ => {
            write(dev, sector, &[0xffu8; 512]);
            format!("ones at dev{dev}+{sector:#x}")
        }
    }
}

/// A space of the undamaged pool: what it reads.
struct Base<'p> {
    id: u64,
    size: u64,
    reader: SpaceReader<'p, Dev>,
    /// Where each grid point is read from.
    places: Vec<String>,
    /// What the guard's report on it skips ([`skips`]).
    skips: BTreeMap<String, (usize, usize)>,
}

/// What became of the mutants of one pool and mode.
#[derive(Default, Debug)]
struct Tally {
    mutants: u64,
    unopened: u64,
    refused: BTreeMap<String, u64>,
    healthy: u64,
    /// Healthy, but reads differ (with what was done).
    wrong: Vec<String>,
    /// Healthy, but reads fail.
    unreadable: Vec<String>,
    /// Over the time limit once, but not when timed again at once (the
    /// machine paused it: a mutant that makes the library slow is slow
    /// again).
    paused: Vec<String>,
    /// A check the report on the undamaged pool makes, skipped (whatever
    /// the verdict), and whether the database still lists the hidden
    /// structure it checks.
    skipped: Vec<(String, bool)>,
    panics: Vec<String>,
    slow: Vec<String>,
}

/// One mutant: the pool opened on damaged devices, the guard asked about
/// each space, the healthy ones read where they read differently. The time
/// the library took (opening, the guard, opening the spaces; not the
/// comparison, which reads a space whole where its layout reads otherwise).
fn check_mutant(devs: Vec<Dev>, bases: &[Base<'_>], user: bool, rng: &mut Rng, t: &mut Tally, what: &str) -> Duration {
    let started = Instant::now();
    let opened = Pool::open(devs);
    let mut spent = started.elapsed();
    let Ok(pool) = opened else {
        t.unopened += 1;
        return spent;
    };
    for b in bases {
        let Some(space) = pool.spaces.get(&b.id) else {
            *t.refused.entry("space gone".into()).or_default() += 1;
            continue;
        };
        let started = Instant::now();
        let report = guard::space_report(&pool, space, &[], &options(), &mut hook);
        spent += started.elapsed();
        let now = skips(&report);
        for (id, &(n, s)) in &b.skips {
            if n > s && now.get(id).is_some_and(|&(_, m)| m > s) {
                t.skipped.push((
                    format!("{what}: space {} {id} skipped ({})", b.id, report.verdict().as_str()),
                    listed(&pool, b.id, id) == Some(true),
                ));
            }
        }
        if report.verdict() != Verdict::Healthy {
            let id = report.problems().first().map_or("?".into(), |c| c.id.to_string());
            *t.refused.entry(id).or_default() += 1;
            continue;
        }
        t.healthy += 1;
        if user {
            continue;
        }
        let started = Instant::now();
        let opened = pool.open_space(b.id);
        spent += started.elapsed();
        let reader = match opened {
            Ok(r) if r.size() == b.size => r,
            Ok(r) => {
                t.wrong.push(format!("{what}: size {} for {}", r.size(), b.size));
                continue;
            }
            Err(e) => {
                t.unreadable.push(format!("{what}: {e}"));
                continue;
            }
        };
        let points: Vec<u64> = (0..b.size.min(GRID)).step_by(STEP as usize).collect();
        if points.is_empty() {
            continue;
        }
        let mut look: Vec<usize> = (0..points.len())
            .filter(|&i| reader.describe(points[i], &[]) != b.places[i])
            .collect();
        look.extend((0..points.len().min(4)).flat_map(|i| [i, points.len() - 1 - i]));
        look.extend((0..32).map(|_| rng.below(points.len() as u64) as usize));
        look.sort_unstable();
        look.dedup();
        for i in look {
            let at = points[i];
            let n = STEP.min(b.size - at) as usize;
            let (mut x, mut y) = (vec![0u8; n], vec![0u8; n]);
            match (b.reader.read_exact_at(&mut x, at), reader.read_exact_at(&mut y, at)) {
                (Ok(()), Ok(())) if x != y => {
                    let k = x.iter().zip(&y).position(|(p, q)| p != q).unwrap();
                    t.wrong.push(format!(
                        "{what}: space {} reads otherwise at {:#x}",
                        b.id,
                        at + k as u64
                    ));
                    break;
                }
                (Ok(()), Err(e)) => {
                    t.unreadable.push(format!("{what}: space {} at {at:#x}: {e}", b.id));
                    break;
                }
                _ => {}
            }
        }
    }
    spent
}

/// The damage oracle on pool `name` (with `user`, of what the guard reads
/// of the spaces' contents); None when the pool is not at hand.
fn damage(name: &str, user: bool, mutants: u64, seed: u64) -> Option<(Tally, usize)> {
    let files = images(name)?;
    let pool = Pool::open(devices(&files, &[])).ok()?;
    let copies = pool.members.iter().filter(|m| m.header.database_copy).count();
    let bases: Vec<Base<'_>> = pool
        .user_spaces()
        .filter_map(|s| {
            let report = guard::space_report(&pool, s, &[], &options(), &mut hook);
            if !user && report.verdict() != Verdict::Healthy {
                return None;
            }
            let reader = pool.open_space(s.id()).ok()?;
            let size = reader.size();
            let places = (0..size.min(GRID))
                .step_by(STEP as usize)
                .map(|o| reader.describe(o, &[]))
                .collect();
            Some(Base {
                id: s.id(),
                size,
                reader,
                places,
                skips: skips(&report),
            })
        })
        .collect();
    let regions = regions(&files, user);
    let mut t = Tally::default();
    if regions.is_empty() {
        return Some((t, copies));
    }
    let only = std::env::var("ORACLE_ONLY").ok().and_then(|v| v.parse::<u64>().ok());
    for i in (0..mutants).filter(|&i| only.is_none_or(|o| o == i)) {
        // Mutant `i`: its devices, what was done to them, and the random
        // numbers its checks go on with.
        let make = || {
            let mut rng = Rng((seed ^ (i + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15)) | 1);
            for _ in 0..4 {
                rng.next();
            }
            let devs = devices(&files, &[]);
            let what = format!(
                "{name} {} #{i}: {}",
                if user { "user" } else { "damage" },
                mutate(&devs, &regions, &mut rng)
            );
            (devs, what, rng)
        };
        let (devs, what, mut rng) = make();
        let out = caught(|| check_mutant(devs, &bases, user, &mut rng, &mut t, &what));
        t.mutants += 1;
        match out {
            Err(stopped) => t.panics.push(format!("{what}: {stopped}")),
            Ok(took) if took > slow() => {
                // Timed again at once, the same mutant: a pause of the
                // machine does not come back, work that takes long does.
                let (devs, _, mut rng) = make();
                let again = caught(|| check_mutant(devs, &bases, user, &mut rng, &mut Tally::default(), &what));
                match again {
                    Ok(then) if then <= slow() => t.paused.push(format!("{what}: {took:?}, then {then:?}")),
                    Ok(then) => t.slow.push(format!("{what}: {took:?}, then {then:?}")),
                    Err(stopped) => t.panics.push(format!("{what}: {stopped} (timed again)")),
                }
            }
            Ok(_) => {}
        }
    }
    Some((t, copies))
}

fn report(name: &str, mode: &str, t: &Tally, copies: usize) {
    eprintln!(
        "{name} {mode}: {} mutants ({copies} database copies): {} not opened, {} healthy, refused {:?}; \
         {} wrong, {} unreadable, {} skipped ({} of a listed structure), {} panics, {} slow \
         ({} more over the limit once, not again)",
        t.mutants,
        t.unopened,
        t.healthy,
        t.refused,
        t.wrong.len(),
        t.unreadable.len(),
        t.skipped.len(),
        t.skipped.iter().filter(|(_, listed)| *listed).count(),
        t.panics.len(),
        t.slow.len(),
        t.paused.len()
    );
    for f in t
        .wrong
        .iter()
        .chain(&t.unreadable)
        .chain(t.skipped.iter().map(|(f, _)| f))
        .chain(&t.panics)
        .chain(&t.slow)
        .chain(&t.paused)
        .take(20)
    {
        eprintln!("  {f}");
    }
}

/// The damage oracle and its user mode over the pools.
#[test]
fn damage_where_the_guard_calls_a_space_healthy_does_not_change_it() {
    let (mutants, seed) = (env("ORACLE_MUTANTS", 6), env("ORACLE_SEED", 1));
    let mut failures = Vec::new();
    for name in pools() {
        for user in modes() {
            let mode = if user { "user" } else { "damage" };
            let Some((t, copies)) = damage(&name, user, mutants, seed) else {
                continue;
            };
            report(&name, mode, &t, copies);
            failures.extend(t.panics.iter().chain(&t.slow).cloned());
            // A damaged copy outvoted by two others is seen; with fewer
            // copies a change may be the one version there is.
            if !user && copies >= 3 {
                failures.extend(t.wrong.iter().cloned());
            }
            failures.extend(t.skipped.iter().filter(|(_, listed)| *listed).map(|(f, _)| f.clone()));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// The missing-disk oracle over the pools.
#[test]
fn a_pool_without_a_disk_is_never_healthy_and_reads_as_whole() {
    let samples = env("ORACLE_SAMPLES", 16);
    let mut failures = Vec::new();
    for name in pools() {
        let Some(files) = images(&name) else { continue };
        let Ok(full) = Pool::open(devices(&files, &[])) else {
            continue;
        };
        let healthy: Vec<u64> = full
            .user_spaces()
            .filter(|s| guard::space_report(&full, s, &[], &options(), &mut hook).verdict() == Verdict::Healthy)
            .map(|s| s.id())
            .collect();
        let n = files.len();
        let mut cases: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();
        if n <= 8 {
            cases.extend((0..n).flat_map(|i| (i + 1..n).map(move |j| vec![i, j])));
        }
        let mut seen = BTreeMap::<String, u64>::new();
        for skip in &cases {
            let out = caught(|| {
                let mut found = Vec::new();
                let Ok(pool) = Pool::open(devices(&files, skip)) else {
                    return (vec!["not opened".to_string()], found);
                };
                let mut verdicts = Vec::new();
                for &id in &healthy {
                    let Some(space) = pool.spaces.get(&id) else { continue };
                    let verdict = guard::space_report(&pool, space, &[], &options(), &mut hook).verdict();
                    verdicts.push(verdict.as_str().to_string());
                    if verdict == Verdict::Healthy {
                        found.push(format!("{name} without disks {skip:?}: space {id} healthy"));
                    }
                    if verdict > Verdict::Degraded {
                        continue;
                    }
                    let (Ok(a), Ok(b)) = (full.open_space(id), pool.open_space(id)) else {
                        found.push(format!(
                            "{name} without disks {skip:?}: space {id} {} but does not open",
                            verdict.as_str()
                        ));
                        continue;
                    };
                    let size = a.size();
                    let step = (size / samples.max(1)).max(1 << 16) & !0xffff;
                    for at in (0..samples).map(|k| k * step).filter(|&o| o + (1 << 16) <= size) {
                        let (mut x, mut y) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
                        if a.read_exact_at(&mut x, at).is_err() {
                            continue;
                        }
                        match b.read_exact_at(&mut y, at) {
                            Ok(()) if x == y => {}
                            Ok(()) => {
                                found.push(format!(
                                    "{name} without disks {skip:?}: space {id} reads otherwise at {at:#x}"
                                ));
                                break;
                            }
                            Err(e) => {
                                found.push(format!(
                                    "{name} without disks {skip:?}: space {id} {} but fails at {at:#x}: {e}",
                                    verdict.as_str()
                                ));
                                break;
                            }
                        }
                    }
                }
                (verdicts, found)
            });
            match out {
                Ok((verdicts, found)) => {
                    for v in verdicts {
                        *seen.entry(v).or_default() += 1;
                    }
                    failures.extend(found);
                }
                Err(stopped) => failures.push(format!("{name} without disks {skip:?}: {stopped}")),
            }
        }
        eprintln!("{name}: {} cases, verdicts {seen:?}", cases.len());
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
