//! The health report in Windows' terms (`storage_spaces::health`): the
//! states Windows recorded for its pools, and the rules for missing disks.

use std::fs::File;
use std::path::Path;

use storage_spaces::Pool;
use storage_spaces::health::health;
use storage_spaces::io::SparseImage;

fn disks(dir: &Path) -> Vec<SparseImage> {
    (0..)
        .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
        .map(|f| SparseImage::read_from(f).unwrap())
        .collect()
}

/// Every fixture pool whose manifest records Windows' view of its spaces
/// and disks (Get-VirtualDisk, Get-PhysicalDisk when the pool was made) is
/// predicted as Windows showed it, retired, hot spare and journal disks
/// included. A space set to manual attach and detached (spstates) is left
/// out: that state is not in the pool's metadata.
#[test]
fn windows_pools_have_the_health_windows_showed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut checked = (0, 0);
    let mut dirs: Vec<_> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().path()).collect();
    dirs.sort();
    for dir in dirs {
        let manifest: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(dir.join("manifest.json"))
                .unwrap()
                .trim_start_matches('\u{feff}'),
        )
        .unwrap();
        let images = disks(&dir);
        let pool = Pool::open(images.iter().collect::<Vec<_>>()).unwrap();
        let h = health(&pool).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        let spaces =
            std::iter::once(&manifest["space"]).chain(manifest["extra_spaces"].as_array().into_iter().flatten());
        for s in spaces {
            let Some(state) = s.get("state") else { continue };
            if state["manual_attach"] == true {
                continue;
            }
            let predicted = h.spaces.iter().find(|p| p.name == s["name"].as_str().unwrap()).unwrap();
            assert_eq!(predicted.state.health.to_string(), state["health"], "{}", dir.display());
            assert_eq!(
                predicted.state.operational.join(" "),
                state["operational"],
                "{}",
                dir.display()
            );
            checked.0 += 1;
        }
        for d in manifest["disks"].as_array().into_iter().flatten() {
            let Some(windows) = d.get("health") else { continue };
            let guid = d["spaces_guid"].as_str().unwrap();
            let disk = pool.disks.values().find(|x| x.guid.to_string() == guid).unwrap();
            let predicted = h.disks.iter().find(|x| x.id == disk.id).unwrap();
            assert_eq!(predicted.state.health.to_string(), *windows, "{}", dir.display());
            assert_eq!(
                predicted.state.operational.join(" "),
                d["operational"],
                "{}",
                dir.display()
            );
            checked.1 += 1;
        }
        assert_eq!(h.pool.to_string(), "Healthy / OK", "{}", dir.display());
    }
    assert!(checked.0 >= 7 && checked.1 >= 20, "{checked:?}");
}

/// The pool of tools/health-states.sh (four disks; a one-column and a
/// four-column simple space, a one- and a two-column mirror, a parity
/// space) with disks left out. Windows could not be asked: it bugchecked
/// when this layout arrived without a disk (see roundtrip.rs); the rules
/// are those of `health`, with the pool states Windows showed elsewhere.
#[test]
fn missing_disks_reduce_the_health_by_the_rules() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios/c11health");
    let all = disks(&dir);
    let cases: [(&[usize], &str, [&str; 5]); 4] = [
        (
            &[0, 1, 2, 3],
            "Healthy / OK",
            [
                "Healthy / OK",
                "Healthy / OK",
                "Healthy / OK",
                "Healthy / OK",
                "Healthy / OK",
            ],
        ),
        // Disk 0 holds the one-column simple space, a column of the wide
        // one, a copy of a column of the two-column mirror and a parity
        // column.
        (
            &[1, 2, 3],
            "Warning / Degraded",
            [
                "Unhealthy / Detached",
                "Unhealthy / Detached",
                "Healthy / OK",
                "Unhealthy / No Redundancy Incomplete",
                "Unhealthy / No Redundancy Incomplete",
            ],
        ),
        // Two of four database copies: no quorum, every space detached.
        (&[0, 1], "Unhealthy / Read-only", ["Unhealthy / Detached"; 5]),
        (&[0], "Unhealthy / Read-only", ["Unhealthy / Detached"; 5]),
    ];
    for (present, pool_state, spaces) in cases {
        let pool = Pool::open(present.iter().map(|&i| &all[i]).collect::<Vec<_>>()).unwrap();
        let h = health(&pool).unwrap();
        assert_eq!(h.pool.to_string(), pool_state, "{present:?}");
        assert_eq!(h.database_copies, (present.len(), 4));
        let lost = h
            .disks
            .iter()
            .filter(|d| d.state.to_string() == "Warning / Lost Communication")
            .count();
        assert_eq!(lost, 4 - present.len());
        let names = ["hsimple", "hwide", "hmirror", "hmirror2", "hparity"];
        for (name, want) in names.iter().zip(spaces) {
            let s = h.spaces.iter().find(|s| s.name == *name).unwrap();
            assert_eq!(s.state.to_string(), want, "{present:?} {name}");
        }
    }
}

/// The pool states Windows showed with disks lost (mgmt-crash.json: the
/// first versions of disk add and retire), which the rules follow: one of
/// three disks lost left the pool Degraded, two of four made it Read-only
/// and detached its spaces; and a two-way mirror that lost a copy had No
/// Redundancy (tornt2.json).
#[test]
fn the_rules_follow_what_windows_showed() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/evidence");
    let read = |name: &str| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(dir.join(name)).unwrap()).unwrap()
    };
    let crash = read("mgmt-crash.json");
    let lost = |s: &serde_json::Value| {
        s["disks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d[1] == "Lost Communication")
            .count()
    };
    let add = &crash["before_fix"]["adddisk"]["connected"];
    assert_eq!((lost(add), add["disks"].as_array().unwrap().len()), (1, 3));
    assert_eq!(add["pool"], serde_json::json!(["Warning", "Degraded"]));
    let retire = &crash["before_fix"]["retire"]["connected"];
    assert_eq!((lost(retire), retire["disks"].as_array().unwrap().len()), (2, 4));
    assert_eq!(retire["pool"], serde_json::json!(["Unhealthy", "Read-only"]));
    assert!(retire["spaces"].as_array().unwrap().iter().all(|s| s[2] == "Detached"));
    let torn = read("tornt2.json");
    assert_eq!(torn["connected"]["spaces"][0][1], "Unhealthy");
    assert!(
        torn["connected"]["spaces"][0][2]
            .as_str()
            .unwrap()
            .starts_with("No Redundancy")
    );
}
