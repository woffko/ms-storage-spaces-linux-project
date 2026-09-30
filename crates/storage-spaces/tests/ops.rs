//! Management operations planned and applied on blank in-memory disks: the
//! results open as clean pools whose spaces read and write, and a crash
//! at any point between steps leaves a pool that opens in the old or the
//! new state.

use storage_spaces::Guid;
use storage_spaces::Pool;
use storage_spaces::io::{DeviceEvent, Overlay, Recorder, SparseImage, WriteAt};
use storage_spaces::ops::{BlankDisk, SpaceSpec, check_pool, plan_create_pool, plan_create_space};

fn guids() -> impl FnMut() -> Guid {
    let mut n = 0u64;
    move || {
        n += 1;
        let mut g = [0u8; 16];
        g[..8].copy_from_slice(&n.to_be_bytes());
        g[8..].copy_from_slice(&0x5a5a_4c69_6e75_7800u64.to_be_bytes());
        Guid(g)
    }
}

fn blank(n: usize) -> (Vec<SparseImage>, Vec<BlankDisk>) {
    let disks = (0..n).map(|_| SparseImage::new(8 << 30)).collect();
    let specs = (0..n)
        .map(|_| BlankDisk {
            size: 8 << 30,
            logical_sector: 512,
            physical_sector: 4096,
            manufacturer: "Linux".into(),
            model: "test".into(),
        })
        .collect();
    (disks, specs)
}

fn spec(name: &str, resiliency: u8, size_mib: u64, thin: bool) -> SpaceSpec {
    SpaceSpec {
        name: name.into(),
        resiliency,
        size: size_mib << 20,
        thin,
        copies: None,
        columns: None,
        interleave: None,
        write_cache: None,
    }
}

/// A pool of three disks gets one space of each kind; after each, the pool
/// opens clean and every space reads back what was written to it.
#[test]
fn created_pools_and_spaces_open_clean_and_work() {
    let mut new_guid = guids();
    let (images, disks) = blank(3);
    let devices: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let (plan, _) = plan_create_pool(&disks, "linuxpool", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &devices.iter().collect::<Vec<_>>())
        .unwrap();
    let pool = Pool::open(devices.iter().collect::<Vec<_>>()).unwrap();
    assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    assert_eq!(pool.name, "linuxpool");
    check_pool(&pool).unwrap();
    drop(pool);
    let kinds = [
        spec("s", 1, 1024, false),
        spec("m", 2, 1024, false),
        spec("p", 3, 2048, false),
        spec("st", 1, 4096, true),
        spec("mt", 2, 4096, true),
        spec("pt", 3, 4096, true),
    ];
    for (k, s) in kinds.iter().enumerate() {
        let pool = Pool::open(devices.iter().collect::<Vec<_>>()).unwrap();
        let (plan, new) = plan_create_space(&pool, s, &mut new_guid).unwrap();
        plan.apply::<_, &Overlay<&SparseImage>>(&devices.iter().collect::<Vec<_>>(), &[])
            .unwrap();
        drop(pool);
        let pool = Pool::open(devices.iter().collect::<Vec<_>>()).unwrap();
        assert!(pool.warnings.is_empty(), "{}: {:?}", s.name, pool.warnings);
        check_pool(&pool).unwrap();
        let space = pool.find_space(&s.name).unwrap();
        assert_eq!(space.info.size, Some(new.size));
        // Every space created so far reads back its own pattern.
        let w = pool.open_space_rw(space.id()).unwrap();
        let data: Vec<u8> = (0..1 << 20).map(|i| (i as u8) ^ (k as u8 * 37)).collect();
        w.write_all_at(&data, 3 << 20).unwrap();
        w.flush().unwrap();
        drop(w);
        let r = pool.open_space(space.id()).unwrap();
        let mut back = vec![0u8; data.len()];
        r.read_exact_at(&mut back, 3 << 20).unwrap();
        assert!(back == data, "{}", s.name);
        // The first sector is zero.
        let mut first = [1u8; 512];
        r.read_exact_at(&mut first, 0).unwrap();
        assert_eq!(first, [0; 512]);
    }
}

