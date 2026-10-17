//! Pool metadata whose numbers lie far beyond anything Windows writes
//! (records carry no checksum, and each number is a varint of up to 64
//! bits): the placement policy of a space or of its hidden spaces, a
//! space's size or a tier's range, and the copy, column and slab numbers of
//! extents; and the offsets the header of a write-back cache or parity
//! journal names (CRC sealed). Each of {0, 1, 2^31, 2^32, 2^36, 2^56, 2^63,
//! 2^64 - 1} in each field, on every fixture: opening the pool, its health,
//! the guard's report, opening a space, describing and reading it end
//! within 5 s, never in a panic or a loop, and a value beyond the limits of
//! `storage_spaces::limits` (or 2^31 and more of a number of columns, copies
//! or slabs) fails the space that holds it.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use storage_spaces::Pool;
use storage_spaces::format::SpaceRole;
use storage_spaces::guard::{self, Options};
use storage_spaces::io::{ReadAt, SparseImage};
use storage_spaces::isolate::isolated;
use storage_spaces::layout::Layout;
use storage_spaces::limits::{MAX_COPIES, MAX_LOG_OFFSET, MAX_SPACE_SIZE};
use storage_spaces::report::Verdict;

fn load(name: &str) -> Vec<SparseImage> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    (0..)
        .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
        .map(|f| SparseImage::read_from(f).unwrap())
        .collect()
}

/// Every fixture: its name and disks.
fn fixtures() -> Vec<(String, Vec<SparseImage>)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names.into_iter().map(|n| (n.clone(), load(&n))).collect()
}

/// A number of the metadata.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Field {
    Redundancy,
    Copies,
    Groups,
    Columns,
    Size,
    RangeStart,
    RangeLength,
    ExtentCopy,
    ExtentColumn,
    SlabCount,
    VirtualSlab,
    PhysicalSlab,
    CacheSlots,
    CacheCheckpoints,
    CacheData,
    JournalSlots,
    JournalCheckpoints,
    JournalRunSize,
}

const FIELDS: [Field; 18] = [
    Field::Redundancy,
    Field::Copies,
    Field::Groups,
    Field::Columns,
    Field::Size,
    Field::RangeStart,
    Field::RangeLength,
    Field::ExtentCopy,
    Field::ExtentColumn,
    Field::SlabCount,
    Field::VirtualSlab,
    Field::PhysicalSlab,
    Field::CacheSlots,
    Field::CacheCheckpoints,
    Field::CacheData,
    Field::JournalSlots,
    Field::JournalCheckpoints,
    Field::JournalRunSize,
];

const VALUES: [u64; 8] = [0, 1, 1 << 31, 1 << 32, 1 << 36, 1 << 56, 1 << 63, u64::MAX];

impl Field {
    /// A field of the 0x60-byte header of a write-back cache or parity
    /// journal (its CRC at 0x24): the role of its hidden space, its
    /// signature and the field's byte.
    fn header(self) -> Option<(SpaceRole, &'static [u8], usize)> {
        let cache = (SpaceRole::Cache, &b"SPCACHE\0"[..]);
        let journal = (SpaceRole::Other(0x0a), &b"SPVDT\0\0\0"[..]);
        let ((role, signature), at) = match self {
            Field::CacheSlots => (cache, 0x30),
            Field::CacheCheckpoints => (cache, 0x40),
            Field::CacheData => (cache, 0x50),
            Field::JournalSlots => (journal, 0x30),
            Field::JournalCheckpoints => (journal, 0x40),
            Field::JournalRunSize => (journal, 0x50),
            _ => return None,
        };
        Some((role, signature, at))
    }

    /// Whether `value` lies far beyond what any pool holds in this field
    /// (beyond a limit, or 2^31 and more of a number of columns, copies or
    /// slabs); `None` for the fields where no value need fail a space.
    fn beyond(self, value: u64) -> Option<bool> {
        Some(match self {
            Field::Redundancy | Field::Copies | Field::Groups | Field::Columns | Field::ExtentColumn => {
                value >= 1 << 31
            }
            Field::ExtentCopy => value >= MAX_COPIES,
            Field::Size => value > MAX_SPACE_SIZE,
            Field::SlabCount => value == 0 || value >= 1 << 31,
            Field::VirtualSlab | Field::PhysicalSlab => value >= 1 << 31,
            Field::CacheSlots | Field::CacheData | Field::JournalSlots => value > MAX_LOG_OFFSET,
            _ => return None,
        })
    }
}

