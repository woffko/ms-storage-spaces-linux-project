//! `spaces check`: the guard's report on each space (storage_spaces::guard)
//! with the file systems of its partitions checked (NTFS by the library,
//! ReFS by the refs crate); on request the deep checks, and a bundle of the
//! pool's metadata for a report.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use storage_spaces::gpt::{MSR_PARTITION_TYPE, Partition};
use storage_spaces::guard::{self, Options};
use storage_spaces::io::ReadAt;
use storage_spaces::report::{Check, Evidence, Report, Status, Truth, Verdict, reports_to_json};
use storage_spaces::{Pool, Space, SpaceReader};

use crate::bundle;

/// What may be done with a space of a verdict, given the flags (attach,
/// the servers and export ask the same).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// As asked (read-write with --rw): the space is healthy.
    Attach { rw: bool },
    /// Read-only: the flag given allows a space that is not healthy.
    ReadOnly,
    /// Nothing, for this reason.
    Refuse(String),
}

/// The guard's rule: healthy spaces as asked; degraded ones read-only with
/// `--degraded` (or `--force`), suspect ones read-only with `--force`;
/// failed ones not at all; writing only healthy ones.
pub fn gate(verdict: Verdict, rw: bool, degraded: bool, force: bool) -> Decision {
    match verdict {
        Verdict::Healthy => Decision::Attach { rw },
        v if rw => Decision::Refuse(format!("writing needs a healthy space, and this one is {}", v.as_str())),
        v if v.allowed(degraded, force) => Decision::ReadOnly,
        Verdict::Failed => Decision::Refuse("a failed space is not read at all".into()),
        v => Decision::Refuse(format!(
            "a {} space needs {} (and is then read-only)",
            v.as_str(),
            v.flag().unwrap_or("--force")
        )),
    }
}

/// What to do about a space the gate refused: the command that reads it
/// anyway, read-only, or where to turn.
pub fn refusal_note(verdict: Verdict, rw: bool, command: &str) -> String {
    match verdict.flag() {
        _ if rw && verdict != Verdict::Failed => {
            format!("Without --rw it is read as its verdict allows: {command}")
        }
        Some(flag) => format!("To read it anyway, read-only: {command} {flag}"),
        None => "Nothing reads a failed space: spaces check --bundle FILE.tar.gz packs the reports \
                 and the pool's metadata for a bug report."
            .into(),
    }
}

/// The report on space `space` of `pool` (quick checks, as attach makes).
pub fn space_report<D: ReadAt>(pool: &Pool<D>, paths: &[PathBuf], space: &Space) -> Report {
    reports(pool, paths, Some(&space.info.guid.to_string()), false)
        .pop()
        .expect("the space is a user space of the pool")
}

/// Refuses what the gate refuses for the servers and export: an error
/// with the report's headline and the reason.
pub fn require<D: ReadAt>(
    pool: &Pool<D>,
    paths: &[PathBuf],
    space: &Space,
    rw: bool,
    degraded: bool,
    force: bool,
) -> Result<()> {
    let report = space_report(pool, paths, space);
    match gate(report.verdict(), rw, degraded, force) {
        Decision::Refuse(why) => {
            for c in report.problems() {
                eprintln!("  {} {}: {}", c.status.as_str().to_uppercase(), c.id, c.summary);
            }
            anyhow::bail!("{}: {why} (spaces check tells more)", report.headline())
        }
        _ => Ok(()),
    }
}

/// Device names for evidence: the paths the devices were opened by.
pub fn names(paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|p| p.display().to_string()).collect()
}

