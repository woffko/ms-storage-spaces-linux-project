//! The guard: what a space must pass before it is attached or read without
//! being asked ([`Verdict::Healthy`](crate::report::Verdict)). Each check
//! looks at one thing Windows recorded (the pool database, the space's
//! logs) or one cross-check (copies that must agree, parity that must
//! match), and says what it found and where; the file systems in a space
//! are checked through a hook. The checks are quick: a few reads per space
//! and partition, and a bounded read of the stripes a parity journal does
//! not record as consistent.
//!
//! Check ids: `pool.quorum`, `pool.members`, `pool.database`,
//! `pool.clean`, `space.state`, `space.layout`, `space.cache`,
//! `space.journal`, `space.drl`, and `fs.*` from the hook.

use std::collections::BTreeSet;

use crate::Pool;
use crate::gpt::Partition;
use crate::health::{Health, HealthStatus, health};
use crate::io::ReadAt;
use crate::pool::{Issue, Space};
use crate::reader::{OpenOptions, Part, SpaceReader};
use crate::report::{Check, Evidence, Report, Status, Truth};

/// How much the checks may read.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Bytes `space.journal` may read to check stripes against their
    /// parity (`None`: every listed stripe, as `spaces check --deep`).
    pub journal_budget: Option<u64>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            journal_budget: Some(1 << 30),
        }
    }
}

/// The checks of the file system in a partition of a space (or in the
/// whole space, `number` 0): the space, the partition, and the device
/// names for evidence. The hook returns no check for a partition it has
/// nothing to say about.
pub type FsHook<'h, D> = dyn FnMut(&SpaceReader<'_, D>, &Partition, &[String]) -> Vec<Check> + 'h;

/// The name of member device `device` for evidence.
fn device_name(names: &[String], device: usize) -> String {
    names.get(device).cloned().unwrap_or_else(|| format!("device {device}"))
}

/// The disk of the pool database `id`, as people know it.
fn disk_label<D>(pool: &Pool<D>, id: u64) -> String {
    match pool.disks.get(&id) {
        Some(d) if !d.name.is_empty() => format!("disk {id} \"{}\" ({})", d.name, d.guid),
        Some(d) => format!("disk {id} ({})", d.guid),
        None => format!("disk {id}"),
    }
}

/// The checks of the pool, the same for each of its spaces.
pub fn pool_checks<D: ReadAt>(pool: &Pool<D>, names: &[String]) -> Vec<Check> {
    vec![
        quorum_check(pool, names),
        members_check(pool, names),
        database_check(pool, names),
        clean_check(pool, names),
    ]
}

fn quorum_check<D>(pool: &Pool<D>, names: &[String]) -> Check {
    const ID: &str = "pool.quorum";
    let copies: Vec<_> = pool.disks.values().filter(|d| d.database_copy).collect();
    let at_hand: Vec<_> = copies.iter().filter(|d| d.member.is_some()).collect();
    if copies.is_empty() {
        return Check::skipped(ID, "the pool database names no disk that keeps a copy of it");
    }
    if at_hand.len() * 2 > copies.len() {
        return Check::ok(
            ID,
            format!(
                "{} of {} copies of the pool database at hand",
                at_hand.len(),
                copies.len()
            ),
        )
        .truth(Truth::Windows);
    }
    let present: Vec<String> = at_hand
        .iter()
        .filter_map(|d| d.member.map(|m| device_name(names, pool.members[m].device)))
        .collect();
    let missing: Vec<String> = copies
        .iter()
        .filter(|d| d.member.is_none())
        .map(|d| disk_label(pool, d.id))
        .collect();
    Check::new(
        ID,
        Status::Suspect,
        format!(
            "only {} of {} copies of the pool database at hand: the pool lacks its quorum",
            at_hand.len(),
            copies.len()
        ),
    )
    .evidence(
        Evidence::new("disks that keep a copy of the pool database")
            .expected(format!("more than {} of {} at hand", copies.len() / 2, copies.len()))
            .found(format!(
                "{} at hand ({}); missing: {}",
                at_hand.len(),
                if present.is_empty() {
                    "none".into()
                } else {
                    present.join(", ")
                },
                missing.join(", ")
            )),
    )
    .truth(Truth::Windows)
    .advice(
        "Windows takes such a pool read-only and detaches its spaces: the disks at hand may be \
         ones that dropped out earlier, and their metadata an old state of the pool. Connect the \
         missing disks.",
    )
}