/// A crash after any step of creating a space leaves a pool that opens and
/// reads: without the space until a database copy is written, with it once
/// the newest copy has it (Windows' rule), and clean again after the last.
/// Writes after the last flush may land in any subset.
#[test]
fn a_crash_while_creating_a_space_leaves_a_pool_that_opens() {
    let mut new_guid = guids();
    let (images, disks) = blank(3);
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let (plan, _) = plan_create_pool(&disks, "crashpool", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &base.iter().collect::<Vec<_>>())
        .unwrap();
    for (k, s) in [spec("p", 3, 2048, false), spec("m", 2, 1024, true)].iter().enumerate() {
        let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
        let (plan, _) = plan_create_space(&pool, s, &mut new_guid).unwrap();
        drop(pool);
        // Record the plan's writes and flushes.
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let scratch: Vec<Overlay<&Overlay<&SparseImage>>> = base.iter().map(Overlay::new).collect();
        let recorders: Vec<Recorder<&Overlay<&Overlay<&SparseImage>>>> = scratch
            .iter()
            .enumerate()
            .map(|(i, d)| Recorder::new(d, i, log.clone()))
            .collect();
        plan.apply::<_, &Recorder<&Overlay<&Overlay<&SparseImage>>>>(&recorders.iter().collect::<Vec<_>>(), &[])
            .unwrap();
        let events = log.lock().unwrap().clone();
        let mut last_flush = 0;
        let mut rng = 0x9e37_79b9_7f4a_7c15u64 ^ k as u64;
        let (mut without, mut with) = (0, 0);
        for end in 0..=events.len() {
            if end > 0 && matches!(events[end - 1], DeviceEvent::Flush { .. }) {
                last_flush = end;
            }
            // The flushed writes, and a random subset of the rest (four
            // subsets per point, all of them when nothing is pending).
            let pending: Vec<usize> = (last_flush..end)
                .filter(|&i| matches!(events[i], DeviceEvent::Write { .. }))
                .collect();
            for _ in 0..if pending.is_empty() { 1 } else { 4 } {
                let replay: Vec<Overlay<&Overlay<&SparseImage>>> = base.iter().map(Overlay::new).collect();
                for (i, e) in events[..end].iter().enumerate() {
                    if let DeviceEvent::Write { device, offset, data } = e {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        if i < last_flush || rng & 1 == 1 {
                            replay[*device].write_all_at(data, *offset).unwrap();
                        }
                    }
                }
                let pool = Pool::open(replay.iter().collect::<Vec<_>>()).unwrap();
                match pool.find_space(&s.name) {
                    None => without += 1,
                    Some(space) => {
                        with += 1;
                        // The space is complete where the pool shows it.
                        let r = pool.open_space(space.id()).unwrap();
                        let mut first = [1u8; 512];
                        r.read_exact_at(&mut first, 0).unwrap();
                    }
                }
                if end == events.len() {
                    assert!(pool.find_space(&s.name).is_some());
                    check_pool(&pool).unwrap();
                }
                if end == 0 {
                    assert!(pool.warnings.is_empty());
                }
            }
        }
        assert!(without > 0 && with > 0, "{without} {with}");
        // Carry it out for the next space.
        plan.apply::<_, &Overlay<&SparseImage>>(&base.iter().collect::<Vec<_>>(), &[])
            .unwrap();
    }
}

fn data(tag: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31) ^ tag).collect()
}

/// Every space of the pool reads back its data.
fn check_data<D: storage_spaces::io::ReadAt>(pool: &Pool<D>, spaces: &[(&str, u8)]) {
    for (name, tag) in spaces {
        let space = pool.find_space(name).unwrap_or_else(|| panic!("{name} is gone"));
        let r = pool.open_space(space.id()).unwrap();
        let want = data(*tag, 8 << 20);
        let mut got = vec![0u8; want.len()];
        r.read_exact_at(&mut got, 1 << 20).unwrap();
        assert!(got == want, "{name}");
    }
}

