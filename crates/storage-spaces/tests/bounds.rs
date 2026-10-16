//! Pool metadata whose numbers lie far beyond anything Windows writes
//! (records carry no checksum, and each number is a varint of up to 64
//! bits): the placement policy of a space or of its hidden spaces, a
//! space's size or a tier's range, and the copy, column and slab numbers of
//! extents. Opening the pool, its health, the guard's report, opening a
//! space, describing and reading it must end in an error or a refusal of
//! that space within seconds: never a panic, never a loop.

use std::fs::File;
use std::path::Path;
use std::time::Duration;

use storage_spaces::Pool;
use storage_spaces::guard::{self, Options};
use storage_spaces::io::SparseImage;
use storage_spaces::isolate::isolated;
use storage_spaces::report::Verdict;

fn load(name: &str) -> Vec<SparseImage> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    (0..)
        .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
        .map(|f| SparseImage::read_from(f).unwrap())
        .collect()
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
}

const FIELDS: [Field; 12] = [
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
];

const VALUES: [u64; 8] = [0, 1, 1 << 31, 1 << 32, 1 << 36, 1 << 56, 1 << 63, u64::MAX];

/// Sets `field` to `value` in the decoded records of the user spaces (and
/// their extents), or with `all` in those of every space: what decoding
/// any record holding the number gives.
fn set(pool: &mut Pool<SparseImage>, field: Field, value: u64, all: bool) {
    for space in pool.spaces.values_mut().filter(|s| all || s.is_user()) {
        let info = &mut space.info;
        if let Some(p) = &mut info.policy {
            match field {
                Field::Redundancy => p.redundancy = value,
                Field::Copies => p.copies = value,
                Field::Groups => p.groups = value,
                Field::Columns => p.columns = value,
                _ => {}
            }
        }
        match field {
            Field::Size if info.size.is_some() => info.size = Some(value),
            Field::RangeStart => info.range = info.range.map(|(_, len)| (value, len)),
            Field::RangeLength => info.range = info.range.map(|(start, _)| (start, value)),
            _ => {}
        }
        for e in &mut space.extents {
            match field {
                Field::ExtentCopy => e.copy = value,
                Field::ExtentColumn => e.column = value,
                Field::SlabCount => e.slab_count = value,
                Field::VirtualSlab => e.virtual_slab = value,
                Field::PhysicalSlab => e.physical_slab = value,
                _ => {}
            }
        }
    }
}

/// What `spaces attach`, `info` and the servers do with a pool, up to its
/// data: the verdict of each user space.
fn exercise(pool: Pool<SparseImage>) -> Vec<(Verdict, String)> {
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
        verdicts.push((r.verdict(), r.to_text()));
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
/// spaces: no panic, no loop, and a user space whose own policy or extents
/// hold a number beyond any pool's is not healthy.
#[test]
fn numbers_beyond_any_pool_are_refused_not_computed_with() {
    for name in [
        "simple1c",
        "mirror3",
        "parity3",
        "lrc12",
        "tiered",
        "wc64",
        "mirrorthin",
        "m4kn",
    ] {
        let disks = load(name);
        for field in FIELDS {
            for value in VALUES {
                for all in [false, true] {
                    let mut pool = Pool::open(disks.clone()).unwrap();
                    // Whether the user spaces' own records or extents change.
                    let own = pool.user_spaces().any(|s| match field {
                        Field::Size | Field::RangeStart | Field::RangeLength => false,
                        Field::ExtentCopy
                        | Field::ExtentColumn
                        | Field::SlabCount
                        | Field::VirtualSlab
                        | Field::PhysicalSlab => !s.extents.is_empty(),
                        _ => true,
                    });
                    set(&mut pool, field, value, all);
                    let case = format!("{name}: {field:?} = {value:#x}{}", if all { " (all)" } else { "" });
                    let verdicts = isolated(Some(Duration::from_secs(10)), move || exercise(pool))
                        .unwrap_or_else(|stopped| panic!("{case}: {stopped}"));
                    if own && value >= 1 << 31 && !all {
                        assert!(
                            verdicts.iter().all(|(v, _)| *v == Verdict::Failed),
                            "{case}: {verdicts:?}"
                        );
                    }
                }
            }
        }
    }
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