/// Sets `field` to `value` in the decoded records of the user spaces (and
/// their extents), or with `all` in those of every space: what decoding
/// any record holding the number gives. The user spaces whose own records
/// or extents changed.
fn set(pool: &mut Pool<SparseImage>, field: Field, value: u64, all: bool) -> Vec<u64> {
    let mut own = Vec::new();
    for space in pool.spaces.values_mut().filter(|s| all || s.is_user()) {
        let mut changed = false;
        let info = &mut space.info;
        if let Some(p) = &mut info.policy {
            let f = match field {
                Field::Redundancy => Some(&mut p.redundancy),
                Field::Copies => Some(&mut p.copies),
                Field::Groups => Some(&mut p.groups),
                Field::Columns => Some(&mut p.columns),
                _ => None,
            };
            if let Some(f) = f {
                *f = value;
                changed = true;
            }
        }
        match field {
            Field::Size if info.size.is_some() => {
                info.size = Some(value);
                changed = true;
            }
            Field::RangeStart => info.range = info.range.map(|(_, len)| (value, len)),
            Field::RangeLength => info.range = info.range.map(|(start, _)| (start, value)),
            _ => {}
        }
        for e in &mut space.extents {
            let f = match field {
                Field::ExtentCopy => &mut e.copy,
                Field::ExtentColumn => &mut e.column,
                Field::SlabCount => &mut e.slab_count,
                Field::VirtualSlab => &mut e.virtual_slab,
                Field::PhysicalSlab => &mut e.physical_slab,
                _ => break,
            };
            *f = value;
            changed = true;
        }
        if changed && space.is_user() {
            own.push(space.id());
        }
    }
    own
}

fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |mut c, &b| {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        c
    })
}

/// Sets header field `field` to `value` on every copy at hand of the
/// header of each write-back cache or parity journal of the user spaces of
/// `disks`, the CRC sealed again: the user spaces whose header changed.
fn set_header(disks: &mut [SparseImage], field: Field, value: u64) -> Vec<u64> {
    let (role, signature, at) = field.header().unwrap();
    let places: Vec<(u64, usize, u64)> = {
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let mut places = Vec::new();
        for user in pool.user_spaces() {
            for container in pool.children(user.id()).filter(|c| c.info.role == role) {
                for child in pool.children(container.id()).filter(|c| !c.extents.is_empty()) {
                    let Some(policy) = child.info.policy else { continue };
                    let base = child.info.range.map_or(0, |(start, _)| start);
                    let Ok(l) = Layout::with_base(&policy, &child.extents, base) else {
                        continue;
                    };
                    let loc = l.locate(base);
                    places.extend(l.copies_of(loc.column).into_iter().filter_map(|copy| {
                        let (disk, slab) = l.physical(loc.column, copy, loc.row)?;
                        let (device, start) = pool.slab_location(disk, slab).ok()??;
                        Some((user.id(), device, start + loc.offset_in_slab))
                    }));
                }
            }
        }
        places
    };
    let mut own = Vec::new();
    for (user, device, offset) in places {
        let mut header = vec![0u8; 0x60];
        disks[device].read_exact_at(&mut header, offset).unwrap();
        // (A fixture holds the copies reading needs.)
        if !header.starts_with(signature) {
            continue;
        }
        header[at..at + 8].copy_from_slice(&value.to_le_bytes());
        header[0x24..0x28].fill(0);
        let crc = crc32(&header);
        header[0x24..0x28].copy_from_slice(&crc.to_le_bytes());
        disks[device].insert(offset, &header);
        if !own.contains(&user) {
            own.push(user);
        }
    }
    own
}

/// Per field: the cases run, those on fixtures without what it is in, the
/// slowest case and its time.
type Seen = BTreeMap<String, (u64, u64, Duration, String)>;

/// What `spaces attach`, `info` and the servers do with a pool, up to its
/// data: the verdict of each user space.
fn exercise(pool: Pool<SparseImage>) -> Vec<(u64, Verdict, String)> {
    let _ = storage_spaces::health::health(&pool);
    for s in pool.spaces.values() {
        let _ = s.allocated();
    }
    let mut verdicts = Vec::new();
    let options = Options {
        journal_budget: Some(1 << 20),
    };
    for space in pool.user_spaces() {
        let r = guard::space_report(&pool, space, &[], &options, &mut guard::ntfs_hook);
        verdicts.push((space.id(), r.verdict(), r.to_text()));
        let Ok(reader) = pool.open_space(space.id()) else {
            continue;
        };
        let _ = reader.condition();
        let _ = reader.segments();
        let size = reader.size();
        let mut buf = vec![0u8; size.min(64 << 10) as usize];
        for at in [0, size / 2, size - buf.len() as u64] {
            let _ = reader.describe(at, &[]);
            let _ = reader.read_exact_at(&mut buf, at);
        }
        let _ = reader.check_unclean_parity(Some(1 << 20));
    }
    verdicts
}

