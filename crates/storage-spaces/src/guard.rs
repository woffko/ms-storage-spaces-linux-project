//! The guard: what a space must pass before it is attached or read without
//! being asked ([`Verdict::Healthy`](crate::report::Verdict)). Each check
//! looks at one thing Windows recorded (the pool database, the space's
//! logs) or one cross-check (copies that must agree, parity that must
//! match), and says what it found and where; the file systems in a space
//! are checked through a hook ([`ntfs_check`] here, ReFS in the `refs`
//! crate). The checks are quick: a few reads per space and partition, and
//! a bounded read of the stripes a parity journal does not record as
//! consistent.
//!
//! Check ids: `pool.quorum`, `pool.members`, `pool.database`,
//! `pool.clean`, `space.state`, `space.layout`, `space.cache`,
//! `space.journal`, `space.drl`, `space.partitions`, and `fs.*` from the
//! hook.

use std::collections::BTreeSet;

use crate::Pool;
use crate::crc::crc32;
use crate::gpt::Partition;
use crate::health::{Health, HealthStatus, health};
use crate::io::{ReadAt, read_vec};
use crate::pool::{Issue, Space};
use crate::reader::{OpenOptions, Part, SpaceReader};
use crate::report::{Check, Evidence, Report, Status, Truth, hex, signature};

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
    let family = pool.family(space.id());
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
    let stale = pool
        .family(space.id())
        .iter()
        .flat_map(|s| &s.extents)
        .filter(|e| !e.is_current())
        .count();
    if stale > 0 {
        check = check.evidence(
            Evidence::new(format!("{stale} copies of extents marked out of date"))
                .expected("every copy current")
                .found("their disks missed writes (they are not read)"),
        );
    }
    check
}

/// The checks of user space `space` of `pool`: the pool's, the space's,
/// its partition table's and, through `fs`, those of its file systems.
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

