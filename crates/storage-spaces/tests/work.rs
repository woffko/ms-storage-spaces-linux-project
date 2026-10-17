//! Work that grows with what a pool's metadata claims, not with what it
//! holds: a parity journal listing a run of 2^30 stripes, a pool of 10 000
//! user spaces and 40 000 hidden ones, a column with 100 000 out-of-date
//! extents. Each must take seconds at most (in 1.2.1: days, minutes,
//! minutes), and `spaces attach` runs them as root before attaching.

use std::fs::File;
use std::path::Path;
use std::time::{Duration, Instant};

use storage_spaces::format::ExtentRecord;
use storage_spaces::guard::{self, Options};
use storage_spaces::io::SparseImage;
use storage_spaces::isolate::isolated;
use storage_spaces::{Guid, Pool};

fn load(rel: &str) -> Pool<SparseImage> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join(rel);
    let disks: Vec<SparseImage> = (0..)
        .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
        .map(|f| SparseImage::read_from(f).unwrap())
        .collect();
    Pool::open(disks).unwrap()
}

/// Runs `work` within `limit` (it must not hang or panic), and says how
/// long it took.
fn timed<T: Send + 'static>(what: &str, limit: u64, work: impl FnOnce() -> T + Send + 'static) -> T {
    let start = Instant::now();
    let out = isolated(Some(Duration::from_secs(limit)), work).unwrap_or_else(|s| panic!("{what}: {s}"));
    eprintln!("{what}: {:?}", start.elapsed());
    out
}

/// A parity space's extent run grown to 2^20 rows on a disk not at hand,
/// whose journal entry covers only the stripes the run had: every stripe
/// after them counts as not recorded consistent. The check counts them as
/// a range and reads within its budget.
#[test]
fn a_journal_listing_billions_of_stripes_is_counted_not_walked() {
    let mut pool = load("scenarios/m5pj2/s5");
    let id = pool.user_spaces().next().unwrap().id();
    let before = pool
        .open_space(id)
        .unwrap()
        .check_unclean_parity(Some(1 << 20))
        .unwrap()
        .unwrap();
    let (disk, columns, interleave) = {
        let space = pool.spaces.get_mut(&id).unwrap();
        let columns = space.info.policy.unwrap().columns;
        let interleave = space.info.policy.unwrap().interleave;
        let data = columns - space.info.policy.unwrap().redundancy;
        let e = space.extents.iter_mut().find(|e| e.virtual_slab == 0).unwrap();
        e.slab_count = 1 << 20;
        e.physical_slab = 1 << 31;
        space.info.size = Some((1u64 << 20) * data * (256 << 20));
        (e.disk_id, columns, interleave)
    };
    pool.disks.get_mut(&disk).unwrap().member = None;
    let check = timed("journal check", 20, move || {
        pool.open_space(id)
            .unwrap()
            .check_unclean_parity(Some(1 << 30))
            .unwrap()
            .unwrap()
    });
    // The run's stripes past its entry: 2^20 rows of 256 MiB, a stripe
    // per interleave, minus those the entry covers.
    let stripes = (1u64 << 20) * ((256 << 20) / interleave);
    assert!(check.listed > before.listed + stripes / 2, "{check:?}");
    assert!(check.incomplete, "{check:?}");
    assert!(
        check.checked + check.unreadable <= (1 << 30) / (columns * interleave),
        "{check:?}"
    );
}

/// The same run grown to 2^32 rows: a space of 2^61 bytes, past the
/// plausible size of one (`limits`), is refused at once instead of walked.
#[test]
fn a_journal_run_of_2_32_rows_is_refused_at_once() {
    let mut pool = load("scenarios/m5pj2/s5");
    let id = pool.user_spaces().next().unwrap().id();
    let disk = {
        let space = pool.spaces.get_mut(&id).unwrap();
        let data = space.info.policy.unwrap().columns - space.info.policy.unwrap().redundancy;
        let e = space.extents.iter_mut().find(|e| e.virtual_slab == 0).unwrap();
        e.slab_count = 1 << 32;
        e.physical_slab = 1 << 31;
        space.info.size = Some((1u64 << 32).saturating_mul(data * (256 << 20)));
        e.disk_id
    };
    pool.disks.get_mut(&disk).unwrap().member = None;
    let refused = timed("journal check, 2^32 rows", 20, move || {
        pool.open_space(id)
            .and_then(|r| r.check_unclean_parity(Some(1 << 30)))
            .err()
            .map(|e| e.to_string())
    });
    let e = refused.expect("refused");
    assert!(e.contains("beyond 1 EiB"), "{e}");
}

/// 10 000 user spaces and 40 000 hidden spaces under one of them: the
/// reports of every space, and a family, in seconds.
#[test]
fn many_spaces_are_checked_in_seconds() {
    let mut pool = load("fixtures/simple1c");
    let template = pool.user_spaces().next().unwrap().clone();
    let first = template.id();
    let before = pool.family(first).len();
    for i in 0..10_000u64 {
        let mut s = template.clone();
        s.info.id = 1_000 + i;
        s.info.guid = Guid([&[0x5a; 8][..], &s.info.id.to_be_bytes()].concat().try_into().unwrap());
        s.info.name = format!("u{i}");
        s.extents.clear();
        pool.spaces.insert(s.id(), s);
    }
    let child = pool
        .spaces
        .values()
        .find(|s| s.info.is_child)
        .cloned()
        .unwrap_or(template.clone());
    for i in 0..40_000u64 {
        let mut s = child.clone();
        s.info.id = 1_000_000 + i;
        s.info.is_child = true;
        s.info.parent = Some(first);
        s.info.role = storage_spaces::format::SpaceRole::Other(0x77);
        s.extents.clear();
        pool.spaces.insert(s.id(), s);
    }
    let pool = std::sync::Arc::new(pool);
    let p = std::sync::Arc::clone(&pool);
    let family = timed("family", 10, move || p.family(first).len());
    assert_eq!(family, before + 40_000);
    let reports = timed("reports", 60, move || {
        let options = Options::default();
        guard::pool_reports(&pool, &[], &options, &mut guard::ntfs_hook).len()
    });
    assert_eq!(reports, 10_001);
}

/// A mirror column with 100 000 out-of-date extents (a disk that missed
/// the writes of a large space): its health in a moment (1.2.1: 20 s).
#[test]
fn thousands_of_out_of_date_extents_are_looked_up_not_scanned() {
    let mut pool = load("fixtures/mirror2");
    let id = pool.user_spaces().next().unwrap().id();
    {
        let space = pool.spaces.get_mut(&id).unwrap();
        let e: ExtentRecord = space.extents[0];
        for i in 0..100_000u64 {
            let mut stale = e;
            stale.copy = 1;
            stale.virtual_slab = 2 * i;
            stale.slab_count = 1;
            stale.physical_slab = (1 << 20) + i;
            stale.stale_marker = 7;
            space.extents.push(stale);
        }
    }
    let h = timed("health", 5, move || {
        storage_spaces::health::health(&pool).map(|h| h.spaces.len())
    });
    assert_eq!(h.unwrap(), 1);
}
