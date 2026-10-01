//! Storage tiers created as Windows creates them (scenarios c10tier and
//! c10mapar of tools/scenarios.sh: SSD and HDD members, tier templates,
//! a tiered space), predicted byte for byte from Windows' choices.

use std::fs::File;
use std::path::Path;

use storage_spaces::create::TierTemplate;
use storage_spaces::database::Database;
use storage_spaces::format::assemble_records;
use storage_spaces::io::SparseImage;
use storage_spaces::manage;
use storage_spaces::records::SpaceBody;

fn state(name: &str, label: &str) -> Vec<SparseImage> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/scenarios")
        .join(name)
        .join(label);
    (0..)
        .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
        .map(|f| SparseImage::read_from(f).unwrap())
        .collect()
}

/// The pool database of `disks` (every copy must be the same; pools of more
/// than five disks have five).
fn database(disks: &[SparseImage]) -> Database {
    let copies: Vec<Database> = disks
        .iter()
        .filter_map(|d| Database::read_formatted(d, (16 << 20) + 0x1000).ok())
        .collect();
    assert!(!copies.is_empty());
    assert!(copies.iter().all(|c| c.bytes() == copies[0].bytes()));
    copies.into_iter().next().unwrap()
}

/// The space records (types 3 and 6) of a database.
fn spaces(db: &Database) -> Vec<SpaceBody> {
    assemble_records(db.bytes(), 0x40)
        .unwrap()
        .iter()
        .filter(|r| r.kind == 3 || r.kind == 6)
        .map(|r| SpaceBody::decode(r.kind == 6, &r.body).unwrap())
        .collect()
}

/// New-StorageTier, once per template (SSD mirror and HDD two-column
/// simple; SSD mirror and HDD three-column parity): one database update
/// each, the template's record and nothing else. Windows chose the ids and
/// GUIDs.
#[test]
fn tier_templates_are_predicted_byte_for_byte() {
    // (name, SSD, resiliency, columns) as the scenarios ask for them.
    for (scenario, before, after, asked) in [
        (
            "c10tier",
            "t0",
            "t1",
            [("c10ssd", true, 2, None), ("c10hdd", false, 1, Some(2))],
        ),
        (
            "c10mapar",
            "m0",
            "m1",
            [("c10mssd", true, 2, None), ("c10mhdd", false, 3, Some(3))],
        ),
    ] {
        let mut db = database(&state(scenario, before));
        let windows = database(&state(scenario, after));
        let made = spaces(&windows);
        for (i, (name, ssd, resiliency, columns)) in asked.into_iter().enumerate() {
            let w = made.iter().find(|s| s.name == name).unwrap();
            let template = TierTemplate {
                id: w.id,
                guid: w.guid,
                name: name.into(),
                ssd,
                resiliency,
                columns,
                interleave_log2: 18,
            };
            let time = if i == 1 { windows.timestamp() } else { 0 };
            db = manage::create_tier(&db, &template, time).unwrap();
        }
        assert!(db.bytes() == windows.bytes(), "{scenario} {before} -> {after}");
    }
}

/// The space records of a pool's tiered space and its family, without what
/// is Windows' own choice (ids, GUIDs, sequences, names of hidden spaces),
/// keyed by role and, for tiers, by media.
fn family_fields(db: &Database, space: &str) -> Vec<String> {
    let all = spaces(db);
    let user = all.iter().find(|s| s.name == space).unwrap();
    let mut ids = vec![user.id];
    let mut out = Vec::new();
    let mut i = 0;
    while i < ids.len() {
        let parent = ids[i];
        let children: Vec<u64> = all
            .iter()
            .filter(|s| s.parent == parent && s.id != parent)
            .map(|s| s.id)
            .collect();
        ids.extend(children);
        i += 1;
    }
    for s in all.iter().filter(|s| ids.contains(&s.id)) {
        let mut s = s.clone();
        s.id = 0;
        s.guid = storage_spaces::Guid([0; 16]);
        s.sequence = 0;
        s.parent = 0;
        s.security_descriptor.clear();
        // A tier's name is the space's and the template's.
        if s.name == space {
            s.name = "SPACE".into();
        } else if let Some(template) = s.name.strip_prefix(&format!("{space}-")) {
            s.name = format!("SPACE-{template}");
        }
        out.push(format!("{s:?}"));
    }
    out.sort();
    out
}