/// A disk added, a disk retired (its data moved away) and removed: the
/// data of every space stays readable throughout, also at every point a
/// crash could stop the retirement, and the pool opens clean at the end
/// with the three remaining disks.
#[test]
fn disks_are_added_retired_and_removed() {
    use storage_spaces::ops::{plan_add_disk, plan_remove_disk, plan_retire_disk};
    let mut new_guid = guids();
    let (images, disks) = blank(4);
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let (plan, _) = plan_create_pool(&disks[..3], "lifecycle", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &base[..3].iter().collect::<Vec<_>>())
        .unwrap();
    let spaces = [("s", 1u8), ("m", 2), ("p", 3)];
    for (name, tag) in spaces {
        let pool = Pool::open(base[..3].iter().collect::<Vec<_>>()).unwrap();
        let (plan, _) = plan_create_space(&pool, &spec(name, tag, 1024, false), &mut new_guid).unwrap();
        plan.apply::<_, &Overlay<&SparseImage>>(&base[..3].iter().collect::<Vec<_>>(), &[])
            .unwrap();
        drop(pool);
        let pool = Pool::open(base[..3].iter().collect::<Vec<_>>()).unwrap();
        let w = pool.open_space_rw(pool.find_space(name).unwrap().id()).unwrap();
        w.write_all_at(&data(tag, 8 << 20), 1 << 20).unwrap();
        w.flush().unwrap();
    }
    // Add the fourth disk.
    let pool = Pool::open(base[..3].iter().collect::<Vec<_>>()).unwrap();
    let plan = plan_add_disk(&pool, &disks[3], &mut new_guid).unwrap();
    drop(pool);
    plan.apply(&base[..3].iter().collect::<Vec<_>>(), &[&base[3]]).unwrap();
    let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
    assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    assert_eq!(pool.members.len(), 4);
    check_pool(&pool).unwrap();
    check_data(&pool, &spaces);
    // Retire disk 1, recording every write, and replay the crash points.
    let first = pool.disks.values().find(|d| d.member == Some(0)).unwrap().id;
    let plan = plan_retire_disk(&pool, first).unwrap();
    drop(pool);
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let scratch: Vec<Overlay<&Overlay<&SparseImage>>> = base.iter().map(Overlay::new).collect();
    let recorders: Vec<Recorder<&Overlay<&Overlay<&SparseImage>>>> = scratch
        .iter()
        .enumerate()
        .map(|(i, d)| Recorder::new(d, i, log.clone()))
        .collect();
    plan.apply::<_, &Recorder<&Overlay<&Overlay<&SparseImage>>>>(&recorders.iter().collect::<Vec<_>>(), &[])
        .unwrap();
    let events = log.lock().unwrap().clone();
    for end in (0..=events.len()).filter(|&e| e == 0 || matches!(events[e - 1], DeviceEvent::Flush { .. })) {
        let replay: Vec<Overlay<&Overlay<&SparseImage>>> = base.iter().map(Overlay::new).collect();
        for e in &events[..end] {
            if let DeviceEvent::Write { device, offset, data } = e {
                replay[*device].write_all_at(data, *offset).unwrap();
            }
        }
        let pool = Pool::open(replay.iter().collect::<Vec<_>>()).unwrap();
        check_data(&pool, &spaces);
    }
    // Carry it out, then remove the disk.
    plan.apply::<_, &Overlay<&SparseImage>>(&base.iter().collect::<Vec<_>>(), &[])
        .unwrap();
    let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
    assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    check_pool(&pool).unwrap();
    check_data(&pool, &spaces);
    assert!(
        pool.spaces
            .values()
            .all(|s| s.info.role == storage_spaces::format::SpaceRole::Metadata
                || s.extents.iter().all(|e| e.disk_id != first))
    );
    let plan = plan_remove_disk(&pool, first).unwrap();
    drop(pool);
    plan.apply::<_, &Overlay<&SparseImage>>(&base.iter().collect::<Vec<_>>(), &[])
        .unwrap();
    let pool = Pool::open(base[1..].iter().collect::<Vec<_>>()).unwrap();
    assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    assert_eq!(pool.members.len(), 3);
    check_pool(&pool).unwrap();
    check_data(&pool, &spaces);
    // The removed disk is no member any more.
    assert!(Pool::open(vec![&base[0]]).is_err());
}