/// The checks of the file system in a partition of a space (`number` 0:
/// the whole space): NTFS and ReFS, found by their boot sectors; with
/// `deep`, a ReFS volume is also checked whole, as `refs check` does.
pub fn fs_checks<D: ReadAt>(reader: &SpaceReader<'_, D>, part: &Partition, names: &[String], deep: bool) -> Vec<Check> {
    let label = if part.number == 0 {
        "the space".to_string()
    } else {
        format!("p{}", part.number)
    };
    let locate = |o: u64| reader.describe(o, names);
    if let Some(c) = guard::ntfs_check(reader, part.offset, part.length, &label, &locate) {
        return vec![c];
    }
    let mut checks = refs::health::quick_checks(reader, part.offset, &label, &locate);
    if checks.is_empty() {
        if part.kind == MSR_PARTITION_TYPE {
            return Vec::new();
        }
        return vec![Check::skipped(
            "fs",
            format!("{label}: neither NTFS nor ReFS, not checked"),
        )];
    }
    if deep
        && checks
            .iter()
            .all(|c| c.status != Status::Failed && c.status != Status::Warning)
    {
        checks.push(deep_refs(reader, part.offset, &label));
    }
    checks
}

/// `deep.refs`: the ReFS volume checked as `refs check` does.
fn deep_refs<D: ReadAt>(reader: &SpaceReader<'_, D>, offset: u64, label: &str) -> Check {
    const ID: &str = "deep.refs";
    let vol = match refs::Volume::open(reader, offset) {
        Ok(v) => v,
        Err(e) => {
            return Check::new(ID, Status::Suspect, format!("{label}: the volume does not open: {e}"))
                .truth(Truth::Format);
        }
    };
    match vol.check(&[]) {
        Ok(r) if r.problem_count == 0 => Check::ok(
            ID,
            format!(
                "{label}: {} pages, {} directories and {} files checked, no problems",
                r.pages, r.directories, r.files
            ),
        )
        .truth(Truth::CrossCheck),
        Ok(r) => {
            let mut check = Check::new(
                ID,
                Status::Suspect,
                format!("{label}: {} problem(s) in the volume's structures", r.problem_count),
            )
            .truth(Truth::CrossCheck)
            .advice("Windows repairs what it can of a ReFS volume when it attaches it (or with refsutil).");
            for p in r.problems.iter().take(5) {
                check = check.evidence(Evidence::new(p.clone()));
            }
            check
        }
        Err(e) => Check::new(ID, Status::Suspect, format!("{label}: the check stopped: {e}")).truth(Truth::CrossCheck),
    }
}

/// `deep.scrub`: every copy of the space's mirror rows and every stripe of
/// a single parity space compared (`spaces pool scrub`, read-only).
fn deep_scrub<D: ReadAt>(pool: &Pool<D>, space: &Space) -> Check {
    const ID: &str = "deep.scrub";
    let scrub = match storage_spaces::ops::scrub_spaces(pool, Some(space.id())) {
        Ok(s) => s,
        Err(e) => return Check::skipped(ID, format!("not scrubbed: {e}")),
    };
    let line = scrub.lines.first().cloned().unwrap_or_default();
    if line.ends_with("not scrubbed") {
        return Check::skipped(ID, line);
    }
    if scrub.mismatches > 0 {
        return Check::new(ID, Status::Suspect, line).truth(Truth::CrossCheck).advice(
            "Copies of the space differ where nothing was being written: one is wrong, and which is not \
             known. spaces pool scrub --repair makes them agree (keeping the first copy, and the data \
             of parity spaces).",
        );
    }
    if scrub.unsettled > 0 {
        return Check::new(ID, Status::Suspect, line).truth(Truth::CrossCheck).advice(
            "Copies differ where writes were under way when the pool was last in use; Windows settles \
             them when it next attaches the pool. Reads of them fail (--unclean-parity data reads one).",
        );
    }
    Check::ok(ID, "every copy and every parity unit read agrees").truth(Truth::CrossCheck)
}

/// What a report says about the system it was made on.
pub fn environment<D>(pool: &Pool<D>) -> Vec<(String, String)> {
    let mut env = vec![("spaces".to_string(), env!("CARGO_PKG_VERSION").to_string())];
    if let Ok(k) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        env.push(("kernel".into(), k.trim().into()));
    }
    env.push(("pool version".into(), pool.version.to_string()));
    env
}

