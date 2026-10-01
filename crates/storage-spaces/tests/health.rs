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

/// Windows' view of the pool of tools/health-states.sh (four disks; a
/// one-column and a four-column simple space, a one- and a two-column
/// mirror, a parity space), created once on Linux (`c11health`) and once by
/// Windows (`c11ctl`), after disks were detached while it was in use: each
/// disk in turn, then two of the four (health-drop.json). The prediction
/// from the other disks matches it for the pool, the disks and every space.
#[test]
fn windows_shows_the_predicted_health_for_lost_disks() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let e: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("evidence/health-drop.json")).unwrap()).unwrap();
    let cases = e["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 12);
    for case in cases {
        let dir = match case["pool"].as_str().unwrap() {
            "linux" => root.join("scenarios/c11health"),
            _ => root.join("scenarios/c11ctl/h0"),
        };
        let all = disks(&dir);
        let dropped: Vec<usize> = case["dropped"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d.as_u64().unwrap() as usize)
            .collect();
        let pool = Pool::open(
            (0..all.len())
                .filter(|i| !dropped.contains(i))
                .map(|i| &all[i])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let h = health(&pool).unwrap();
        let what = format!("{} without {dropped:?}", case["pool"]);
        let w = &case["windows"];
        assert_eq!(
            h.pool.to_string(),
            format!(
                "{} / {}",
                w["pool"][0].as_str().unwrap(),
                w["pool"][1].as_str().unwrap()
            ),
            "{what}"
        );
        let lost = h
            .disks
            .iter()
            .filter(|d| d.state.to_string() == "Warning / Lost Communication")
            .count();
        assert_eq!(lost, w["lost_disks"].as_u64().unwrap() as usize, "{what}");
        let spaces = w["spaces"].as_object().unwrap();
        assert_eq!(spaces.len(), h.spaces.len());
        for s in &h.spaces {
            let ws = &spaces[&s.name];
            assert_eq!(
                s.state.to_string(),
                format!("{} / {}", ws[0].as_str().unwrap(), ws[1].as_str().unwrap()),
                "{what}: {}",
                s.name
            );
        }
    }
}

/// Without a quorum of database copies (one of four disks), every space is
/// detached: the rule of `health`, as Windows showed it with two of four.
#[test]
fn a_pool_without_quorum_detaches_every_space() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios/c11health");
    let all = disks(&dir);
    let pool = Pool::open(vec![&all[0]]).unwrap();
    let h = health(&pool).unwrap();
    assert_eq!(h.pool.to_string(), "Unhealthy / Read-only");
    assert_eq!(h.database_copies, (1, 4));
    assert!(h.spaces.iter().all(|s| s.state.to_string() == "Unhealthy / Detached"));
}

/// The pool states Windows showed with disks lost (mgmt-crash.json: the
/// first versions of disk add and retire), which the rules follow: one of
/// three disks lost left the pool Degraded, two of four made it Read-only
/// and detached its spaces; and a two-column simple space that lost a disk
/// had No Redundancy (tornt2.json).
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