fn members_check<D>(pool: &Pool<D>, names: &[String]) -> Check {
    const ID: &str = "pool.members";
    let total = pool.disks.len();
    let present: Vec<String> = pool
        .disks
        .values()
        .filter_map(|d| d.member.map(|m| device_name(names, pool.members[m].device)))
        .collect();
    let missing: Vec<_> = pool.disks.values().filter(|d| d.member.is_none()).collect();
    if missing.is_empty() {
        return Check::ok(ID, format!("{total} of {total} disks ({})", present.join(", "))).truth(Truth::Windows);
    }
    let mut check = Check::new(
        ID,
        Status::Degraded,
        format!("{} of {total} disks: {} missing", present.len(), missing.len()),
    )
    .truth(Truth::Windows)
    .advice(
        "Connect the missing disks. Whether the data of a space is complete without them is \
         space.state's verdict.",
    );
    for d in missing {
        check = check.evidence(
            Evidence::new(format!("{}, {}", disk_label(pool, d.id), d.usage.name()))
                .expected("present (the pool database lists it)")
                .found("not among the devices"),
        );
    }
    check
}

fn database_check<D>(pool: &Pool<D>, names: &[String]) -> Check {
    const ID: &str = "pool.database";
    let devices = |list: &[usize]| {
        list.iter()
            .map(|&d| device_name(names, d))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut evidence = Vec::new();
    let mut newer_unusable = false;
    for issue in &pool.issues {
        let e = match issue {
            Issue::StaleCopy {
                device,
                sequence,
                current,
            } => Evidence::new(format!(
                "the copy of the pool database on {}",
                device_name(names, *device)
            ))
            .expected(format!("sequence {current}, the newest"))
            .found(format!("sequence {sequence} (older, not used)")),
            Issue::TornCopy {
                devices: list,
                sequence,
                used,
            } => Evidence::new(format!("the copies of sequence {sequence}"))
                .expected(format!("the same as on {}", devices(used)))
                .found(format!("different on {} (a torn write, not used)", devices(list))),
            Issue::UnusableCopy {
                devices: list,
                sequence,
                error,
                newer,
            } => {
                newer_unusable |= newer;
                Evidence::new(format!("the copy of sequence {sequence} on {}", devices(list)))
                    .expected(if *newer {
                        format!("it decodes (it is newer than sequence {})", pool.database.sequence)
                    } else {
                        "it decodes".into()
                    })
                    .found(error.clone())
            }
            Issue::UnreadableCopy { device, error } => Evidence::new(format!(
                "the copy of the pool database on {}",
                device_name(names, *device)
            ))
            .expected("readable")
            .found(error.clone()),
            _ => continue,
        };
        evidence.push(e);
    }
    let copies = pool.members.iter().filter(|m| m.db_sequence.is_some()).count();
    if evidence.is_empty() {
        return Check::ok(
            ID,
            format!(
                "{copies} cop{} of sequence {}, all the same",
                if copies == 1 { "y" } else { "ies" },
                pool.database.sequence
            ),
        )
        .truth(Truth::CrossCheck);
    }
    let mut check = if newer_unusable {
        Check::new(
            ID,
            Status::Suspect,
            format!(
                "the newest copy of the pool database does not decode; sequence {} is read",
                pool.database.sequence
            ),
        )
        .advice(
            "What Windows recorded last cannot be read: the older state may map data where it no \
             longer is. Attach the pool to Windows, which rewrites its copies.",
        )
    } else {
        Check::new(
            ID,
            Status::Warning,
            format!(
                "sequence {} is read; {} other cop{} not used",
                pool.database.sequence,
                evidence.len(),
                if evidence.len() == 1 { "y is" } else { "ies are" }
            ),
        )
        .advice(
            "Windows reads the newest copy too and rewrites the others when it attaches the pool; \
             nothing that is read depends on them.",
        )
    }
    .truth(Truth::CrossCheck);
    for e in evidence {
        check = check.evidence(e);
    }
    check
}

fn clean_check<D>(pool: &Pool<D>, names: &[String]) -> Check {
    const ID: &str = "pool.clean";
    let mut suspect = Vec::new();
    let mut warn = Vec::new();
    for issue in &pool.issues {
        match issue {
            Issue::ForeignDatabase { device, owner } => suspect.push(
                Evidence::new(format!("the pool database on {}", device_name(names, *device)))
                    .expected(format!("owned by this pool ({})", pool.guid))
                    .found(format!("owned by {owner}")),
            ),
            Issue::UnknownSpaceExtent { space } => suspect.push(
                Evidence::new(format!("an extent of space {space}"))
                    .expected("a space the pool database lists")
                    .found("no such space"),
            ),
            Issue::UnlistedDevice { device } => warn.push(
                Evidence::new(format!(
                    "{}, which carries this pool's metadata",
                    device_name(names, *device)
                ))
                .expected("one of the pool's disks")
                .found("not listed in the pool database (a disk removed from the pool?); not used"),
            ),
            Issue::IgnoredDevice { device, error } => warn.push(
                Evidence::new(format!("the disk header on {}", device_name(names, *device)))
                    .expected("a valid header")
                    .found(format!("{error}; the device is not used")),
            ),
            _ => {}
        }
    }
    if suspect.is_empty() && warn.is_empty() {
        return Check::ok(ID, "the pool's metadata agrees with itself and with the devices");
    }
    let (status, summary, advice) = if suspect.is_empty() {
        (
            Status::Warning,
            format!("{} device(s) with this pool's metadata not used", warn.len()),
            "The devices named are not read; the pool's disks are what pool.members lists.",
        )
    } else {
        (
            Status::Suspect,
            format!("{} inconsistenc(ies) in the pool's metadata", suspect.len()),
            "The pool's metadata contradicts itself: what is read may not be what Windows reads. \
             Attach the pool to Windows to see what it makes of it.",
        )
    };
    let mut check = Check::new(ID, status, summary).truth(Truth::Format).advice(advice);
    for e in suspect.into_iter().chain(warn) {
        check = check.evidence(e);
    }
    check
}

/// The spaces of `space`'s family (tiers, cache, logs) with copies on the
/// disks for which `missing` holds: (disk label, extents).
fn extents_on<D: ReadAt>(pool: &Pool<D>, space: &Space, missing: impl Fn(u64) -> bool) -> Vec<(String, usize)> {
    let mut family = vec![space];
    let mut i = 0;
    while i < family.len() {
        family.extend(pool.children(family[i].id()));
        i += 1;
    }
    let mut by_disk = std::collections::BTreeMap::new();
    for e in family.iter().flat_map(|s| &s.extents).filter(|e| missing(e.disk_id)) {
        *by_disk.entry(e.disk_id).or_insert(0) += 1;
    }
    by_disk
        .into_iter()
        .map(|(disk, n)| (disk_label(pool, disk), n))
        .collect()
}

fn state_check<D: ReadAt>(pool: &Pool<D>, space: &Space, health: &Result<Health, String>) -> Check {
    const ID: &str = "space.state";
    let h = match health {
        Ok(h) => h,
        Err(e) => {
            return Check::new(ID, Status::Failed, format!("the state cannot be worked out: {e}"))
                .truth(Truth::Windows)
                .advice("The layout of the space is not understood; nothing reads it.");
        }
    };
    let Some(s) = h.spaces.iter().find(|s| s.id == space.id()) else {
        return Check::skipped(ID, "the space is not in the pool's list of spaces");
    };
    let left = |n: u64| format!("survives {n} more disk failure{}", if n == 1 { "" } else { "s" });
    let missing = |disk: u64| pool.disks.get(&disk).is_some_and(|d| d.member.is_none());
    let mut check = match s.failures_left {
        _ if s.state.health == HealthStatus::Healthy => {
            return Check::ok(
                ID,
                format!(
                    "{}, as Windows would show it ({})",
                    s.state,
                    left(s.failures_left.unwrap_or(0))
                ),
            )
            .truth(Truth::Windows);
        }
        None => Check::new(
            ID,
            Status::Failed,
            format!("{}: some data is only on missing disks", s.state),
        )
        .advice("Connect the missing disks; without them part of the space is lost, and nothing reads it."),
        Some(_) if h.pool.health == HealthStatus::Unhealthy => Check::new(
            ID,
            Status::Suspect,
            format!("{}: the pool lacks its quorum (pool.quorum)", s.state),
        )
        .advice("Windows detaches the space until the pool has its quorum again."),
        Some(n) => Check::new(
            ID,
            Status::Degraded,
            format!("{}: redundancy reduced, the data complete ({})", s.state, left(n)),
        )
        .advice(
            "Windows repairs the space once its disks are back (or with Repair-VirtualDisk); \
             until then a further failure may lose data.",
        ),
    }
    .truth(Truth::Windows);
    for (disk, n) in extents_on(pool, space, missing) {
        check = check.evidence(
            Evidence::new(format!("{n} extent(s) of the space on {disk}"))
                .expected("the disk at hand")
                .found("missing"),
        );
    }
    let stale: usize = {
        let mut family = vec![space];
        let mut i = 0;
        while i < family.len() {
            family.extend(pool.children(family[i].id()));
            i += 1;
        }
        family
            .iter()
            .flat_map(|s| &s.extents)
            .filter(|e| !e.is_current())
            .count()
    };
    if stale > 0 {
        check = check.evidence(
            Evidence::new(format!("{stale} copies of extents marked out of date"))
                .expected("every copy current")
                .found("their disks missed writes (they are not read)"),
        );
    }
    check
}

/// The checks of user space `space` of `pool`: the pool's, the space's
/// and, through `fs`, those of its file systems.
/// `names` names the pool's devices in evidence.
pub fn space_report<D: ReadAt>(
    pool: &Pool<D>,
    space: &Space,
    names: &[String],
    options: &Options,
    fs: &mut FsHook<'_, D>,
) -> Report {
    let mut report = Report::new(format!(
        "space \"{}\" ({}) of pool \"{}\" ({})",
        space.name(),
        space.info.guid,
        pool.name,
        pool.guid
    ));
    report.pool = Some((pool.name.clone(), pool.guid.to_string()));
    report.space = Some((space.name().to_owned(), space.info.guid.to_string()));
    report.checks = pool_checks(pool, names);
    let h = health(pool).map_err(|e| e.to_string());
    report.checks.push(state_check(pool, space, &h));
    report.checks.extend(space_checks(pool, space, names, options, fs));
    report
}

/// Reports for every user space of `pool`.
pub fn pool_reports<D: ReadAt>(
    pool: &Pool<D>,
    names: &[String],
    options: &Options,
    fs: &mut FsHook<'_, D>,
) -> Vec<Report> {
    pool.user_spaces()
        .map(|s| space_report(pool, s, names, options, fs))
        .collect()
}

/// `space.layout` to `space.drl`, and the file systems.
fn space_checks<D: ReadAt>(
    pool: &Pool<D>,
    space: &Space,
    names: &[String],
    options: &Options,
    fs: &mut FsHook<'_, D>,
) -> Vec<Check> {
    let reader = match SpaceReader::open_parts(pool, space.id(), OpenOptions::default()) {
        Ok(r) => r,
        Err((part, e)) => {
            let (id, what) = match part {
                Part::Layout | Part::Tiers => ("space.layout", "the slab map of the space"),
                Part::Cache => ("space.cache", "the write-back cache"),
                Part::Journal => ("space.journal", "the parity journal"),
                Part::DirtyRegions => ("space.drl", "the dirty region log"),
            };
            let status = Status::Failed;
            let mut checks = vec![
                Check::new(id, status, format!("{what} cannot be read"))
                    .evidence(Evidence::new(what).expected("valid").found(e.to_string()))
                    .truth(Truth::Format)
                    .advice("The space is not read: what it holds is not understood."),
            ];
            for other in ["space.layout", "space.cache", "space.journal", "space.drl"] {
                if other != id {
                    checks.push(Check::skipped(other, format!("not checked: {id} failed")));
                }
            }
            return checks;
        }
    };
    let mut checks = vec![layout_check(pool, space, &reader)];
    checks.push(cache_check(&reader, names));
    checks.push(journal_check(&reader, names, options));
    checks.push(drl_check(&reader));
    let sector = u64::from(pool.logical_sector_size);
    let whole = Partition {
        number: 0,
        offset: 0,
        length: reader.size(),
        kind: String::new(),
        name: String::new(),
    };
    let targets = match crate::gpt::read_partitions(&reader, sector) {
        Ok(parts) if !parts.is_empty() => parts,
        _ => vec![whole],
    };
    for p in &targets {
        checks.extend(fs(&reader, p, names));
    }
    checks
}

fn layout_check<D: ReadAt>(pool: &Pool<D>, space: &Space, reader: &SpaceReader<'_, D>) -> Check {
    let l = reader.layout();
    let disks: BTreeSet<u64> = space.extents.iter().map(|e| e.disk_id).collect();
    let tiers = pool
        .children(space.id())
        .filter(|c| c.info.is_child && !c.extents.is_empty())
        .count();
    Check::ok(
        "space.layout",
        format!(
            "{:?}, {} column(s), {} cop{}, interleave {} KiB, {} extent(s) on {} disk(s){}",
            l.resiliency,
            l.columns,
            l.copies,
            if l.copies == 1 { "y" } else { "ies" },
            l.interleave >> 10,
            space.extents.len(),
            disks.len(),
            if tiers > 0 {
                format!(", {tiers} storage tier(s)")
            } else {
                String::new()
            }
        ),
    )
    .truth(Truth::Format)
}

fn cache_check<D: ReadAt>(reader: &SpaceReader<'_, D>, names: &[String]) -> Check {
    const ID: &str = "space.cache";
    let Some(cache) = reader.cache() else {
        return Check::skipped(ID, "no write-back cache");
    };
    let chunks = cache.cached_chunks();
    let partly = cache
        .mappings()
        .iter()
        .filter(|m| !matches!(m.2, crate::cache::Validity::Full))
        .count();
    let conflicts = cache.conflicting_offsets();
    if !conflicts.is_empty() {
        let mut check = Check::new(
            ID,
            Status::Suspect,
            format!(
                "the copies of the cache disagree about {} chunk(s) after an unclean shutdown",
                conflicts.len()
            ),
        )
        .truth(Truth::CrossCheck)
        .advice(
            "A cache update reached only some copies of the cache: which one Windows keeps is not \
             known. Windows settles it when it next attaches the pool. Reads of those chunks fail \
             (--unclean-parity data reads the newest).",
        );
        for &offset in conflicts.iter().take(5) {
            check = check.evidence(
                Evidence::new(format!(
                    "the chunk of {} KiB at space byte {offset:#x}",
                    cache.header.chunk_size >> 10
                ))
                .at(reader.describe(offset, names))
                .expected("mapped the same way by every copy of the cache")
                .found("mapped differently"),
            );
        }
        return check;
    }
    if chunks == 0 {
        return Check::ok(ID, "empty").truth(Truth::CrossCheck);
    }
    Check::new(
        ID,
        Status::Info,
        format!("{chunks} chunk(s) cached ({partly} partly), the copies agree"),
    )
    .truth(Truth::CrossCheck)
}

fn journal_check<D: ReadAt>(reader: &SpaceReader<'_, D>, names: &[String], options: &Options) -> Check {
    const ID: &str = "space.journal";
    let check = match reader.check_unclean_parity(options.journal_budget) {
        Ok(None) => return Check::skipped(ID, "no parity journal (not a parity space)"),
        Ok(Some(c)) => c,
        Err(e) => {
            return Check::new(ID, Status::Failed, "the listed stripes cannot be read")
                .evidence(
                    Evidence::new("stripes the journal does not record as consistent")
                        .expected("readable")
                        .found(e.to_string()),
                )
                .truth(Truth::CrossCheck);
        }
    };
    if check.listed == 0 {
        return Check::ok(ID, "every stripe recorded as consistent").truth(Truth::Windows);
    }
    let cached = if check.cached > 0 {
        format!(", {} held whole by the cache", check.cached)
    } else {
        String::new()
    };
    if !check.mismatches.is_empty() {
        let mut c = Check::new(
            ID,
            Status::Suspect,
            format!(
                "{} of {} stripe(s) not recorded as consistent do not match their parity",
                check.mismatches.len(),
                check.checked
            ),
        )
        .truth(Truth::CrossCheck)
        .advice(
            "These stripes were being written when the pool was last in use, or never written \
             and hold what the disks held before. Windows settles them when it next attaches the \
             pool; reads of them fail (--unclean-parity data reads them as on disk).",
        );
        for &offset in check.mismatches.iter().take(5) {
            c = c.evidence(
                Evidence::new("a stripe the parity journal does not record as consistent")
                    .at(reader.describe(offset, names))
                    .expected("its parity equal to the XOR of its data units")
                    .found("different"),
            );
        }
        return c;
    }
    let unread = if check.unreadable > 0 {
        format!(", {} with a unit on a missing disk", check.unreadable)
    } else {
        String::new()
    };
    if check.incomplete {
        let left = check.listed - check.checked - check.cached - check.unreadable;
        return Check::new(
            ID,
            Status::Info,
            format!(
                "{} stripe(s) not recorded as consistent (never written, or written when last in use): \
                 {} checked and matching{cached}{unread}, {left} left to the read budget of {} MiB \
                 (each is checked when read; spaces check --deep checks them all)",
                check.listed,
                check.checked,
                options.journal_budget.unwrap_or(0) >> 20
            ),
        )
        .truth(Truth::CrossCheck);
    }
    if check.checked == 0 && check.unreadable == 0 {
        return Check::new(
            ID,
            Status::Info,
            format!(
                "{} stripe(s) not recorded as consistent, all held whole by the write-back cache",
                check.listed
            ),
        )
        .truth(Truth::CrossCheck);
    }
    Check::new(
        ID,
        Status::Info,
        format!(
            "{} stripe(s) not recorded as consistent (never written, or written when last in use): \
             {} checked, all match their parity{cached}{unread}",
            check.listed, check.checked
        ),
    )
    .truth(Truth::CrossCheck)
}

fn drl_check<D: ReadAt>(reader: &SpaceReader<'_, D>) -> Check {
    const ID: &str = "space.drl";
    match reader.dirty_regions() {
        None => Check::skipped(ID, "no dirty region log (not a mirror space)"),
        Some(d) if d.dirty_runs() == 0 => Check::ok(ID, "no extent runs listed").truth(Truth::Windows),
        Some(d) => Check::new(
            ID,
            Status::Info,
            format!(
                "{} extent run(s) written since the space was last disconnected: their copies are \
                 compared when read",
                d.dirty_runs()
            ),
        )
        .truth(Truth::Windows),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::path::Path;

    use super::*;
    use crate::format::{ExtentRecord, SpaceRole};
    use crate::io::SparseImage;
    use crate::layout::Layout;
    use crate::report::Verdict;

    fn images(rel: &str) -> Vec<SparseImage> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join(rel);
        let v: Vec<SparseImage> = (0..)
            .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
            .map(|f| SparseImage::read_from(f).unwrap())
            .collect();
        assert!(!v.is_empty(), "{rel}");
        v
    }

    fn names(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("/dev/disk{i}")).collect()
    }

    fn no_fs<D: ReadAt>(_: &SpaceReader<'_, D>, _: &Partition, _: &[String]) -> Vec<Check> {
        Vec::new()
    }

    fn reports<D: ReadAt>(pool: &Pool<D>, options: &Options) -> Vec<Report> {
        pool_reports(pool, &names(8), options, &mut no_fs)
    }

    fn check<'r>(r: &'r Report, id: &str) -> &'r Check {
        r.checks
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("no {id} in\n{}", r.to_text()))
    }

    #[test]
    fn a_healthy_pool_passes_every_check() {
        let disks = images("fixtures/mirror3");
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let all = reports(&pool, &Options::default());
        assert!(!all.is_empty());
        for r in &all {
            assert_eq!(r.verdict(), Verdict::Healthy, "{}", r.to_text());
            for id in [
                "pool.quorum",
                "pool.members",
                "pool.database",
                "pool.clean",
                "space.state",
                "space.layout",
            ] {
                assert_eq!(check(r, id).status, Status::Ok, "{id}\n{}", r.to_text());
            }
            assert!(r.subject.starts_with("space \""), "{}", r.subject);
            assert_eq!(r.pool.as_ref().unwrap().0, pool.name);
        }
        assert!(
            check(&all[0], "pool.members").summary.contains("/dev/disk0"),
            "{}",
            all[0].to_text()
        );
    }

    #[test]
    fn a_missing_disk_degrades_and_names_the_disk() {
        let disks = images("fixtures/mirror3");
        let pool = Pool::open(disks.iter().skip(1).collect::<Vec<_>>()).unwrap();
        let r = &reports(&pool, &Options::default())[0];
        assert_eq!(r.verdict(), Verdict::Degraded, "{}", r.to_text());
        let members = check(r, "pool.members");
        assert_eq!(members.status, Status::Degraded);
        assert_eq!(members.truth, Some(Truth::Windows));
        let missing = pool.disks.values().find(|d| d.member.is_none()).unwrap();
        assert!(
            members.evidence[0].what.contains(&missing.guid.to_string()),
            "{:?}",
            members.evidence
        );
        assert_eq!(members.evidence[0].found.as_deref(), Some("not among the devices"));
        assert_eq!(check(r, "space.state").status, Status::Degraded);
        assert!(check(r, "space.state").summary.starts_with("Warning / Degraded"));
    }

    #[test]
    fn without_a_quorum_the_pool_is_suspect_and_lost_data_failed() {
        // One of four disks: no quorum; spaces with data only on the
        // others have lost it.
        let disks = images("scenarios/c11health");
        let pool = Pool::open(vec![&disks[0]]).unwrap();
        let all = reports(&pool, &Options::default());
        for r in &all {
            let quorum = check(r, "pool.quorum");
            assert_eq!(quorum.status, Status::Suspect, "{}", r.to_text());
            assert_eq!(quorum.evidence[0].expected.as_deref(), Some("more than 2 of 4 at hand"));
            assert!(r.verdict() >= Verdict::Suspect);
        }
        assert!(all.iter().any(|r| r.verdict() == Verdict::Failed));
        // Two disks of a two-column simple space: one lost, its data with it.
        let disks = images("fixtures/simple2c");
        let pool = Pool::open(vec![&disks[0]]).unwrap();
        let r = &reports(&pool, &Options::default())[0];
        let state = check(r, "space.state");
        assert_eq!(state.status, Status::Failed, "{}", r.to_text());
        assert!(state.summary.contains("only on missing disks"));
        assert_eq!(r.verdict(), Verdict::Failed);
    }

    #[test]
    fn database_copies_not_used_are_warnings_but_a_newer_one_is_suspect() {
        let disks = images("scenarios/m5stale/s4");
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let r = &reports(&pool, &Options::default())[0];
        let db = check(r, "pool.database");
        assert_eq!(db.status, Status::Warning, "{}", r.to_text());
        assert!(db.evidence[0].found.as_deref().unwrap().contains("older, not used"));
        assert_eq!(r.verdict(), Verdict::Healthy);

        let disks = images("fixtures/mirror3");
        let mut pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        pool.issues.push(Issue::TornCopy {
            devices: vec![1],
            sequence: pool.database.sequence,
            used: vec![0, 2],
        });
        let r = &reports(&pool, &Options::default())[0];
        assert_eq!(check(r, "pool.database").status, Status::Warning);
        pool.issues.push(Issue::UnusableCopy {
            devices: vec![2],
            sequence: pool.database.sequence + 1,
            error: "invalid on-disk data: pool database has no pool record".into(),
            newer: true,
        });
        let r = &reports(&pool, &Options::default())[0];
        let db = check(r, "pool.database");
        assert_eq!(db.status, Status::Suspect, "{}", r.to_text());
        assert!(
            db.evidence
                .iter()
                .any(|e| e.found.as_deref().unwrap().contains("no pool record"))
        );
        assert_eq!(r.verdict(), Verdict::Suspect);
    }

    #[test]
    fn metadata_that_contradicts_itself_is_suspect() {
        let disks = images("fixtures/mirror3");
        let mut pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        pool.issues.push(Issue::UnlistedDevice { device: 4 });
        let r = &reports(&pool, &Options::default())[0];
        assert_eq!(check(r, "pool.clean").status, Status::Warning, "{}", r.to_text());
        pool.issues.push(Issue::UnknownSpaceExtent { space: 99 });
        let r = &reports(&pool, &Options::default())[0];
        let clean = check(r, "pool.clean");
        assert_eq!(clean.status, Status::Suspect, "{}", r.to_text());
        assert_eq!(clean.evidence[0].what, "an extent of space 99");
        assert_eq!(clean.truth, Some(Truth::Format));
    }

    #[test]
    fn a_slab_map_that_does_not_parse_fails_the_space() {
        let disks = images("fixtures/mirror3");
        let mut pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let id = pool.user_spaces().next().unwrap().id();
        let space = pool.spaces.get_mut(&id).unwrap();
        let twice: ExtentRecord = space.extents[0];
        space.extents.push(twice);
        let r = &reports(&pool, &Options::default())[0];
        let layout = check(r, "space.layout");
        assert_eq!(layout.status, Status::Failed, "{}", r.to_text());
        assert!(layout.evidence[0].found.as_deref().unwrap().contains("overlapping"));
        assert_eq!(check(r, "space.cache").status, Status::Skipped);
        assert_eq!(r.verdict(), Verdict::Failed);
    }

    /// Device and byte offset of byte `offset` of the write-back cache of
    /// `space`, copy `copy`.
    fn cache_byte<D: ReadAt>(pool: &Pool<D>, space: &Space, copy: u64, offset: u64) -> (usize, u64) {
        let container = pool
            .children(space.id())
            .find(|c| c.info.role == SpaceRole::Cache)
            .unwrap();
        let cache = pool.children(container.id()).find(|c| !c.extents.is_empty()).unwrap();
        let base = cache.info.range.map_or(0, |(start, _)| start);
        let l = Layout::with_base(cache.info.policy.as_ref().unwrap(), &cache.extents, base).unwrap();
        let loc = l.locate(base + offset);
        let (disk, slab) = l.physical(loc.column, copy, loc.row).unwrap();
        let (device, start) = pool.slab_location(disk, slab).unwrap().unwrap();
        (device, start + loc.offset_in_slab)
    }

    #[test]
    fn cache_copies_that_disagree_are_suspect() {
        let mut disks = images("scenarios/m5wbc2/s1");
        let (device, at, len) = {
            let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
            let space = pool.user_spaces().next().unwrap();
            let reader = pool.open_space(space.id()).unwrap();
            let cache = reader.cache().unwrap();
            assert!(cache.cached_chunks() > 0);
            let slot = cache.slots().iter().find(|s| s.kind == 0).unwrap().index as u64;
            let h = &cache.header;
            let (device, at) = cache_byte(&pool, space, 1, h.slot_offset + slot * u64::from(h.slot_size));
            (device, at, h.slot_size as usize)
        };
        // The newest slot reached only copy 0.
        disks[device].insert(at, &vec![0u8; len]);
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let r = &reports(&pool, &Options::default())[0];
        let cache = check(r, "space.cache");
        assert_eq!(cache.status, Status::Suspect, "{}", r.to_text());
        assert_eq!(cache.truth, Some(Truth::CrossCheck));
        assert!(
            cache.evidence[0]
                .location
                .as_deref()
                .unwrap()
                .contains("in the write-back cache"),
            "{:?}",
            cache.evidence
        );
        assert_eq!(r.verdict(), Verdict::Suspect);
    }

    #[test]
    fn listed_stripes_are_checked_against_their_parity() {
        let mut disks = images("scenarios/m5pj2/s5");
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let small = Options {
            journal_budget: Some(64 << 20),
        };
        let r = &reports(&pool, &small)[0];
        let journal = check(r, "space.journal");
        assert_eq!(journal.status, Status::Info, "{}", r.to_text());
        assert!(
            journal.summary.contains("left to the read budget of 64 MiB"),
            "{}",
            journal.summary
        );
        assert_eq!(r.verdict(), Verdict::Healthy);
        assert!(journal.summary.contains("checked and matching"), "{}", journal.summary);
        // Stripe 8 (4 MiB in) was never written; give one of its data units
        // other bytes than its parity covers.
        let (device, at) = {
            let space = pool.user_spaces().next().unwrap();
            let reader = pool.open_space(space.id()).unwrap();
            let l = reader.layout();
            assert!(reader.journal().unwrap().is_dirty(0, 8));
            let loc = l.locate(8 * l.data_columns * l.interleave);
            let (disk, slab) = l.physical(loc.column, 0, loc.row).unwrap();
            let (device, start) = pool.slab_location(disk, slab).unwrap().unwrap();
            (device, start + loc.offset_in_slab)
        };
        drop(pool);
        disks[device].insert(at, &[0xab; 4096]);
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let r = &reports(&pool, &small)[0];
        let journal = check(r, "space.journal");
        assert_eq!(journal.status, Status::Suspect, "{}", r.to_text());
        assert_eq!(journal.evidence.len(), 1);
        let place = journal.evidence[0].location.as_deref().unwrap();
        assert!(place.starts_with("space byte 0x400000"), "{place}");
        assert!(place.contains(&format!("/dev/disk{device} at {at:#x}")), "{place}");
        assert_eq!(r.verdict(), Verdict::Suspect);
    }

    #[test]
    fn listed_mirror_runs_are_information() {
        let disks = images("fixtures/drtdism");
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let r = &reports(&pool, &Options::default())[0];
        assert_eq!(check(r, "space.drl").status, Status::Info, "{}", r.to_text());
        assert_eq!(r.verdict(), Verdict::Healthy);
    }
}