/// A disk lost: the repair rebuilds its copies of a mirror and a parity
/// space on the other disks (the parity column as the XOR of the others),
/// the data readable at every point a crash could stop it; afterwards the
/// missing disk is removed and the pool is clean again.
#[test]
fn a_lost_disk_is_repaired_away_and_removed() {
    use storage_spaces::ops::{plan_remove_disk, plan_repair};
    let mut new_guid = guids();
    let (images, disks) = blank(5);
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let (plan, _) = plan_create_pool(&disks, "repair", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &base.iter().collect::<Vec<_>>())
        .unwrap();
    let spaces = [("m", 2u8), ("p", 3)];
    for (name, tag) in spaces {
        let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
        let mut s = spec(name, tag, 1024, false);
        s.columns = Some(if tag == 2 { 1 } else { 3 });
        let (plan, _) = plan_create_space(&pool, &s, &mut new_guid).unwrap();
        plan.apply::<_, &Overlay<&SparseImage>>(&base.iter().collect::<Vec<_>>(), &[])
            .unwrap();
        drop(pool);
        let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
        let w = pool.open_space_rw(pool.find_space(name).unwrap().id()).unwrap();
        w.write_all_at(&data(tag, 8 << 20), 1 << 20).unwrap();
        w.flush().unwrap();
    }
    // The disk holding the most of the spaces is lost.
    let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
    let busiest = (0..5)
        .max_by_key(|&d| {
            let id = pool.disks.values().find(|x| x.member == Some(d)).unwrap().id;
            pool.user_spaces()
                .flat_map(|s| s.extents.iter())
                .filter(|e| e.disk_id == id)
                .count()
        })
        .unwrap();
    let lost = pool.disks.values().find(|x| x.member == Some(busiest)).unwrap().id;
    drop(pool);
    let left: Vec<&Overlay<&SparseImage>> = base
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != busiest)
        .map(|(_, d)| d)
        .collect();
    let pool = Pool::open(left.clone()).unwrap();
    assert!(pool.warnings.iter().any(|w| w.ends_with("is missing")));
    check_data(&pool, &spaces);
    let plan = plan_repair(&pool).unwrap();
    assert!(plan.steps.iter().any(|s| {
        s.actions
            .iter()
            .any(|a| matches!(a, storage_spaces::plan::Action::Xor { .. }))
    }));
    drop(pool);
    // Every crash point of the repair.
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let scratch: Vec<Overlay<&Overlay<&SparseImage>>> = left.iter().map(|d| Overlay::new(*d)).collect();
    let recorders: Vec<Recorder<&Overlay<&Overlay<&SparseImage>>>> = scratch
        .iter()
        .enumerate()
        .map(|(i, d)| Recorder::new(d, i, log.clone()))
        .collect();
    plan.apply::<_, &Recorder<&Overlay<&Overlay<&SparseImage>>>>(&recorders.iter().collect::<Vec<_>>(), &[])
        .unwrap();
    let events = log.lock().unwrap().clone();
    for end in (0..=events.len()).filter(|&e| e == 0 || matches!(events[e - 1], DeviceEvent::Flush { .. })) {
        let replay: Vec<Overlay<&Overlay<&SparseImage>>> = left.iter().map(|d| Overlay::new(*d)).collect();
        for e in &events[..end] {
            if let DeviceEvent::Write { device, offset, data } = e {
                replay[*device].write_all_at(data, *offset).unwrap();
            }
        }
        let pool = Pool::open(replay.iter().collect::<Vec<_>>()).unwrap();
        check_data(&pool, &spaces);
    }
    plan.apply::<_, &Overlay<&SparseImage>>(&left, &[]).unwrap();
    let pool = Pool::open(left.clone()).unwrap();
    for space in pool.user_spaces() {
        assert_eq!(
            pool.open_space(space.id()).unwrap().condition(),
            storage_spaces::Condition::Healthy,
            "{}",
            space.name()
        );
    }
    check_data(&pool, &spaces);
    // Nothing left to repair; the missing disk goes.
    assert!(plan_repair(&pool).unwrap().steps.is_empty());
    let plan = plan_remove_disk(&pool, lost).unwrap();
    drop(pool);
    plan.apply::<_, &Overlay<&SparseImage>>(&left, &[]).unwrap();
    let pool = Pool::open(left).unwrap();
    assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    check_pool(&pool).unwrap();
    check_data(&pool, &spaces);
}

