//! Checks shared by the corpus tests (full images) and the fixture tests
//! (captured metadata only).
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde_json::Value;
use storage_spaces::format::{Resiliency, SLAB_SIZE};
use storage_spaces::io::ReadAt;
use storage_spaces::{Pool, Space};

pub fn manifest(dir: &Path) -> Value {
    let text = fs::read_to_string(dir.join("manifest.json")).unwrap();
    serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap()
}

pub fn family<'p, D: ReadAt>(pool: &'p Pool<D>, root: &'p Space) -> Vec<&'p Space> {
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        out.extend(pool.children(out[i].id()));
        i += 1;
    }
    out
}

/// Compares the parsed pool with Windows' view recorded in the manifest.
pub fn check_metadata<D: ReadAt>(pool: &Pool<D>, m: &Value, dir: &Path) {
    assert!(pool.warnings.is_empty(), "{}: {:?}", dir.display(), pool.warnings);
    assert_eq!(pool.guid.to_string(), m["pool"]["guid"].as_str().unwrap());
    assert_eq!(
        format!("Version {}", pool.version),
        m["pool"]["version"].as_str().unwrap()
    );
    // Spaces inherit the pool's sector size.
    assert_eq!(
        Some(pool.logical_sector_size as u64),
        m["space"]["logical_sector"].as_u64()
    );
    assert_eq!(
        Some(pool.physical_sector_size as u64),
        m["space"]["physical_sector"].as_u64()
    );
    let s = &m["space"];
    let space = pool.find_space(s["name"].as_str().unwrap()).expect("space not found");
    // Manifests from the first generator version recorded the pool GUID here.
    if s["guid"] != m["pool"]["guid"] {
        assert_eq!(space.info.guid.to_string(), s["guid"].as_str().unwrap());
    }
    assert_eq!(space.info.size, s["size"].as_u64());
    let p = space.info.policy.unwrap();
    let resiliency = match p.resiliency {
        Resiliency::Simple => "Simple",
        Resiliency::Mirror => "Mirror",
        Resiliency::Parity => "Parity",
        Resiliency::Other(_) => "?",
    };
    assert_eq!(resiliency, s["resiliency"].as_str().unwrap());
    assert_eq!(Some(p.columns), s["columns"].as_u64());
    assert_eq!(Some(p.copies), s["copies"].as_u64());
    assert_eq!(Some(p.redundancy), s["redundancy"].as_u64());
    assert_eq!(Some(p.interleave), s["interleave"].as_u64());

    // Windows lists the extents of the space and of its hidden children.
    let guid_of = |unique_id: &str| {
        m["disks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["unique_id"] == unique_id)
            .unwrap()["spaces_guid"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let expected: BTreeSet<_> = m["extents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let n = |k: &str| e[k].as_u64().unwrap();
            (
                n("column"),
                n("copy"),
                n("virtual_offset"),
                n("size"),
                n("physical_offset"),
                guid_of(e["disk_unique_id"].as_str().unwrap()),
            )
        })
        .collect();
    let actual: BTreeSet<_> = family(pool, space)
        .iter()
        .flat_map(|sp| &sp.extents)
        .map(|e| {
            let disk = pool.disks[&e.disk_id].guid.to_string();
            (
                e.column,
                e.copy,
                e.virtual_slab * SLAB_SIZE,
                e.slab_count * SLAB_SIZE,
                e.physical_slab * SLAB_SIZE,
                disk,
            )
        })
        .collect();
    assert_eq!(actual, expected, "{}", dir.display());
}
