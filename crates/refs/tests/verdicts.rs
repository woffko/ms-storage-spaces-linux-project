//! No false alarms: every space of every pool of the corpus gets the
//! guard's verdict this table expects (healthy unless listed, and then with
//! the check that makes it so and why), with the file systems in it
//! checked as `spaces attach` checks them (NTFS, ReFS). Where a manifest
//! records the state Windows showed, space.state says the same.
//!
//! Full images (testdata/pools, crash, stale, snapshots, refs; skipped
//! when missing) and the metadata fixtures and scenario states of
//! crates/storage-spaces/tests (always present).

use std::fs::File;
use std::path::{Path, PathBuf};

use storage_spaces::Pool;
use storage_spaces::gpt::Partition;
use storage_spaces::guard::{self, Options};
use storage_spaces::io::{ReadAt, SparseImage};
use storage_spaces::report::{Check, Verdict};

/// The pools and states that are not healthy, and why: (directory, from
/// its end; verdict; the check that says so; the reason).
const NOT_HEALTHY: &[(&str, Verdict, &str, &str)] = &[
    (
        "crash/crashparity",
        Verdict::Suspect,
        "space.cache",
        "disks pulled while Windows wrote: a cache slot reached one copy only",
    ),
    (
        "crash/crashparitywc",
        Verdict::Suspect,
        "space.cache",
        "disks pulled while Windows wrote: a cache slot reached one copy only",
    ),
    (
        "stale/stale3",
        Verdict::Degraded,
        "space.state",
        "a disk missed writes while it was away: copies out of date",
    ),
    (
        "snapshots/m5stale/s2",
        Verdict::Suspect,
        "pool.quorum",
        "one of two disks away: the pool lacks its quorum",
    ),
    (
        "snapshots/m5stale/s3",
        Verdict::Suspect,
        "pool.quorum",
        "one of two disks away: the pool lacks its quorum",
    ),
    (
        "scenarios/m5stale/s2",
        Verdict::Suspect,
        "pool.quorum",
        "one of two disks away: the pool lacks its quorum",
    ),
    (
        "scenarios/m5stale/s3",
        Verdict::Suspect,
        "pool.quorum",
        "one of two disks away: the pool lacks its quorum",
    ),
    (
        "refs/r314mirror",
        Verdict::Suspect,
        "fs.refs.log",
        "the ReFS volume in it was detached without being dismounted: its log holds a record past the checkpoint",
    ),
];

/// States after the pool was removed: no pool to check.
const NO_POOL: &[&str] = &[
    "snapshots/c9disk/d5",
    "snapshots/c9ops/o8",
    "snapshots/c9smoke/b0",
    "snapshots/c9smoke/p3",
    "scenarios/c9ops/o8",
    "scenarios/c9smoke/p3",
];