/// After a disk is added, optimizing moves extents onto it until the disks
/// hold about as much, the data staying readable; a second optimization
/// finds nothing to do.
#[test]
fn an_added_disk_is_filled_by_optimizing() {
    use storage_spaces::ops::{plan_add_disk, plan_rebalance};
    let mut new_guid = guids();
    let (images, disks) = blank(4);
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let (plan, _) = plan_create_pool(&disks[..3], "optimize", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &base[..3].iter().collect::<Vec<_>>())
        .unwrap();
    let spaces = [("s", 1u8), ("m", 2), ("p", 3)];
    for (name, tag) in spaces {
        let pool = Pool::open(base[..3].iter().collect::<Vec<_>>()).unwrap();
        let (plan, _) = plan_create_space(&pool, &spec(name, tag, 2048, false), &mut new_guid).unwrap();
        plan.apply::<_, &Overlay<&SparseImage>>(&base[..3].iter().collect::<Vec<_>>(), &[])
            .unwrap();
        drop(pool);
        let pool = Pool::open(base[..3].iter().collect::<Vec<_>>()).unwrap();
        let w = pool.open_space_rw(pool.find_space(name).unwrap().id()).unwrap();
        w.write_all_at(&data(tag, 8 << 20), 1 << 20).unwrap();
        w.flush().unwrap();
    }
    let pool = Pool::open(base[..3].iter().collect::<Vec<_>>()).unwrap();
    let plan = plan_add_disk(&pool, &disks[3], &mut new_guid).unwrap();
    drop(pool);
    plan.apply(&base[..3].iter().collect::<Vec<_>>(), &[&base[3]]).unwrap();
    let used = |pool: &Pool<&Overlay<&SparseImage>>| -> Vec<u64> {
        let mut u: Vec<u64> = pool
            .disks
            .keys()
            .map(|d| {
                pool.spaces
                    .values()
                    .flat_map(|s| s.extents.iter())
                    .filter(|e| e.disk_id == *d)
                    .map(|e| e.slab_count)
                    .sum()
            })
            .collect();
        u.sort();
        u
    };
    let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
    let before = used(&pool);
    assert!(before[0] <= 1, "{before:?}");
    let plan = plan_rebalance(&pool).unwrap();
    drop(pool);
    plan.apply::<_, &Overlay<&SparseImage>>(&base.iter().collect::<Vec<_>>(), &[])
        .unwrap();
    let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
    assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    check_pool(&pool).unwrap();
    check_data(&pool, &spaces);
    let after = used(&pool);
    assert!(after[3] - after[0] <= 4, "{before:?} -> {after:?}");
    assert!(plan_rebalance(&pool).unwrap().steps.is_empty());
}

