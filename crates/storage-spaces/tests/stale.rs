//! Mirror pools where a disk dropped out and the space was written while it
//! was away (see tools/vm/New-StalePool.ps1). The returning disk holds
//! out-of-date copies that must never be read.

mod common;

use std::fs::{self, File};
use std::path::Path;

use storage_spaces::{Condition, Pool, testpattern};

#[test]
fn out_of_date_copies_are_never_read() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/stale");
    let Ok(entries) = fs::read_dir(&root) else {
        eprintln!("no stale pools in {}, skipping", root.display());
        return;
    };
    for dir in entries
        .map(|e| e.unwrap().path())
        // Hidden directories are downloads in progress.
        .filter(|p| p.join("manifest.json").exists() && !p.file_name().unwrap().to_string_lossy().starts_with('.'))
    {
        let m = common::manifest(&dir);
        let name = m["space"]["name"].as_str().unwrap();
        let n = m["disks"].as_array().unwrap().len();
        let open = |skip: Option<usize>| {
            let files: Vec<File> = (0..n)
                .filter(|&i| Some(i) != skip)
                .map(|i| File::open(dir.join(format!("disk{i}.img"))).unwrap())
                .collect();
            Pool::open(files).unwrap()
        };
        let patterns: Vec<(String, u64, u64)> = m["patterns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p["tag"].as_str().unwrap().to_string(),
                    p["start"].as_u64().unwrap(),
                    p["length"].as_u64().unwrap(),
                )
            })
            .collect();

        // All disks: every range holds its newest pattern.
        let pool = open(None);
        let space = pool.find_space(name).unwrap();
        let reader = pool.open_space(space.id()).unwrap();
        // The disk that was away holds out-of-date copies.
        assert_eq!(reader.condition(), Condition::Degraded, "{name}");
        let mut block = vec![0u8; testpattern::BLOCK];
        for (tag, start, len) in &patterns {
            for offset in (*start..start + len).step_by(0x40000) {
                reader.read_exact_at(&mut block, offset).unwrap();
                assert_eq!(testpattern::verify(&block, offset, tag), None, "{name} at {offset:#x}");
            }
        }

        // Without the disk holding the current copies, the rewritten range
        // has only out-of-date copies left: reads must fail.
        let current_disk = space
            .extents
            .iter()
            .find(|e| e.is_current() && e.virtual_slab == 0)
            .unwrap()
            .disk_id;
        let guid = pool.disks[&current_disk].guid.to_string();
        let device = m["disks"]
            .as_array()
            .unwrap()
            .iter()
            .position(|d| d["spaces_guid"] == guid.as_str())
            .unwrap();
        let degraded = open(Some(device));
        let reader = degraded.open_space(degraded.find_space(name).unwrap().id()).unwrap();
        assert_eq!(reader.condition(), Condition::Failed, "{name}");
        let (tag, start, _) = &patterns[0];
        match reader.read_exact_at(&mut block, *start) {
            Err(_) => {}
            Ok(()) => panic!(
                "{name}: read succeeded without a current copy (old data: {})",
                testpattern::verify(&block, *start, tag).is_some()
            ),
        }
    }
}

#[test]
fn a_lone_dropped_out_disk_has_no_quorum() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/stale/stale3");
    if !dir.join("manifest.json").exists() {
        return;
    }
    let m = common::manifest(&dir);
    let removed = m["removed_disk"].as_u64().unwrap();
    let pool = Pool::open(vec![File::open(dir.join(format!("disk{removed}.img"))).unwrap()]).unwrap();
    assert!(!pool.has_quorum());
    let full = Pool::open(
        (0..3)
            .map(|i| File::open(dir.join(format!("disk{i}.img"))).unwrap())
            .collect(),
    )
    .unwrap();
    assert!(full.has_quorum());
}
