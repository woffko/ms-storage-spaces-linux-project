//! Checks shared by the corpus tests (full images) and the fixture tests
//! (captured metadata only).
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde_json::Value;
use storage_spaces::format::{Provisioning, Resiliency, SLAB_SIZE};
use storage_spaces::io::ReadAt;
use storage_spaces::{Condition, Pool, Space};

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
    match m["pool"]["version_number"].as_u64() {
        Some(n) => assert_eq!(u64::from(pool.version), n),
        // Older manifests have the display string only; Windows 11 24H2
        // shows version 28 as "Windows Server vNext".
        None => match m["pool"]["version"].as_str().unwrap() {
            "Windows Server vNext" => assert_eq!(pool.version, 28),
            s => assert_eq!(format!("Version {}", pool.version), s),
        },
    }
    // Spaces inherit the pool's sector size.
    assert_eq!(
        Some(pool.logical_sector_size as u64),
        m["space"]["logical_sector"].as_u64()
    );
    assert_eq!(
        Some(pool.physical_sector_size as u64),
        m["space"]["physical_sector"].as_u64()
    );
    for d in m["disks"].as_array().unwrap() {
        if let Some(usage) = d["usage"].as_str() {
            let guid = d["spaces_guid"].as_str().unwrap();
            let disk = pool.disks.values().find(|p| p.guid.to_string() == guid).unwrap();
            assert_eq!(disk.usage.name(), usage, "{}: disk {guid}", dir.display());
        }
    }
    check_space(pool, m, &m["space"], &m["extents"], &m["tiers"], dir);
    for extra in m["extra_spaces"].as_array().into_iter().flatten() {
        check_space(pool, m, extra, &extra["extents"], &Value::Null, dir);
    }
}

/// Compares one space (`s`, with Windows' extent list and tiers) with the
/// parsed pool.
fn check_space<D: ReadAt>(pool: &Pool<D>, m: &Value, s: &Value, extents: &Value, tiers: &Value, dir: &Path) {
    let space = pool.find_space(s["name"].as_str().unwrap()).expect("space not found");
    // With every disk present, compare with the health Windows reported.
    let condition = pool.open_space(space.id()).unwrap().condition();
    let expected = match s["state"]["health"].as_str() {
        Some("Warning") => Condition::Degraded,
        Some("Unhealthy") => Condition::Failed,
        _ => Condition::Healthy,
    };
    assert_eq!(condition, expected, "{}: {}", dir.display(), space.name());
    // Manifests from the first generator version recorded the pool GUID here.
    if s["guid"] != m["pool"]["guid"] {
        assert_eq!(space.info.guid.to_string(), s["guid"].as_str().unwrap());
    }
    assert_eq!(space.info.size, s["size"].as_u64());
    if let Some(prov) = s["provisioning"].as_str() {
        let expected = match prov {
            "Thin" => Provisioning::Thin,
            _ => Provisioning::Fixed,
        };
        assert_eq!(space.info.provisioning, expected, "{}", dir.display());
    }
    if let Some(au) = s["allocation_unit"].as_u64() {
        assert_eq!(space.info.allocation_unit, au, "{}", dir.display());
    }
    let name_of = |r: Resiliency| match r {
        Resiliency::Simple => "Simple",
        Resiliency::Mirror => "Mirror",
        Resiliency::Parity => "Parity",
        Resiliency::Other(_) => "?",
    };
    if s["resiliency"].is_null() {
        // Tiered space: Windows reports the policy per tier.
        for t in tiers.as_array().into_iter().flatten() {
            let tier = family(pool, space)
                .into_iter()
                .find(|c| c.info.is_child && c.name().ends_with(t["name"].as_str().unwrap()))
                .unwrap_or_else(|| panic!("{}: tier {} not found", dir.display(), t["name"]));
            let p = tier.info.policy.unwrap();
            assert_eq!(name_of(p.resiliency), t["resiliency"].as_str().unwrap());
            assert_eq!(Some(p.columns), t["columns"].as_u64());
            assert_eq!(Some(p.copies), t["copies"].as_u64());
            assert_eq!(Some(p.interleave), t["interleave"].as_u64());
        }
    } else {
        let p = space.info.policy.unwrap();
        assert_eq!(name_of(p.resiliency), s["resiliency"].as_str().unwrap());
        assert_eq!(Some(p.columns), s["columns"].as_u64());
        assert_eq!(Some(p.copies), s["copies"].as_u64());
        assert_eq!(Some(p.redundancy), s["redundancy"].as_u64());
        assert_eq!(Some(p.interleave), s["interleave"].as_u64());
    }

    // Windows lists the extents of the space and of its hidden children
    // (not recorded while a repair was moving them).
    if extents.is_null() {
        return;
    }
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
    let expected: BTreeSet<_> = extents
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