/// Scrubbing finds differences between mirror copies and parity units
/// that do not match: where nothing was being written they are mismatches;
/// in a mirror's extent runs written since Windows last had the pool (its
/// dirty region table lists them) and in parity stripes the journal does
/// not list as consistent (never written) they are unsettled. The repair
/// makes both agree and the data reads back as before.
#[test]
fn scrubbing_finds_differences_and_makes_them_agree() {
    use storage_spaces::layout::Location;
    use storage_spaces::ops::scrub;
    let mut new_guid = guids();
    let (images, disks) = blank(3);
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let members = || base.iter().collect::<Vec<_>>();
    let (plan, _) = plan_create_pool(&disks, "scrub", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &members()).unwrap();
    let spaces = [("m", 2u8), ("p", 3)];
    for (name, tag) in spaces.iter().copied().chain([("quiet", 2)]) {
        let pool = Pool::open(members()).unwrap();
        let (plan, _) = plan_create_space(&pool, &spec(name, tag, 1024, false), &mut new_guid).unwrap();
        plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();
        drop(pool);
        if name == "quiet" {
            continue;
        }
        let pool = Pool::open(members()).unwrap();
        let w = pool.open_space_rw(pool.find_space(name).unwrap().id()).unwrap();
        // Parity stripes 2-17 of the first row (256 KiB units, 2 data columns).
        w.write_all_at(&data(tag, 8 << 20), 1 << 20).unwrap();
        w.destage().unwrap();
        w.flush().unwrap();
    }
    let pool = Pool::open(members()).unwrap();
    let clean = scrub(&pool).unwrap();
    assert_eq!((clean.mismatches, clean.unsettled), (0, 0), "{:?}", clean.lines);
    assert!(clean.plan.steps.is_empty());

    let corrupt = |disk: u64, slab: u64, offset: u64| {
        let (device, at) = pool.slab_location(disk, slab).unwrap().unwrap();
        base[device].write_all_at(&[0xa5; 4096], at + offset).unwrap();
    };
    let space = |name: &str| pool.open_space(pool.find_space(name).unwrap().id()).unwrap();
    // The second copy of both mirrors, in rows 0 and 1: unsettled in the
    // written one, mismatches in the other.
    for name in ["m", "quiet"] {
        let r = space(name);
        let l = r.layout();
        let second = l.copies_of(0)[1];
        for (row, offset) in [(0, 3 << 20), (1, 5 << 20)] {
            let (disk, slab) = l.physical(0, second, row).unwrap();
            corrupt(disk, slab, offset);
        }
    }
    // The parity unit of stripe 8 (written: a mismatch) and of a stripe of
    // row 1 (never written: unsettled).
    let r = space("p");
    let l = r.layout();
    for row in [0, 1] {
        let loc = Location {
            column: 0,
            row,
            offset_in_slab: 2 << 20,
            contiguous: l.interleave,
        };
        let (disk, slab) = l.physical(l.parity_column(l.stripe_of(&loc)), 0, row).unwrap();
        corrupt(disk, slab, 2 << 20);
    }
    drop(r);

    let found = scrub(&pool).unwrap();
    assert_eq!((found.mismatches, found.unsettled), (3, 3), "{:?}", found.lines);
    assert_eq!(found.plan.steps.iter().map(|s| s.actions.len()).sum::<usize>(), 6);
    drop(pool);
    found.plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();
    let pool = Pool::open(members()).unwrap();
    let after = scrub(&pool).unwrap();
    assert_eq!((after.mismatches, after.unsettled), (0, 0), "{:?}", after.lines);
    check_pool(&pool).unwrap();
    check_data(&pool, &spaces);
    let r = pool.open_space(pool.find_space("quiet").unwrap().id()).unwrap();
    let mut row1 = vec![1u8; 1 << 20];
    r.read_exact_at(&mut row1, (256 << 20) + (5 << 20)).unwrap();
    assert!(row1.iter().all(|&b| b == 0));
}

/// Sizes are rounded up to whole rows without wrapping: a size near 2^64
/// once became a space of 0 bytes. Spaces hold at most 2^32 slabs (1 EiB),
/// on creation and when grown.
#[test]
fn huge_sizes_are_refused_not_wrapped() {
    use storage_spaces::ops::{MAX_SPACE_SIZE, plan_resize_space};
    let mut new_guid = guids();
    let (images, disks) = blank(2);
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let members = || base.iter().collect::<Vec<_>>();
    let (plan, _) = plan_create_pool(&disks, "huge", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &members()).unwrap();
    let pool = Pool::open(members()).unwrap();
    for size in [u64::MAX, u64::MAX - 1000, MAX_SPACE_SIZE + 1] {
        let mut s = spec("x", 1, 0, true);
        s.size = size;
        assert!(plan_create_space(&pool, &s, &mut new_guid).is_err(), "{size}");
    }
    let mut s = spec("x", 1, 0, true);
    s.size = MAX_SPACE_SIZE;
    let (_, space) = plan_create_space(&pool, &s, &mut new_guid).unwrap();
    assert_eq!(space.size, MAX_SPACE_SIZE);
    let (plan, _) = plan_create_space(&pool, &spec("x", 1, 1024, false), &mut new_guid).unwrap();
    drop(pool);
    plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();
    let pool = Pool::open(members()).unwrap();
    assert!(plan_resize_space(&pool, "x", u64::MAX).is_err());
}