/// Whether `key` (a name or GUID) names space `space`.
pub fn names_space(space: &Space, key: &str) -> bool {
    key == space.name() || key.eq_ignore_ascii_case(&space.info.guid.to_string())
}

/// The reports on the user spaces of `pool` (or on space `only`).
pub fn reports<D: ReadAt>(pool: &Pool<D>, paths: &[PathBuf], only: Option<&str>, deep: bool) -> Vec<Report> {
    let names = names(paths);
    let options = Options {
        journal_budget: if deep { None } else { Options::default().journal_budget },
    };
    pool.user_spaces()
        .filter(|s| only.is_none_or(|key| names_space(s, key)))
        .map(|space| {
            let mut r = guard::space_report(pool, space, &names, &options, &mut |reader, part, names| {
                fs_checks(reader, part, names, deep)
            });
            if deep {
                r.checks.push(deep_scrub(pool, space));
            }
            r.environment = environment(pool);
            r
        })
        .collect()
}

/// The report on devices that do not open as a pool.
fn unopened(paths: &[PathBuf], error: &str) -> Report {
    let mut r = Report::new(format!("the pool on {}", names(paths).join(", ")));
    r.checks.push(
        Check::new("pool.open", Status::Failed, "the pool cannot be assembled")
            .evidence(Evidence::new("the pool's metadata").expected("readable").found(error))
            .truth(Truth::Format),
    );
    r
}

/// Files of a bundle for the pool on `paths` (under `pool-<guid>/`).
fn bundle_files<D: ReadAt>(pool: &Pool<D>, reports: &[Report], out: &mut bundle::Files) {
    let dir = format!("pool-{}", pool.guid);
    let mut add = |name: String, data: Vec<u8>| out.push((format!("{dir}/{name}"), data));
    for r in reports {
        let guid = r.space.as_ref().map_or("pool".to_string(), |s| s.1.clone());
        add(format!("reports/{guid}.txt"), r.to_text().into_bytes());
        add(format!("reports/{guid}.json"), r.to_json().into_bytes());
    }
    add(
        "dump.txt".into(),
        crate::dump::dump(pool)
            .iter()
            .map(|l| format!("{l}\n"))
            .collect::<String>()
            .into_bytes(),
    );
    for space in pool.user_spaces() {
        let guid = space.info.guid.to_string();
        add(
            format!("extents/{guid}.txt"),
            crate::extents_text(pool, space).into_bytes(),
        );
        // The partition table's sectors at both ends of the space.
        const ENDS: u64 = 64 << 10;
        match pool.open_space(space.id()) {
            Ok(reader) => {
                for (what, at) in [("start", 0), ("end", reader.size().saturating_sub(ENDS))] {
                    let len = ENDS.min(reader.size()) as usize;
                    let mut buf = vec![0u8; len];
                    match reader.read_exact_at(&mut buf, at) {
                        Ok(()) => add(format!("gpt/{guid}-{what}.bin"), buf),
                        Err(e) => add(format!("gpt/{guid}-{what}.error"), e.to_string().into_bytes()),
                    }
                }
            }
            Err(e) => add(format!("gpt/{guid}.error"), e.to_string().into_bytes()),
        }
    }
}