/// `space.layout` to `space.partitions` and the file systems.
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
            for other in [
                "space.layout",
                "space.cache",
                "space.journal",
                "space.drl",
                "space.partitions",
            ] {
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
    let at = |lba: u64| format!("LBA {lba} ({})", reader.describe(lba * sector, names));
    let (check, partitions) = partition_check(&reader, reader.size(), sector, &at);
    checks.push(check);
    let whole = Partition {
        number: 0,
        offset: 0,
        length: reader.size(),
        kind: String::new(),
        name: String::new(),
    };
    let targets = partitions.unwrap_or_else(|| vec![whole]);
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

/// A GPT header as found.
struct GptHeader {
    lba: u64,
    raw: Vec<u8>,
    header_size: usize,
    my_lba: u64,
    alternate: u64,
    first_usable: u64,
    last_usable: u64,
    disk_guid: [u8; 16],
    entries_lba: u64,
    count: usize,
    entry_size: usize,
    entries_crc: u32,
}

impl GptHeader {
    fn parse(raw: Vec<u8>, lba: u64) -> Option<GptHeader> {
        if raw.get(0..8) != Some(b"EFI PART") || raw.len() < 92 {
            return None;
        }
        let u32_at = |o: usize| u32::from_le_bytes(raw[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(raw[o..o + 8].try_into().unwrap());
        Some(GptHeader {
            lba,
            header_size: u32_at(0x0c) as usize,
            my_lba: u64_at(0x18),
            alternate: u64_at(0x20),
            first_usable: u64_at(0x28),
            last_usable: u64_at(0x30),
            disk_guid: raw[0x38..0x48].try_into().unwrap(),
            entries_lba: u64_at(0x48),
            count: u32_at(0x50) as usize,
            entry_size: u32_at(0x54) as usize,
            entries_crc: u32_at(0x58),
            raw,
        })
    }

    /// Whether the header's own CRC32 matches.
    fn crc_ok(&self) -> bool {
        if !(92..=self.raw.len()).contains(&self.header_size) {
            return false;
        }
        let mut copy = self.raw[..self.header_size].to_vec();
        let stored = u32::from_le_bytes(copy[0x10..0x14].try_into().unwrap());
        copy[0x10..0x14].fill(0);
        crc32(&copy) == stored
    }

    /// Whether the entry array is plausible (and small enough to read).
    fn table_ok(&self, lbas: u64) -> bool {
        (128..=4096).contains(&self.entry_size)
            && self.entry_size.is_multiple_of(8)
            && self.count <= 4096
            && self.entries_lba < lbas
    }
}

/// `space.partitions`: the protective MBR, both GPT headers with their
/// CRCs and positions, both entry arrays the same, and the partitions
/// inside the space without overlapping; or an MBR's partitions. Returns
/// the partitions to check the file systems of (`None`: no partition
/// table, the whole space is checked).
fn partition_check(
    dev: &dyn ReadAt,
    size: u64,
    sector: u64,
    at: &dyn Fn(u64) -> String,
) -> (Check, Option<Vec<Partition>>) {
    const ID: &str = "space.partitions";
    let lbas = size / sector;
    let read = |lba: u64, len: usize| -> Result<Vec<u8>, String> {
        let offset = lba.checked_mul(sector).filter(|o| o + len as u64 <= size);
        let offset = offset.ok_or_else(|| format!("LBA {lba} lies beyond the end of the space"))?;
        read_vec(dev, offset, len).map_err(|e| format!("LBA {lba}: {e}"))
    };
    let unreadable = |e: String| {
        Check::new(ID, Status::Suspect, "the partition table cannot be read")
            .evidence(Evidence::new("the partition table").expected("readable").found(e))
            .truth(Truth::CrossCheck)
    };
    if lbas < 2 {
        return (Check::skipped(ID, "the space is too small for a partition table"), None);
    }
    let mbr = match read(0, 512) {
        Ok(b) => b,
        Err(e) => return (unreadable(e), Some(Vec::new())),
    };
    let mbr_entries: Vec<(u8, u64, u64)> = if mbr[510..512] == [0x55, 0xaa] {
        (0..4)
            .map(|i| &mbr[446 + i * 16..462 + i * 16])
            .map(|e| {
                (
                    e[4],
                    u64::from(u32::from_le_bytes(e[8..12].try_into().unwrap())),
                    u64::from(u32::from_le_bytes(e[12..16].try_into().unwrap())),
                )
            })
            .filter(|&(kind, _, count)| kind != 0 && count != 0)
            .collect()
    } else {
        Vec::new()
    };
    let protective = mbr_entries.iter().any(|&(kind, ..)| kind == 0xee);
    let primary = match read(1, sector as usize) {
        Ok(b) => GptHeader::parse(b, 1),
        Err(e) => return (unreadable(e), Some(Vec::new())),
    };
    let Some(primary) = primary else {
        // No GPT: an MBR's partitions, or none.
        if mbr_entries.is_empty() {
            return (
                Check::skipped(ID, "no partition table (the whole space is checked)"),
                None,
            );
        }
        let parts: Vec<Partition> = mbr_entries
            .iter()
            .enumerate()
            .map(|(i, &(kind, start, count))| Partition {
                number: i as u32 + 1,
                offset: start * sector,
                length: count * sector,
                kind: format!("mbr:{kind:#04x}"),
                name: String::new(),
            })
            .collect();
        let problems = bounds_problems(&parts, 1, lbas.saturating_sub(1), sector, at);
        if problems.is_empty() {
            return (
                Check::ok(ID, format!("MBR with {} partition(s)", parts.len())).truth(Truth::Format),
                Some(parts),
            );
        }
        let mut check = Check::new(ID, Status::Suspect, "MBR partitions outside the space or overlapping")
            .truth(Truth::Format)
            .advice("The partition table does not fit the space: what is read may not be what Windows reads.");
        for e in problems {
            check = check.evidence(e);
        }
        let good = parts.into_iter().filter(|p| p.offset + p.length <= size).collect();
        return (check, Some(good));
    };

    let mut problems: Vec<Evidence> = Vec::new();
    let mut warnings: Vec<Evidence> = Vec::new();
    if !primary.crc_ok() {
        problems.push(
            Evidence::new("the primary GPT header's CRC32")
                .at(at(1))
                .expected("the CRC32 of its first bytes (header size)")
                .found(format!(
                    "header size {}, stored {:08x}",
                    primary.header_size,
                    u32::from_le_bytes(primary.raw[0x10..0x14].try_into().unwrap())
                )),
        );
    }
    if primary.my_lba != 1 {
        problems.push(
            Evidence::new("the primary GPT header's own LBA")
                .at(at(1))
                .expected("1")
                .found(primary.my_lba.to_string()),
        );
    }
    let entries_of = |h: &GptHeader| -> Result<Vec<u8>, String> {
        if !h.table_ok(lbas) {
            return Err(format!(
                "{} entries of {} bytes from LBA {} (not plausible)",
                h.count, h.entry_size, h.entries_lba
            ));
        }
        read(h.entries_lba, h.count * h.entry_size)
    };
    let primary_entries = match entries_of(&primary) {
        Ok(t) => {
            if crc32(&t) != primary.entries_crc {
                problems.push(
                    Evidence::new("the primary partition entries' CRC32")
                        .at(at(primary.entries_lba))
                        .expected(format!("{:08x} (the header's)", primary.entries_crc))
                        .found(format!("{:08x}", crc32(&t))),
                );
            }
            Some(t)
        }
        Err(e) => {
            problems.push(
                Evidence::new("the primary partition entries")
                    .at(at(primary.entries_lba))
                    .expected("readable")
                    .found(e),
            );
            None
        }
    };
    let last = lbas - 1;
    // The backup header where the primary says it is.
    let backup = if primary.alternate == 0 || primary.alternate > last {
        problems.push(
            Evidence::new("the backup GPT header's LBA, as the primary names it")
                .at(at(1))
                .expected(format!("inside the space (at most {last})"))
                .found(primary.alternate.to_string()),
        );
        None
    } else {
        if primary.alternate != last {
            warnings.push(
                Evidence::new("the backup GPT header's LBA")
                    .at(at(1))
                    .expected(format!("{last}, the last LBA of the space"))
                    .found(format!(
                        "{} (the space grew after it was partitioned?)",
                        primary.alternate
                    )),
            );
        }
        match read(primary.alternate, sector as usize) {
            Ok(raw) => {
                let found = signature(&raw[..8]);
                let found = if raw[..8].iter().all(|&b| b == 0) {
                    hex(&raw, 16)
                } else {
                    found
                };
                match GptHeader::parse(raw, primary.alternate) {
                    Some(b) => Some(b),
                    None => {
                        problems.push(
                            Evidence::new("the backup GPT header")
                                .at(at(primary.alternate))
                                .expected(format!(
                                    "\"EFI PART\" (the primary at LBA 1 names LBA {} as its backup)",
                                    primary.alternate
                                ))
                                .found(found),
                        );
                        None
                    }
                }
            }
            Err(e) => {
                problems.push(
                    Evidence::new("the backup GPT header")
                        .at(at(primary.alternate))
                        .expected("readable")
                        .found(e),
                );
                None
            }
        }
    };
    if let Some(b) = &backup {
        if !b.crc_ok() {
            problems.push(
                Evidence::new("the backup GPT header's CRC32")
                    .at(at(b.lba))
                    .expected("the CRC32 of its first bytes (header size)")
                    .found(format!(
                        "header size {}, stored {:08x}",
                        b.header_size,
                        u32::from_le_bytes(b.raw[0x10..0x14].try_into().unwrap())
                    )),
            );
        }
        for (what, expected, found) in [
            ("its own LBA", b.lba, b.my_lba),
            ("the primary's LBA it names", 1, b.alternate),
            ("the first usable LBA", primary.first_usable, b.first_usable),
            ("the last usable LBA", primary.last_usable, b.last_usable),
            ("the number of entries", primary.count as u64, b.count as u64),
            ("the size of an entry", primary.entry_size as u64, b.entry_size as u64),
            (
                "the entries' CRC32",
                u64::from(primary.entries_crc),
                u64::from(b.entries_crc),
            ),
        ] {
            if expected != found {
                problems.push(
                    Evidence::new(format!("the backup GPT header: {what}"))
                        .at(at(b.lba))
                        .expected(format!("{expected:#x}"))
                        .found(format!("{found:#x}")),
                );
            }
        }
        if b.disk_guid != primary.disk_guid {
            problems.push(
                Evidence::new("the backup GPT header: the disk GUID")
                    .at(at(b.lba))
                    .expected(hex(&primary.disk_guid, 16))
                    .found(hex(&b.disk_guid, 16)),
            );
        }
        match (entries_of(b), &primary_entries) {
            (Ok(t), Some(p)) if t != *p => {
                let first = t.iter().zip(p).position(|(a, b)| a != b).unwrap_or(0);
                problems.push(
                    Evidence::new("the backup partition entries")
                        .at(at(b.entries_lba + (first as u64) / sector))
                        .expected(format!(
                            "the same as the primary's: {}",
                            hex(&p[first..(first + 16).min(p.len())], 16)
                        ))
                        .found(hex(&t[first..(first + 16).min(t.len())], 16)),
                );
            }
            (Err(e), _) => problems.push(
                Evidence::new("the backup partition entries")
                    .at(at(b.entries_lba))
                    .expected("readable")
                    .found(e),
            ),
            _ => {}
        }
    }
    let mut parts = Vec::new();
    if let Some(table) = &primary_entries {
        for (i, e) in table.chunks_exact(primary.entry_size).enumerate() {
            if e[0..16].iter().all(|&b| b == 0) {
                continue;
            }
            let first = u64::from_le_bytes(e[32..40].try_into().unwrap());
            let end = u64::from_le_bytes(e[40..48].try_into().unwrap());
            let name: Vec<u16> = e[56..128.min(e.len())]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&c| u16::from_le_bytes(c))
                .collect();
            parts.push(Partition {
                number: i as u32 + 1,
                offset: first.saturating_mul(sector),
                length: end.saturating_sub(first).saturating_add(1).saturating_mul(sector),
                kind: crate::Guid::from_mixed_endian(e[0..16].try_into().unwrap()).to_string(),
                name: String::from_utf16_lossy(&name).trim_end_matches('\0').to_string(),
            });
            if end < first {
                problems.push(
                    Evidence::new(format!("partition {}", i + 1))
                        .at(at(primary.entries_lba))
                        .expected("its first LBA not after its last")
                        .found(format!("{first}..{end}")),
                );
            }
        }
        problems.extend(bounds_problems(
            &parts,
            primary.first_usable,
            primary.last_usable.min(last),
            sector,
            at,
        ));
    }
    let mbr_note = if protective {
        ""
    } else {
        " (no protective MBR: the kernel ignores this GPT; spaces maps the partitions itself)"
    };
    if !problems.is_empty() {
        let mut check = Check::new(
            ID,
            Status::Suspect,
            match problems.first() {
                Some(e) if e.what == "the backup GPT header" => "the backup GPT header is not valid".to_string(),
                Some(e) => format!("{} does not check out", e.what),
                None => unreachable!(),
            },
        )
        .truth(Truth::CrossCheck)
        .advice(
            "Windows keeps two copies of the partition table, and they disagree here (or one is \
             not valid). If Windows shows this space as healthy, spaces probably reads part of it \
             wrongly: please report it with the bundle of spaces check --bundle.",
        );
        for e in problems.into_iter().chain(warnings) {
            check = check.evidence(e);
        }
        let good = parts
            .into_iter()
            .filter(|p| p.length > 0 && p.offset + p.length <= size)
            .collect();
        return (check, Some(good));
    }
    let summary = format!(
        "GPT with {} partition(s), the primary and the backup agree{mbr_note}",
        parts.len()
    );
    let mut check = if warnings.is_empty() {
        Check::ok(ID, summary)
    } else {
        Check::new(ID, Status::Warning, summary).advice(
            "The backup partition table is not at the end of the space; Windows moves it when it \
             next writes the table. Nothing that is read depends on it.",
        )
    }
    .truth(Truth::CrossCheck);
    for e in warnings {
        check = check.evidence(e);
    }
    (check, Some(parts))
}

/// Partitions outside `first..=last` (LBAs) or overlapping each other.
fn bounds_problems(
    parts: &[Partition],
    first: u64,
    last: u64,
    sector: u64,
    at: &dyn Fn(u64) -> String,
) -> Vec<Evidence> {
    let mut out = Vec::new();
    let lba = |p: &Partition| (p.offset / sector, (p.offset + p.length) / sector);
    for p in parts {
        let (start, end) = lba(p);
        if p.length == 0 || start < first || end.saturating_sub(1) > last {
            out.push(
                Evidence::new(format!("partition {}", p.number))
                    .at(at(start))
                    .expected(format!("inside LBAs {first}..={last}"))
                    .found(format!("LBAs {start}..={}", end.saturating_sub(1))),
            );
        }
    }
    let mut sorted: Vec<&Partition> = parts.iter().filter(|p| p.length > 0).collect();
    sorted.sort_by_key(|p| p.offset);
    for w in sorted.windows(2) {
        if w[0].offset + w[0].length > w[1].offset {
            out.push(
                Evidence::new(format!("partitions {} and {}", w[0].number, w[1].number))
                    .at(at(w[1].offset / sector))
                    .expected("not overlapping")
                    .found(format!(
                        "partition {} ends at LBA {}, partition {} starts at LBA {}",
                        w[0].number,
                        (w[0].offset + w[0].length) / sector - 1,
                        w[1].number,
                        w[1].offset / sector
                    )),
            );
        }
    }
    out
}

/// `fs.ntfs`: the NTFS boot sector at byte `offset` of `dev` (a partition
/// of `length` bytes) and its copy in the partition's last sector. `None`
/// when there is no NTFS boot sector. `label` names the partition in the
/// summary ("p2"), `locate` describes a byte offset of `dev`.
pub fn ntfs_check<R: ReadAt + ?Sized>(
    dev: &R,
    offset: u64,
    length: u64,
    label: &str,
    locate: &dyn Fn(u64) -> String,
) -> Option<Check> {
    const ID: &str = "fs.ntfs";
    let boot = read_vec(dev, offset, 512).ok()?;
    if &boot[3..11] != b"NTFS    " {
        return None;
    }
    let bps = u64::from(u16::from_le_bytes([boot[0x0b], boot[0x0c]]));
    let total = u64::from_le_bytes(boot[0x28..0x30].try_into().unwrap());
    let mut problems = Vec::new();
    if !(bps.is_power_of_two() && (256..=4096).contains(&bps)) || boot[510..512] != [0x55, 0xaa] {
        problems.push(
            Evidence::new("the NTFS boot sector")
                .at(locate(offset))
                .expected("bytes per sector a power of two from 256 to 4096, and 55 aa at 0x1fe")
                .found(format!("{bps} bytes per sector, {}", hex(&boot[510..512], 2))),
        );
    } else if total.checked_mul(bps).is_none_or(|v| v > length) {
        problems.push(
            Evidence::new("the size of the NTFS volume")
                .at(locate(offset))
                .expected(format!("at most the partition's {length} bytes"))
                .found(format!("{total} sectors of {bps} bytes")),
        );
    } else {
        let copy_at = offset + length - bps;
        match read_vec(dev, copy_at, 512) {
            Ok(copy) if copy == boot => {}
            Ok(copy) => {
                let first = copy.iter().zip(&boot).position(|(a, b)| a != b).unwrap_or(0);
                problems.push(
                    Evidence::new("the copy of the NTFS boot sector in the partition's last sector")
                        .at(locate(copy_at))
                        .expected(format!(
                            "the same as the boot sector: {} at byte {first:#x}",
                            hex(&boot[first..(first + 16).min(512)], 16)
                        ))
                        .found(hex(&copy[first..(first + 16).min(512)], 16)),
                );
            }
            Err(e) => problems.push(
                Evidence::new("the copy of the NTFS boot sector")
                    .at(locate(copy_at))
                    .expected("readable")
                    .found(e.to_string()),
            ),
        }
    }
    if problems.is_empty() {
        return Some(
            Check::ok(
                ID,
                format!("{label}: the NTFS boot sector and its copy in the last sector agree"),
            )
            .truth(Truth::CrossCheck),
        );
    }
    let mut check = Check::new(
        ID,
        Status::Suspect,
        format!("{label}: the NTFS boot sector or its copy does not check out"),
    )
    .truth(Truth::CrossCheck)
    .advice(
        "NTFS keeps a copy of its boot sector in the last sector of the partition. If Windows \
         shows this volume as healthy, the end of the space is probably read wrongly: please \
         report it with the bundle of spaces check --bundle. chkdsk repairs a damaged boot sector.",
    );
    for e in problems {
        check = check.evidence(e);
    }
    Some(check)
}

/// A file system hook that checks NTFS only (the `refs` crate checks ReFS).
pub fn ntfs_hook<D: ReadAt>(reader: &SpaceReader<'_, D>, part: &Partition, names: &[String]) -> Vec<Check> {
    let label = if part.number == 0 {
        "the space".to_string()
    } else {
        format!("p{}", part.number)
    };
    ntfs_check(reader, part.offset, part.length, &label, &|o| reader.describe(o, names))
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::path::Path;

    use super::*;
    use crate::format::{ExtentRecord, SpaceRole};
    use crate::io::{MemDevice, SparseImage};
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

    #[test]
    fn a_slab_mapped_twice_fails_the_space_with_both_extents_named() {
        let disks = images("fixtures/mirror3");
        let mut pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let user = pool.user_spaces().next().unwrap().id();
        let other = *pool.spaces.keys().find(|&&id| id != user).unwrap();
        let mut twice = pool.spaces[&user].extents[0];
        twice.space_id = other;
        pool.spaces.get_mut(&other).unwrap().extents.push(twice);
        let r = &reports(&pool, &Options::default())[0];
        let layout = check(r, "space.layout");
        assert_eq!(layout.status, Status::Failed, "{}", r.to_text());
        let found = layout.evidence[0].found.as_deref().unwrap();
        assert!(
            found.contains("is mapped twice") && found.contains(&format!("space {other}")),
            "{found}"
        );
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

    /// A disk of `lbas` sectors of 512 bytes with a protective MBR and both
    /// copies of a GPT holding `parts` (first and last LBA).
    fn gpt_disk(lbas: u64, parts: &[(u64, u64)]) -> MemDevice {
        let mut d = vec![0u8; lbas as usize * 512];
        d[446 + 4] = 0xee;
        d[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
        d[446 + 12..446 + 16].copy_from_slice(&((lbas - 1) as u32).to_le_bytes());
        d[510] = 0x55;
        d[511] = 0xaa;
        let mut entries = vec![0u8; 128 * 128];
        for (i, &(first, last)) in parts.iter().enumerate() {
            let e = &mut entries[i * 128..(i + 1) * 128];
            // Basic data partition (mixed-endian ebd0a0a2-b9e5-4433-87c0-68b6b72699c7).
            e[0..16].copy_from_slice(&[
                0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99, 0xc7,
            ]);
            e[16..32].fill(i as u8 + 1);
            e[32..40].copy_from_slice(&first.to_le_bytes());
            e[40..48].copy_from_slice(&last.to_le_bytes());
        }
        let entries_crc = crc32(&entries);
        let header = |my: u64, alternate: u64, entries_lba: u64| {
            let mut h = vec![0u8; 92];
            h[0..8].copy_from_slice(b"EFI PART");
            h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
            h[12..16].copy_from_slice(&92u32.to_le_bytes());
            h[24..32].copy_from_slice(&my.to_le_bytes());
            h[32..40].copy_from_slice(&alternate.to_le_bytes());
            h[40..48].copy_from_slice(&34u64.to_le_bytes());
            h[48..56].copy_from_slice(&(lbas - 34).to_le_bytes());
            h[56..72].fill(0x5a);
            h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
            h[80..84].copy_from_slice(&128u32.to_le_bytes());
            h[84..88].copy_from_slice(&128u32.to_le_bytes());
            h[88..92].copy_from_slice(&entries_crc.to_le_bytes());
            let crc = crc32(&h);
            h[16..20].copy_from_slice(&crc.to_le_bytes());
            h
        };
        let put = |d: &mut Vec<u8>, lba: u64, bytes: &[u8]| {
            d[lba as usize * 512..lba as usize * 512 + bytes.len()].copy_from_slice(bytes);
        };
        put(&mut d, 1, &header(1, lbas - 1, 2));
        put(&mut d, 2, &entries);
        put(&mut d, lbas - 33, &entries);
        put(&mut d, lbas - 1, &header(lbas - 1, 1, lbas - 33));
        MemDevice(d)
    }

    fn table(d: &MemDevice) -> (Check, Option<Vec<Partition>>) {
        partition_check(d, d.0.len() as u64, 512, &|lba| format!("LBA {lba}"))
    }

    const LBAS: u64 = 8192;

    #[test]
    fn both_copies_of_a_gpt_agree() {
        let (check, parts) = table(&gpt_disk(LBAS, &[(34, 2047), (2048, 8000)]));
        assert_eq!(check.status, Status::Ok, "{check:?}");
        assert_eq!(check.id, "space.partitions");
        assert!(
            check
                .summary
                .starts_with("GPT with 2 partition(s), the primary and the backup agree")
        );
        let parts = parts.unwrap();
        assert_eq!(
            (parts[1].number, parts[1].offset, parts[1].length),
            (2, 2048 * 512, 5953 * 512)
        );
    }

    #[test]
    fn a_backup_header_that_is_not_there_is_suspect() {
        // What 1.1.0 read at the end of spaces with 4 KiB sectors and a
        // write-back cache: zeros where the backup GPT header is.
        let mut d = gpt_disk(LBAS, &[(34, 8000)]);
        let last = (LBAS - 1) as usize;
        d.0[last * 512..].fill(0);
        let (check, parts) = table(&d);
        assert_eq!(check.status, Status::Suspect, "{check:?}");
        assert_eq!(check.summary, "the backup GPT header is not valid");
        let e = &check.evidence[0];
        assert_eq!(e.location.as_deref(), Some("LBA 8191"));
        assert!(
            e.expected
                .as_deref()
                .unwrap()
                .starts_with("\"EFI PART\" (the primary at LBA 1 names LBA 8191")
        );
        assert!(e.found.as_deref().unwrap().ends_with("(all zero)"), "{e:?}");
        assert_eq!(check.truth, Some(Truth::CrossCheck));
        // The primary's partitions are still checked.
        assert_eq!(parts.unwrap().len(), 1);
    }

    #[test]
    fn copies_of_the_entries_that_differ_are_suspect() {
        let mut d = gpt_disk(LBAS, &[(34, 8000)]);
        d.0[((LBAS - 33) * 512 + 32) as usize] ^= 1;
        let (check, _) = table(&d);
        assert_eq!(check.status, Status::Suspect, "{check:?}");
        let e = check
            .evidence
            .iter()
            .find(|e| e.what == "the backup partition entries")
            .unwrap_or_else(|| panic!("{check:?}"));
        assert_eq!(e.location.as_deref(), Some("LBA 8159"));
    }

    #[test]
    fn a_header_whose_crc_does_not_match_is_suspect() {
        let mut d = gpt_disk(LBAS, &[(34, 8000)]);
        d.0[512 + 0x28] ^= 1; // the primary's first usable LBA
        let (check, _) = table(&d);
        assert_eq!(check.status, Status::Suspect, "{check:?}");
        assert_eq!(check.evidence[0].what, "the primary GPT header's CRC32");
    }

    #[test]
    fn a_backup_short_of_the_end_is_a_warning() {
        // The space grew after it was partitioned.
        let mut d = gpt_disk(LBAS, &[(34, 4000)]);
        d.0.resize(2 * LBAS as usize * 512, 0);
        let (check, _) = table(&d);
        assert_eq!(check.status, Status::Warning, "{check:?}");
        assert_eq!(
            check.evidence[0].expected.as_deref(),
            Some("16383, the last LBA of the space")
        );
    }

    #[test]
    fn partitions_that_overlap_or_leave_the_usable_area_are_suspect() {
        let (check, _) = table(&gpt_disk(LBAS, &[(34, 3000), (2048, 8000)]));
        assert_eq!(check.status, Status::Suspect, "{check:?}");
        assert!(
            check.evidence.iter().any(|e| e.what == "partitions 1 and 2"),
            "{check:?}"
        );
        let (check, _) = table(&gpt_disk(LBAS, &[(34, LBAS - 1)]));
        assert_eq!(check.status, Status::Suspect, "{check:?}");
        assert_eq!(check.evidence[0].expected.as_deref(), Some("inside LBAs 34..=8158"));
    }

    #[test]
    fn a_gpt_without_a_protective_mbr_is_read() {
        let mut d = gpt_disk(LBAS, &[(34, 8000)]);
        d.0[..512].fill(0);
        let (check, parts) = table(&d);
        assert_eq!(check.status, Status::Ok, "{check:?}");
        assert!(check.summary.contains("no protective MBR"), "{}", check.summary);
        assert_eq!(parts.unwrap().len(), 1);
    }

    #[test]
    fn mbr_partitions_are_checked_and_no_table_checks_the_whole_space() {
        let mut d = MemDevice(vec![0u8; LBAS as usize * 512]);
        let (check, parts) = table(&d);
        assert_eq!(check.status, Status::Skipped);
        assert!(parts.is_none());
        for (i, (start, count)) in [(2048u32, 2048u32), (4096, 2048)].into_iter().enumerate() {
            let e = 446 + i * 16;
            d.0[e + 4] = 0x07;
            d.0[e + 8..e + 12].copy_from_slice(&start.to_le_bytes());
            d.0[e + 12..e + 16].copy_from_slice(&count.to_le_bytes());
        }
        d.0[510] = 0x55;
        d.0[511] = 0xaa;
        let (check, parts) = table(&d);
        assert_eq!(check.status, Status::Ok, "{check:?}");
        assert_eq!(check.summary, "MBR with 2 partition(s)");
        assert_eq!(parts.unwrap()[1].offset, 4096 * 512);
        d.0[446 + 16 + 8..446 + 16 + 12].copy_from_slice(&3000u32.to_le_bytes());
        let (check, _) = table(&d);
        assert_eq!(check.status, Status::Suspect, "{check:?}");
    }

    /// An NTFS volume filling a partition of `sectors` sectors of 512
    /// bytes, with the copy of its boot sector in the last one.
    fn ntfs_partition(sectors: u64) -> MemDevice {
        let mut d = vec![0u8; sectors as usize * 512];
        d[0..3].copy_from_slice(&[0xeb, 0x52, 0x90]);
        d[3..11].copy_from_slice(b"NTFS    ");
        d[0x0b..0x0d].copy_from_slice(&512u16.to_le_bytes());
        d[0x0d] = 8;
        d[0x28..0x30].copy_from_slice(&(sectors - 1).to_le_bytes());
        d[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
        d[510] = 0x55;
        d[511] = 0xaa;
        let boot = d[..512].to_vec();
        let last = (sectors as usize - 1) * 512;
        d[last..last + 512].copy_from_slice(&boot);
        MemDevice(d)
    }

    fn ntfs(d: &MemDevice) -> Option<Check> {
        ntfs_check(d, 0, d.0.len() as u64, "p2", &|o| format!("byte {o:#x}"))
    }

    #[test]
    fn the_ntfs_boot_sector_and_its_copy() {
        let d = ntfs_partition(4096);
        let check = ntfs(&d).unwrap();
        assert_eq!(check.status, Status::Ok, "{check:?}");
        assert_eq!(
            check.summary,
            "p2: the NTFS boot sector and its copy in the last sector agree"
        );
        let mut other = d.clone();
        other.0[4095 * 512 + 0x30] ^= 1;
        let check = ntfs(&other).unwrap();
        assert_eq!(check.status, Status::Suspect, "{check:?}");
        let e = &check.evidence[0];
        assert_eq!(e.location.as_deref(), Some("byte 0x1ffe00"));
        assert!(e.expected.as_deref().unwrap().contains("at byte 0x30"), "{e:?}");
        let mut zero = d.clone();
        zero.0[4095 * 512..].fill(0);
        assert_eq!(ntfs(&zero).unwrap().status, Status::Suspect);
        let mut large = d.clone();
        large.0[0x28..0x30].copy_from_slice(&5000u64.to_le_bytes());
        let check = ntfs(&large).unwrap();
        assert_eq!(check.status, Status::Suspect);
        assert_eq!(check.evidence[0].what, "the size of the NTFS volume");
        assert!(ntfs(&MemDevice(vec![0u8; 1 << 20])).is_none());
    }
}
