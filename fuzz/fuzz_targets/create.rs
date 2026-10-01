//! Creating pools and spaces from fuzzed parameters: whatever the planners
//! accept must, once applied to blank disks, open as a clean pool that the
//! management checks accept, with every new space readable and writable
//! at both ends (a thin space's end only while the pool has free slabs);
//! sometimes a tiered space over disks of fuzzed media, which must read.
#![no_main]

use libfuzzer_sys::fuzz_target;
use storage_spaces::io::{Overlay, SparseImage};
use storage_spaces::ops::{
    BlankDisk, SpaceSpec, check_pool, plan_create_pool, plan_create_space, plan_create_tier, plan_create_tiered_space,
    plan_set_disk,
};
use storage_spaces::{Guid, Pool};

fuzz_target!(|data: &[u8]| {
    let mut bytes = data.iter().copied().chain(std::iter::repeat(0));
    let mut next = move || bytes.next().unwrap();
    let mut counter = 0u64;
    let mut guid = move || {
        counter += 1;
        let mut g = [0x5a; 16];
        g[..8].copy_from_slice(&counter.to_le_bytes());
        Guid(g)
    };
    let n = 1 + next() as usize % 8;
    let disks: Vec<BlankDisk> = (0..n)
        .map(|_| {
            let b = next();
            BlankDisk {
                size: (4 + u64::from(b % 61)) << 30 | u64::from(next() % 4) << 20,
                logical_sector: if b & 0x80 != 0 { 4096 } else { 512 },
                physical_sector: 4096,
                manufacturer: "fuzz".into(),
                model: "disk".into(),
            }
        })
        .collect();
    let name: String = (0..next() % 12).map(|_| char::from(b'a' + next() % 26)).collect();
    let logical = match next() % 3 {
        0 => None,
        1 => Some(512),
        _ => Some(4096),
    };
    let Ok((plan, _)) = plan_create_pool(&disks, &name, logical, &mut guid) else { return };
    let images: Vec<Overlay<SparseImage>> = disks.iter().map(|d| Overlay::new(SparseImage::new(d.size))).collect();
    let members: Vec<&Overlay<SparseImage>> = images.iter().collect();
    plan.apply::<&Overlay<SparseImage>, _>(&[], &members).unwrap();
    let pool = Pool::open(members.clone()).unwrap();
    assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    check_pool(&pool).unwrap();
    drop(pool);
    // Sometimes a tiered space: media, two templates, the space.
    if next() & 3 == 0 {
        let apply = |plan: storage_spaces::plan::Plan| plan.apply::<_, &Overlay<SparseImage>>(&members, &[]).unwrap();
        for d in 1..=n as u64 {
            let pool = Pool::open(members.clone()).unwrap();
            let plan = plan_set_disk(&pool, d, Some(1 + next() % 2), None).unwrap();
            drop(pool);
            apply(plan);
        }
        for (name, ssd) in [("fast", true), ("big", false)] {
            let pool = Pool::open(members.clone()).unwrap();
            let columns = (next() & 1 == 0).then(|| 1 + u64::from(next() % 8));
            let resiliency = if ssd { 2 } else { 1 + 2 * (next() % 2) };
            let plan = plan_create_tier(&pool, name, ssd, resiliency, columns, &mut guid).unwrap();
            drop(pool);
            apply(plan);
        }
        let tiers = [
            ("fast".to_owned(), u64::from(next()) << 24 | 1),
            ("big".to_owned(), u64::from(next()) << 26 | 1),
        ];
        let pool = Pool::open(members.clone()).unwrap();
        if let Ok((plan, _)) = plan_create_tiered_space(&pool, "tiered", &tiers, &mut guid) {
            drop(pool);
            apply(plan);
            let pool = Pool::open(members.clone()).unwrap();
            assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
            check_pool(&pool).unwrap();
            let r = pool.open_space(pool.find_space("tiered").unwrap().id()).unwrap();
            assert!(r.size() >= tiers[0].1 + tiers[1].1);
            let mut back = [0u8; 4096];
            for at in [0, r.size() - 4096] {
                r.read_exact_at(&mut back, at).unwrap();
            }
        }
        return;
    }
    for i in 0..3 {
        let spec = SpaceSpec {
            name: format!("s{i}"),
            resiliency: 1 + next() % 3,
            // Mostly up to 16 GiB, sometimes anything (rounding must not wrap).
            size: if next() == 0xff {
                u64::from_le_bytes(std::array::from_fn(|_| next()))
            } else {
                u64::from(next()) << 26 | u64::from(next() % 16) << 20
            },
            thin: next() & 1 == 1,
            copies: [None, Some(2), Some(3)][next() as usize % 3],
            columns: [None, Some(1), Some(2), Some(u64::from(next() % 9))][next() as usize % 4],
            interleave: [None, Some(16 << 10), Some(64 << 10), Some(1 << 20), Some(u64::from(next()) << 12)]
                [next() as usize % 5],
            write_cache: [None, Some(64 << 20), Some(u64::from(next()) << 24)][next() as usize % 3],
        };
        let pool = Pool::open(members.clone()).unwrap();
        let Ok((plan, _)) = plan_create_space(&pool, &spec, &mut guid) else { continue };
        drop(pool);
        plan.apply::<_, &Overlay<SparseImage>>(&members, &[]).unwrap();
        let pool = Pool::open(members.clone()).unwrap();
        assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
        check_pool(&pool).unwrap();
        let space = pool.find_space(&spec.name).expect("the new space is listed");
        let w = pool.open_space_rw(space.id()).unwrap();
        let size = w.size();
        assert!(size >= spec.size, "{size} < {}", spec.size);
        let block = [0xa5u8; 4096];
        for at in [0, size - 4096] {
            match w.write_all_at(&block, at) {
                Ok(()) => {}
                // A row of a thin space not allocated yet needs free slabs.
                Err(e) if spec.thin && e.to_string().contains("the pool is full") => continue,
                Err(e) => panic!("writing at {at:#x}: {e}"),
            }
            let mut back = [0u8; 4096];
            w.read_exact_at(&mut back, at).unwrap();
            assert_eq!(back, block);
        }
        w.flush().unwrap();
    }
});