fn fs_hook<D: ReadAt>(reader: &storage_spaces::SpaceReader<'_, D>, part: &Partition, names: &[String]) -> Vec<Check> {
    let label = format!("p{}", part.number);
    let locate = |o: u64| reader.describe(o, names);
    if let Some(c) = guard::ntfs_check(reader, part.offset, part.length, &label, &locate) {
        return vec![c];
    }
    refs::health::quick_checks(reader, part.offset, &label, &locate)
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Every directory below `root` (itself included) that holds disk0.`ext`.
fn pools(root: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(d) = todo.pop() {
        if d.join(format!("disk0.{ext}")).exists() {
            out.push(d.clone());
        }
        todo.extend(subdirs(&d));
    }
    out.sort();
    out
}

/// Checks the pool of `disks` against the table; returns its spaces.
fn check_pool<D: ReadAt>(
    name: &str,
    dir: &Path,
    disks: Vec<D>,
    failures: &mut Vec<String>,
    seen: &mut Vec<String>,
) -> usize {
    seen.push(name.to_string());
    let expected_none = NO_POOL.iter().any(|n| name.ends_with(n));
    let pool = match Pool::open(disks) {
        Ok(p) => p,
        Err(e) => {
            if !expected_none {
                failures.push(format!("{name}: the pool does not open: {e}"));
            }
            return 0;
        }
    };
    if expected_none {
        failures.push(format!("{name}: a pool where none was expected"));
    }
    let names: Vec<String> = (0..pool.members.len() + 8).map(|i| format!("disk{i}")).collect();
    // The listed stripes match their parity everywhere (with the full
    // budget too): a small budget keeps the test fast.
    let options = Options {
        journal_budget: Some(64 << 20),
    };
    let manifest: Option<serde_json::Value> = std::fs::read_to_string(dir.join("manifest.json"))
        .ok()
        .and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok());
    let reports = guard::pool_reports(&pool, &names, &options, &mut fs_hook);
    for r in &reports {
        let space = &r.space.as_ref().unwrap().0;
        let expected = NOT_HEALTHY.iter().find(|e| name.ends_with(e.0));
        let verdict = r.verdict();
        match expected {
            Some(&(_, v, id, why)) => {
                if verdict != v || !r.problems().iter().any(|c| c.id == id) {
                    failures.push(format!(
                        "{name}/{space}: expected {} by {id} ({why}), got\n{}",
                        v.as_str(),
                        r.to_text()
                    ));
                }
            }
            None if verdict != Verdict::Healthy => {
                failures.push(format!("{name}/{space}: not healthy\n{}", r.to_text()));
            }
            None => {}
        }
        // Where Windows' state is on record, space.state says the same.
        let recorded = manifest.as_ref().and_then(|m| {
            std::iter::once(&m["space"])
                .chain(m["extra_spaces"].as_array().into_iter().flatten())
                .find(|s| s["name"] == space.as_str())
                .and_then(|s| s.get("state"))
                .filter(|s| s["manual_attach"] != true)
                .and_then(|s| s["health"].as_str().map(str::to_string))
        });
        if let Some(health) = recorded {
            let state = r.checks.iter().find(|c| c.id == "space.state").unwrap();
            if !state.summary.starts_with(&health) {
                failures.push(format!(
                    "{name}/{space}: Windows showed {health}, space.state says {}",
                    state.summary
                ));
            }
        }
    }
    reports.len()
}

#[test]
fn every_corpus_space_gets_the_expected_verdict() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut failures = Vec::new();
    let mut seen = Vec::new();
    let mut spaces = (0, 0);
    for kind in ["pools", "crash", "stale", "snapshots", "refs"] {
        for dir in pools(&root.join("testdata").join(kind), "img") {
            let name = format!(
                "{kind}/{}",
                dir.strip_prefix(root.join("testdata").join(kind)).unwrap().display()
            );
            let disks: Vec<File> = (0..)
                .map_while(|i| File::open(dir.join(format!("disk{i}.img"))).ok())
                .collect();
            // A plain ReFS volume (refs/NAME/disk.img) is no pool.
            if kind == "refs" && disks.len() < 2 {
                continue;
            }
            spaces.0 += check_pool(&name, &dir, disks, &mut failures, &mut seen);
        }
    }
    let tests = root.join("crates/storage-spaces/tests");
    for kind in ["fixtures", "scenarios", "data"] {
        for dir in pools(&tests.join(kind), "fixture") {
            let name = format!("{kind}/{}", dir.strip_prefix(tests.join(kind)).unwrap().display());
            let images: Vec<SparseImage> = (0..)
                .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
                .map(|f| SparseImage::read_from(f).unwrap())
                .collect();
            spaces.1 += check_pool(&name, &dir, images.iter().collect::<Vec<_>>(), &mut failures, &mut seen);
        }
    }
    eprintln!("{} spaces of full images, {} of fixtures checked", spaces.0, spaces.1);
    assert!(spaces.1 >= 240, "{spaces:?}");
    // The states of the table that are always here were checked.
    for listed in NOT_HEALTHY.iter().map(|e| e.0).chain(NO_POOL.iter().copied()) {
        if listed.starts_with("scenarios/") {
            assert!(seen.iter().any(|s| s.ends_with(listed)), "{listed} not found");
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