/// `spaces check`: prints the reports; returns whether every space is
/// healthy.
pub fn run(
    json: bool,
    deep: bool,
    only: Option<&str>,
    bundle_path: Option<&Path>,
    devices: &[PathBuf],
) -> Result<bool> {
    let groups: Vec<Vec<PathBuf>> = if devices.is_empty() {
        scanned()
    } else {
        vec![devices.to_vec()]
    };
    let mut all = Vec::new();
    let mut files = bundle::Files::new();
    for paths in groups {
        let opened = paths
            .iter()
            .map(|p| crate::open_member(p, false, false))
            .collect::<Result<Vec<File>>>()
            .and_then(|f| Pool::open(f).map_err(anyhow::Error::from));
        match opened {
            Ok(pool) => {
                let reports = reports(&pool, &paths, only, deep);
                if bundle_path.is_some() {
                    bundle_files(&pool, &reports, &mut files);
                }
                all.extend(reports);
            }
            Err(e) => all.push(unopened(&paths, &format!("{e:#}"))),
        }
    }
    if all.is_empty() {
        println!("no Storage Spaces pool members found");
    } else if json {
        print!("{}", reports_to_json(&all));
    } else {
        for (i, r) in all.iter().enumerate() {
            if i > 0 {
                println!();
            }
            print!("{}", r.to_text());
        }
    }
    if let Some(path) = bundle_path {
        files.insert(
            0,
            (
                "README.txt".into(),
                b"spaces check --bundle: the reports, `spaces dump` and `spaces extents` of each pool,\n\
                  and the first and last 64 KiB of each space (its partition table). No file data,\n\
                  but the names and GUIDs of pools, spaces and disks: review it before sharing it.\n"
                    .to_vec(),
            ),
        );
        let mut versions = format!("spaces {}\n", env!("CARGO_PKG_VERSION"));
        for (what, file) in [("kernel", "/proc/sys/kernel/osrelease"), ("os", "/etc/os-release")] {
            if let Ok(text) = std::fs::read_to_string(file) {
                let value = if what == "os" {
                    text.lines()
                        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                        .unwrap_or_default()
                        .trim_matches('"')
                        .to_string()
                } else {
                    text.trim().to_string()
                };
                versions += &format!("{what} {value}\n");
            }
        }
        files.insert(1, ("versions.txt".into(), versions.into_bytes()));
        let mtime = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        std::fs::write(path, bundle::tar_gz(&files, mtime))
            .with_context(|| format!("cannot write {}", path.display()))?;
        eprintln!(
            "bundle: {} (pool, space and disk names and GUIDs; no file data: review it before sharing)",
            path.display()
        );
    }
    Ok(all.iter().all(|r| r.verdict() == Verdict::Healthy))
}

/// The member devices of every pool found by scanning.
#[cfg(target_os = "linux")]
fn scanned() -> Vec<Vec<PathBuf>> {
    let found = crate::scan::scan();
    if found.denied > 0 {
        eprintln!(
            "{} block device(s) could not be opened (permission denied); run as root to see them all",
            found.denied
        );
    }
    found
        .pools
        .into_values()
        .map(|m| m.into_iter().map(|c| c.path).collect())
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn scanned() -> Vec<Vec<PathBuf>> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_follows_the_verdict_and_the_flags() {
        use Decision::*;
        assert_eq!(gate(Verdict::Healthy, false, false, false), Attach { rw: false });
        assert_eq!(gate(Verdict::Healthy, true, false, false), Attach { rw: true });
        assert!(matches!(gate(Verdict::Degraded, false, false, false), Refuse(w) if w.contains("--degraded")));
        assert_eq!(gate(Verdict::Degraded, false, true, false), ReadOnly);
        assert_eq!(gate(Verdict::Degraded, false, false, true), ReadOnly);
        assert!(matches!(gate(Verdict::Suspect, false, true, false), Refuse(w) if w.contains("--force")));
        assert_eq!(gate(Verdict::Suspect, false, false, true), ReadOnly);
        assert!(matches!(gate(Verdict::Failed, false, true, true), Refuse(w) if w.contains("failed")));
        // Writing needs healthy, whatever the flags.
        for v in [Verdict::Degraded, Verdict::Suspect, Verdict::Failed] {
            assert!(
                matches!(gate(v, true, true, true), Refuse(w) if w.contains("writing needs")),
                "{v:?}"
            );
        }
        assert!(refusal_note(Verdict::Suspect, false, "spaces attach").ends_with("spaces attach --force"));
        assert!(refusal_note(Verdict::Failed, false, "spaces attach").contains("--bundle"));
    }
}
