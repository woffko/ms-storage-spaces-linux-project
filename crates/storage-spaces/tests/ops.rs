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