/// spaces' own tiered spaces: on pools like the scenarios' (two SSD and two,
/// three or four HDD disks, the same templates), `plan_create_tiered_space`
/// makes the records Windows made, field for field but for ids, GUIDs and
/// where the slabs lie; the hidden spaces' contents are those Windows wrote
/// (cache chunks of the HDD tier's stripe, the journal over the whole
/// space); the pool opens clean and healthy, and the space reads zeros.
#[test]
fn tiered_spaces_are_planned_as_windows_makes_them() {
    use storage_spaces::Pool;
    use storage_spaces::io::Overlay;
    use storage_spaces::ops::{
        BlankDisk, check_pool, plan_create_pool, plan_create_tier, plan_create_tiered_space, plan_set_disk,
    };
    let mut counter = 0u64;
    let mut guid = move || {
        counter += 1;
        storage_spaces::Guid([counter as u8; 16])
    };
    for (scenario, label, hdd, templates, sizes) in [
        (
            "c10tier",
            "t2",
            2,
            [("c10ssd", 2, None), ("c10hdd", 1, Some(2))],
            [1024u64, 2048],
        ),
        (
            "c10mapar",
            "m2",
            3,
            [("c10mssd", 2, None), ("c10mhdd", 3, Some(3))],
            [1024, 2048],
        ),
        (
            "c10tier4",
            "u2",
            4,
            [("c10ssd4", 2, None), ("c10hdd4", 1, Some(4))],
            [1024, 4096],
        ),
    ] {
        let n = 2 + hdd;
        let images: Vec<SparseImage> = (0..n).map(|_| SparseImage::new(8 << 30)).collect();
        let blank: Vec<BlankDisk> = (0..n)
            .map(|_| BlankDisk {
                size: 8 << 30,
                logical_sector: 512,
                physical_sector: 4096,
                manufacturer: "Msft".into(),
                model: "Virtual Disk".into(),
            })
            .collect();
        let devices: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
        let members = || devices.iter().collect::<Vec<_>>();
        let (plan, _) = plan_create_pool(&blank, "tiers", None, &mut guid).unwrap();
        plan.apply::<&Overlay<&SparseImage>, _>(&[], &members()).unwrap();
        for d in 1..=n as u64 {
            let pool = Pool::open(members()).unwrap();
            let plan = plan_set_disk(&pool, d, Some(if d <= 2 { 2 } else { 1 }), None).unwrap();
            drop(pool);
            plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();
        }
        for (name, resiliency, columns) in templates {
            let pool = Pool::open(members()).unwrap();
            let plan = plan_create_tier(&pool, name, name.contains("ssd"), resiliency, columns, &mut guid).unwrap();
            drop(pool);
            plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();
        }
        let space = format!("{scenario}x");
        let tiers: Vec<(String, u64)> = templates
            .iter()
            .zip(sizes)
            .map(|(t, mb)| (t.0.to_owned(), mb << 20))
            .collect();
        let pool = Pool::open(members()).unwrap();
        let (plan, _) = plan_create_tiered_space(&pool, &space, &tiers, &mut guid).unwrap();
        drop(pool);
        plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();

        let pool = Pool::open(members()).unwrap();
        assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
        check_pool(&pool).unwrap();
        let h = storage_spaces::health::health(&pool).unwrap();
        assert!(h.spaces.iter().all(|s| s.state.to_string() == "Healthy / OK"), "{h:?}");
        let mine = Database::read_formatted(&devices[0], (16 << 20) + 0x1000).unwrap();
        let windows = database(&state(scenario, label));
        let windows_space = spaces(&windows).into_iter().find(|s| s.role == 2).unwrap().name;
        assert_eq!(
            family_fields(&mine, &space),
            family_fields(&windows, &windows_space),
            "{scenario}"
        );
        // Every tier reads (zeros on blank disks).
        let r = pool.open_space(pool.find_space(&space).unwrap().id()).unwrap();
        let mut buf = vec![1u8; 1 << 20];
        for at in [0, (sizes[0] << 20) - (1 << 20), sizes[0] << 20, r.size() - (1 << 20)] {
            r.read_exact_at(&mut buf, at).unwrap();
            assert!(buf.iter().all(|&b| b == 0), "{scenario} at {at:#x}");
        }
    }
}
