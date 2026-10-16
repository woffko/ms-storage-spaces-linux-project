//! `spaces check`: the guard's report on each space (storage_spaces::guard)
//! with the file systems of its partitions checked (NTFS by the library,
//! ReFS by the refs crate); on request the deep checks, and a bundle of the
//! pool's metadata for a report.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use storage_spaces::gpt::{MSR_PARTITION_TYPE, Partition};
use storage_spaces::guard::{self, Options};
use storage_spaces::health::Health;
use storage_spaces::io::ReadAt;
use storage_spaces::isolate::{self, Stopped};
use storage_spaces::report::{Check, Evidence, Report, Status, Truth, Verdict, reports_to_json};
use storage_spaces::{Pool, Space, SpaceReader};

use crate::bundle;

// The checks run on threads of their own (`isolate::isolated`), which share
// the pool.
const _: fn() = || {
    fn shared<T: Send + Sync + 'static>() {}
    shared::<Pool<File>>();
};

/// How long the quick checks of a space, and the opening of a pool, may
/// take: SPACES_CHECK_TIMEOUT seconds (0: no limit), 40 by default. Real
/// ones take a second or two; a hostile disk can make them loop, and
/// `spaces attach` checks every pool in one process.
pub fn deadline() -> Option<Duration> {
    match std::env::var("SPACES_CHECK_TIMEOUT")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        Some(0) => None,
        Some(s) => Some(Duration::from_secs(s)),
        None => Some(Duration::from_secs(40)),
    }
}

/// Opens the pool on `paths` to check it (read-only, not exclusively; its
/// warnings go into the reports), on a thread of its own: a panic or a hang
/// in reading its metadata is an error, not the end of the process.
pub fn open_pool(paths: &[PathBuf]) -> Result<Pool<File>> {
    let owned = paths.to_vec();
    let opened = isolate::isolated(deadline(), move || -> Result<Pool<File>> {
        let files = owned
            .iter()
            .map(|p| crate::open_member(p, false, false))
            .collect::<Result<Vec<File>>>()?;
        Ok(Pool::open(files)?)
    });
    opened.unwrap_or_else(|stopped| Err(anyhow::anyhow!("reading its metadata {stopped}")))
}

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

/// The health of a pool as the checks of its spaces take it, worked out
/// once for them all, on a thread of its own ([`pool_health`]).
pub type PoolHealth = Arc<Result<Result<Health, String>, Stopped>>;

/// Works out the health of `pool` for [`space_report`].
pub fn pool_health<D: ReadAt + 'static>(pool: &Arc<Pool<D>>, limit: Option<Duration>) -> PoolHealth {
    let shared = Arc::clone(pool);
    Arc::new(isolate::isolated(limit, move || guard::pool_health(&shared)))
}

/// The report on space `space` of `pool` (quick checks, as attach makes),
/// made on a thread of its own: checks that panic give a failed
/// `space.checks`, checks past the [`deadline`] a suspect one.
pub fn space_report<D: ReadAt + 'static>(
    pool: &Arc<Pool<D>>,
    health: &PoolHealth,
    paths: &[PathBuf],
    space: &Space,
) -> Report {
    report(pool, health, paths, space, false, deadline(), fs_checks)
}

/// What the file systems of a space are checked with: [`fs_checks`] (the
/// tests give others).
type FsChecks<D> = fn(&SpaceReader<'_, D>, &Partition, &[String], bool) -> Vec<Check>;

/// The report on `space` (with `deep`, the deep checks too), made on a
/// thread of its own that may take `limit`.
fn report<D: ReadAt + 'static>(
    pool: &Arc<Pool<D>>,
    health: &PoolHealth,
    paths: &[PathBuf],
    space: &Space,
    deep: bool,
    limit: Option<Duration>,
    fs: FsChecks<D>,
) -> Report {
    if let Err(stopped) = health.as_ref() {
        return stopped_report(pool, space, stopped);
    }
    let (shared, health, names, id) = (Arc::clone(pool), Arc::clone(health), names(paths), space.id());
    let work = move || {
        let space = &shared.spaces[&id];
        let options = Options {
            journal_budget: if deep { None } else { Options::default().journal_budget },
        };
        let health = health.as_ref().as_ref().expect("a stopped health returned above");
        let mut r = guard::space_report_with(&shared, space, health, &names, &options, &mut |reader, part, names| {
            fs(reader, part, names, deep)
        });
        if deep {
            r.checks.push(deep_scrub(&shared, space));
        }
        r
    };
    let mut r = isolate::isolated(limit, work).unwrap_or_else(|stopped| stopped_report(pool, space, &stopped));
    r.environment = environment(pool);
    r
}