/// Every number of the list in every field, of the user spaces and of all
/// spaces, on every fixture: no panic, no loop, and a user space whose own
/// policy, extents or log header hold a number far beyond any pool's
/// fails.
#[test]
fn numbers_beyond_any_pool_are_refused_not_computed_with() {
    let fixtures = fixtures();
    assert!(fixtures.len() >= 79, "{} fixtures", fixtures.len());
    let mut cases = Vec::new();
    for f in 0..fixtures.len() {
        for field in FIELDS {
            for value in VALUES {
                for all in [false, true] {
                    if !(all && field.header().is_some()) {
                        cases.push((f, field, value, all));
                    }
                }
            }
        }
    }
    let limit = Duration::from_secs(5);
    let next = AtomicUsize::new(0);
    let failures = Mutex::new(Vec::new());
    let seen: Mutex<Seen> = Mutex::default();
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get().min(16));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while let Some(&(f, field, value, all)) = cases.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let (name, disks) = &fixtures[f];
                    let case = format!("{name}: {field:?} = {value:#x}{}", if all { " (all)" } else { "" });
                    let mut disks = disks.clone();
                    let (own, pool) = if field.header().is_some() {
                        let own = set_header(&mut disks, field, value);
                        (own, Pool::open(disks))
                    } else {
                        match Pool::open(disks) {
                            Ok(mut pool) => (set(&mut pool, field, value, all), Ok(pool)),
                            Err(e) => (Vec::new(), Err(e)),
                        }
                    };
                    let key = format!("{field:?}");
                    if own.is_empty() && field.header().is_some() {
                        seen.lock().unwrap().entry(key).or_default().1 += 1;
                        continue;
                    }
                    let Ok(pool) = pool else { continue };
                    let started = Instant::now();
                    let out = isolated(Some(limit), move || exercise(pool));
                    let took = started.elapsed();
                    {
                        let mut seen = seen.lock().unwrap();
                        let e = seen.entry(key).or_default();
                        e.0 += 1;
                        if took > e.2 {
                            (e.2, e.3) = (took, case.clone());
                        }
                    }
                    let verdicts = match out {
                        Ok(v) => v,
                        Err(stopped) => {
                            failures.lock().unwrap().push(format!("{case}: {stopped}"));
                            continue;
                        }
                    };
                    let beyond = field.beyond(value) == Some(true) && !all;
                    for (id, verdict, text) in &verdicts {
                        if beyond && own.contains(id) && *verdict != Verdict::Failed {
                            failures
                                .lock()
                                .unwrap()
                                .push(format!("{case}: space {id} {}\n{text}", verdict.as_str()));
                        }
                    }
                }
            });
        }
    });
    let seen = seen.into_inner().unwrap();
    let run: u64 = seen.values().map(|e| e.0).sum();
    eprintln!("{} fixtures, {run} cases ({} workers)", fixtures.len(), workers);
    for (field, (n, without, slowest, case)) in &seen {
        eprintln!("  {field}: {n} cases ({without} skipped: no such header), slowest {slowest:?} ({case})");
    }
    let failures = failures.into_inner().unwrap();
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// A space whose layout is not understood fails alone: the others of its
/// pool are judged on their own. (1.2.0 and 1.2.1 worked out the health
/// of the pool as a whole, and failed every space for one; 1.1.0 attached
/// the others.)
#[test]
fn a_space_not_understood_fails_alone() {
    use storage_spaces::format::Resiliency;
    let mut pool = Pool::open(load("multi")).unwrap();
    let ids: Vec<u64> = pool.user_spaces().map(|s| s.id()).collect();
    assert!(ids.len() >= 2, "{ids:?}");
    let space = pool.spaces.get_mut(&ids[0]).unwrap();
    space.info.policy.as_mut().unwrap().resiliency = Resiliency::Other(9);
    let reports = guard::pool_reports(&pool, &[], &Options::default(), &mut guard::ntfs_hook);
    assert_eq!(reports.len(), ids.len());
    assert_eq!(reports[0].verdict(), Verdict::Failed, "{}", reports[0].to_text());
    for r in &reports[1..] {
        assert_eq!(r.verdict(), Verdict::Healthy, "{}", r.to_text());
    }
    let h = storage_spaces::health::health(&pool).unwrap();
    assert_eq!(h.unknown.len(), 1);
    assert_eq!(h.spaces.len(), ids.len() - 1);
}
