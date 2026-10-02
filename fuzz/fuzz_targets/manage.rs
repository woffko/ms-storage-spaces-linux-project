//! Management operations on the committed metadata fixtures (the Windows
//! pools and the Linux-created `c11health`) with byte patches from the
//! input (as in `pool_write`): planning an operation on hostile metadata
//! must not panic, and every action of the plan must write only inside the
//! pool partitions of the members (removing the pool: only its partition
//! tables), or to a disk being added. Plans of up to 64 MiB (the database
//! updates; not moves of whole slabs, which the in-memory disks would
//! have to hold) are also applied and their writes checked.
#![no_main]

use std::fs::File;
use std::path::Path;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use storage_spaces::io::{Overlay, SparseImage};
use storage_spaces::ops::{self, BlankDisk, SpaceSpec};
use storage_spaces::plan::{Action, Target};
use storage_spaces::{Guid, Pool};

fn fixtures() -> &'static Vec<Vec<SparseImage>> {
    static POOLS: OnceLock<Vec<Vec<SparseImage>>> = OnceLock::new();
    POOLS.get_or_init(|| {
        let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/storage-spaces/tests");
        let mut dirs: Vec<_> = std::fs::read_dir(tests.join("fixtures"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        dirs.sort();
        dirs.push(tests.join("scenarios/c11health"));
        dirs.iter()
            .map(|dir| {
                (0..)
                    .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
                    .map(|f| SparseImage::read_from(f).unwrap())
                    .collect()
            })
            .collect()
    })
}

fuzz_target!(|data: &[u8]| {
    let pools = fixtures();
    let [first, op, param, patches @ ..] = data else { return };
    let mut disks = pools[*first as usize % pools.len()].clone();
    let count = disks.len();
    for p in patches.chunks_exact(8) {
        let disk = &mut disks[p[0] as usize % count];
        let ranges = disk.ranges();
        let (start, len) = ranges[u16::from_le_bytes([p[1], p[2]]) as usize % ranges.len()];
        let at = start + u32::from_le_bytes([p[3], p[4], p[5], p[6]]) as u64 % len as u64;
        disk.insert(at, &[p[7]]);
    }
    let sizes: Vec<u64> = disks.iter().map(|d| d.size).collect();
    let disks: Vec<Overlay<SparseImage>> = disks.into_iter().map(Overlay::new).collect();
    let new = [Overlay::new(SparseImage::new(16 << 30))];
    let Ok(pool) = Pool::open(disks.iter().collect::<Vec<_>>()) else {
        return;
    };
    let mut allowed: Vec<Vec<(u64, u64)>> = vec![Vec::new(); count];
    for m in &pool.members {
        allowed[m.device].push((m.partition.offset, m.partition.offset + m.partition.length));
    }
    let mut counter = 0u64;
    let mut guid = move || {
        counter += 1;
        Guid([counter as u8; 16])
    };
    let space = pool
        .user_spaces()
        .nth(usize::from(*param % 4))
        .map(|s| s.name().to_owned());
    let disk_id = pool
        .disks
        .keys()
        .nth(usize::from(*param) % pool.disks.len().max(1))
        .copied();
    let param = u64::from(*param);
    let plan = match op % 15 {
        0 => ops::plan_rename_pool(&pool, &"p".repeat(1 + param as usize % 300)),
        1 => space.map_or(Ok(Default::default()), |s| ops::plan_rename_space(&pool, &s, "renamed")),
        2 => space.map_or(Ok(Default::default()), |s| ops::plan_delete_space(&pool, &s)),
        3 => disk_id.map_or(Ok(Default::default()), |d| {
            ops::plan_set_disk(&pool, d, Some(param as u8 % 4), Some((param >> 2) as u8 % 6))
        }),
        4 => space.map_or(Ok(Default::default()), |s| {
            ops::plan_resize_space(&pool, &s, param << 28)
        }),
        5 => disk_id.map_or(Ok(Default::default()), |d| ops::plan_retire_disk(&pool, d)),
        6 => disk_id.map_or(Ok(Default::default()), |d| ops::plan_remove_disk(&pool, d)),
        7 => ops::plan_repair(&pool),
        8 => ops::plan_rebalance(&pool),
        // Scrubbing reads every slab: only on pools of up to four.
        9 if pool
            .user_spaces()
            .flat_map(|s| &s.extents)
            .map(|e| e.slab_count)
            .sum::<u64>()
            <= 4 =>
        {
            ops::scrub(&pool).map(|s| s.plan)
        }
        9 => return,
        10 => {
            let spec = SpaceSpec {
                name: "fuzzed".into(),
                resiliency: 1 + param as u8 % 3,
                size: (param + 1) << 26,
                thin: param & 8 != 0,
                copies: None,
                columns: None,
                interleave: None,
                write_cache: None,
            };
            ops::plan_create_space(&pool, &spec, &mut guid).map(|(plan, _)| plan)
        }
        11 => {
            let blank = BlankDisk {
                size: 16 << 30,
                logical_sector: 512,
                physical_sector: 4096,
                manufacturer: "fuzz".into(),
                model: "disk".into(),
            };
            ops::plan_add_disk(&pool, &blank, &mut guid)
        }
        13 => ops::plan_create_tier(
            &pool,
            "fuzzed",
            param & 1 == 0,
            1 + (param >> 1) as u8 % 3,
            (param & 16 != 0).then_some(1 + (param >> 5) % 8),
            &mut guid,
        ),
        14 => {
            // Over the pool's first two templates, if it has them.
            let Ok(db) = ops::check_pool(&pool) else { return };
            let Ok(templates) = ops::tier_templates(&db) else {
                return;
            };
            if templates.len() < 2 {
                return;
            }
            let tiers = [
                (templates[0].name.clone(), (param + 1) << 28),
                (templates[1].name.clone(), (param + 2) << 28),
            ];
            ops::plan_create_tiered_space(&pool, "fuzzed", &tiers, &mut guid).map(|(plan, _)| plan)
        }
        _ => {
            // Only the partition tables at either end of each member.
            for (a, size) in allowed.iter_mut().zip(&sizes) {
                *a = vec![(0, 1 << 20), (size.saturating_sub(1 << 20), *size)];
            }
            ops::plan_remove_pool(&pool)
        }
    };
    drop(pool);
    let Ok(plan) = plan else { return };
    let inside = |allowed: &[(u64, u64)], offset: u64, len: u64| {
        allowed
            .iter()
            .any(|&(start, end)| start <= offset && offset.checked_add(len).is_some_and(|e| e <= end))
    };
    for action in plan.steps.iter().flat_map(|s| &s.actions) {
        let (target, offset, len) = match action {
            Action::Write { target, offset, bytes } => (*target, *offset, bytes.len() as u64),
            Action::Copy { to, to_offset, len, .. } | Action::Xor { to, to_offset, len, .. } => (*to, *to_offset, *len),
        };
        match target {
            Target::Member(d) => assert!(
                d < count && inside(&allowed[d], offset, len),
                "device {d}: {len:#x} bytes at {offset:#x} outside {:x?}",
                allowed.get(d)
            ),
            Target::New(i) => assert!(i < new.len() && offset + len <= 16 << 30, "new disk {i}: {offset:#x}"),
        }
    }
    if plan.bytes() > 64 << 20 {
        return;
    }
    let _ = plan.apply(&disks.iter().collect::<Vec<_>>(), &new.iter().collect::<Vec<_>>());
    for (device, (disk, allowed)) in disks.iter().zip(&allowed).enumerate() {
        for page in disk.written_pages() {
            assert!(
                allowed.iter().any(|&(start, end)| start <= page && page < end),
                "device {device}: write at {page:#x} outside {allowed:x?}"
            );
        }
    }
});