/// The report on a space whose checks stopped.
fn stopped_report<D>(pool: &Pool<D>, space: &Space, stopped: &Stopped) -> Report {
    let mut r = guard::empty_report(pool, space);
    r.checks.push(stopped.check("space.checks", "the checks of the space"));
    r
}

/// Refuses what the gate refuses for the servers and export: an error
/// with the report's headline and the reason. (The checks run on this
/// thread; a panic in them fails the space.)
pub fn require<D: ReadAt>(
    pool: &Pool<D>,
    paths: &[PathBuf],
    space: &Space,
    rw: bool,
    degraded: bool,
    force: bool,
) -> Result<()> {
    let names = names(paths);
    let report = isolate::caught(|| {
        let mut r = guard::space_report(pool, space, &names, &Options::default(), &mut |reader, part, names| {
            fs_checks(reader, part, names, false)
        });
        r.environment = environment(pool);
        r
    })
    .unwrap_or_else(|stopped| stopped_report(pool, space, &stopped));
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
    let mut checks = refs::health::quick_checks_in(reader, part.offset, Some(part.length), &label, &locate);
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
pub fn reports<D: ReadAt + 'static>(
    pool: &Arc<Pool<D>>,
    paths: &[PathBuf],
    only: Option<&str>,
    deep: bool,
) -> Vec<Report> {
    // The deep checks read every copy: no time limit.
    let limit = if deep { None } else { deadline() };
    let health = pool_health(pool, deadline());
    pool.user_spaces()
        .filter(|s| only.is_none_or(|key| names_space(s, key)))
        .map(|space| report(pool, &health, paths, space, deep, limit, fs_checks))
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
        match open_pool(&paths) {
            Ok(pool) => {
                let pool = Arc::new(pool);
                let reports = reports(&pool, &paths, only, deep);
                if bundle_path.is_some()
                    && let Err(stopped) = isolate::caught(|| bundle_files(&pool, &reports, &mut files))
                {
                    files.push((
                        format!("pool-{}/bundle-error.txt", pool.guid),
                        format!("packing the pool's files {stopped}\n").into_bytes(),
                    ));
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

    /// Checks that panic fail their space, checks that hang past the limit
    /// make it suspect; either way the report comes back.
    #[test]
    fn checks_that_panic_or_hang_stop_their_space_only() {
        use storage_spaces::io::SparseImage;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../storage-spaces/tests/fixtures/mirror3");
        let disks: Vec<SparseImage> = (0..)
            .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
            .map(|f| SparseImage::read_from(f).unwrap())
            .collect();
        let pool = Arc::new(Pool::open(disks).unwrap());
        let space = pool.user_spaces().next().unwrap();
        let limit = Some(Duration::from_secs(30));
        let health = pool_health(&pool, limit);
        let fine = report(&pool, &health, &[], space, false, limit, |_, _, _, _| Vec::new());
        assert_eq!(fine.verdict(), Verdict::Healthy, "{}", fine.to_text());
        assert!(fine.subject.starts_with("space \""), "{}", fine.subject);

        let r = report(&pool, &health, &[], space, false, limit, |_, _, _, _| {
            panic!("a defect")
        });
        assert_eq!(r.verdict(), Verdict::Failed, "{}", r.to_text());
        let c = r.checks.iter().find(|c| c.id == "space.checks").unwrap();
        assert!(c.evidence[0].found.as_deref().unwrap().contains("a defect"), "{c:?}");
        assert_eq!(r.space, fine.space);
        assert!(!r.environment.is_empty());

        let r = report(
            &pool,
            &health,
            &[],
            space,
            false,
            Some(Duration::from_millis(200)),
            |_, _, _, _| loop {
                std::thread::sleep(Duration::from_millis(50));
            },
        );
        assert_eq!(r.verdict(), Verdict::Suspect, "{}", r.to_text());
        assert!(
            r.headline()
                .contains("space.checks: the checks of the space did not finish in 0 s")
        );
    }
}
