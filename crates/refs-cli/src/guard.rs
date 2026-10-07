//! The guard for refs: the quick checks of a ReFS volume (refs::health)
//! before it is mounted, read or written; for a device of a space `spaces
//! attach` attached, also the verdict the space got there; for `--space`,
//! the checks of the pool and the space (storage_spaces::guard).

use std::fs::File;
use std::path::{Path, PathBuf};

use storage_spaces::gpt::Partition;
use storage_spaces::io::ReadAt;
use storage_spaces::report::{Check, Evidence, Report, Status, Verdict};
use storage_spaces::{Pool, SpaceReader};

/// Where `spaces attach` keeps the state of attached spaces.
const STATE_DIR: &str = "/run/storage-spaces";
/// Where reports go (as those of `spaces attach`).
const REPORT_DIR: &str = "/run/storage-spaces/reports";

/// The file system checks of a partition of a space: NTFS and ReFS.
fn fs_hook<D: ReadAt>(reader: &SpaceReader<'_, D>, part: &Partition, names: &[String]) -> Vec<Check> {
    let label = if part.number == 0 {
        "the space".to_string()
    } else {
        format!("p{}", part.number)
    };
    let locate = |o: u64| reader.describe(o, names);
    if let Some(c) = storage_spaces::guard::ntfs_check(reader, part.offset, part.length, &label, &locate) {
        return vec![c];
    }
    refs::health::quick_checks(reader, part.offset, &label, &locate)
}

/// The report on the ReFS volume at byte `offset` of `dev`, the device
/// `path` (`None` for a space read with --space, whose checks `space`
/// gives instead).
pub fn volume_report(dev: &dyn ReadAt, offset: u64, path: &Path) -> Report {
    let name = path.display().to_string();
    let mut r = Report::new(format!("the ReFS volume on {name}"));
    r.checks = refs::health::quick_checks(dev, offset, "the volume", &|o| format!("{name} at byte {o:#x}"));
    if let Some(c) = inherited(path) {
        r.checks.insert(0, c);
    }
    r.environment.push(("refs".into(), env!("CARGO_PKG_VERSION").into()));
    r
}

/// The report on space `name` of the pool on `paths` (`spaces check`'s
/// checks, the file systems in it included).
pub fn space_report(pool: &Pool<File>, paths: &[PathBuf], name: &str) -> Option<Report> {
    let space = pool.find_space(name)?;
    let names: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    let mut r = storage_spaces::guard::space_report(
        pool,
        space,
        &names,
        &storage_spaces::guard::Options::default(),
        &mut fs_hook,
    );
    r.environment.push(("refs".into(), env!("CARGO_PKG_VERSION").into()));
    Some(r)
}

/// `space.verdict`: what the space a device belongs to got when `spaces
/// attach` attached it (each layer asks on its own: a volume in a space
/// attached past its verdict mounts only with --force too).
fn inherited(path: &Path) -> Option<Check> {
    let (dm, verdict, report) = attached_space(path)?;
    let verdict = Verdict::parse(&verdict)?;
    let status = match verdict {
        Verdict::Healthy => Status::Ok,
        Verdict::Degraded => Status::Degraded,
        Verdict::Suspect => Status::Suspect,
        Verdict::Failed => Status::Failed,
    };
    let mut c = Check::new(
        "space.verdict",
        status,
        format!("the volume is on {dm}, a space attached as {}", verdict.as_str()),
    );
    if let Some(r) = report {
        c = c.evidence(Evidence::new("the space's report").at(r));
    }
    if verdict != Verdict::Healthy {
        c = c.advice(
            "The space is not healthy (its report says why): what it holds is read only when asked, read-only.",
        );
    }
    Some(c)
}

/// The device-mapper name of the attached space `path` is (or is a
/// partition of), its verdict and the report's path, from the state of
/// `spaces attach`.
fn attached_space(path: &Path) -> Option<(String, String, Option<String>)> {
    let real = std::fs::canonicalize(path).ok()?;
    let kernel = real.file_name()?.to_str()?;
    let dm = std::fs::read_to_string(format!("/sys/block/{kernel}/dm/name")).ok()?;
    let dm = dm.trim().to_string();
    if !dm.starts_with("ss-") {
        return None;
    }
    for entry in std::fs::read_dir(STATE_DIR).ok()?.filter_map(|e| e.ok()) {
        let p = entry.path();
        if p.extension().is_none_or(|x| x != "state") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let field = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(&format!("{k}=")).map(str::to_string))
        };
        if text.lines().any(|l| l == format!("dm={dm}")) {
            return Some((dm, field("verdict")?, field("report")));
        }
    }
    None
}

/// Writes `report` into the report directory as `<name>.txt` and `.json`
/// when this process may (root); returns the text's path.
pub fn write_report(report: &Report, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let dir = Path::new(REPORT_DIR);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)
        .ok()?;
    for (ext, text) in [("txt", report.to_text()), ("json", report.to_json())] {
        let path = dir.join(format!("{name}.{ext}"));
        let tmp = dir.join(format!("{name}.{ext}.tmp"));
        std::fs::write(&tmp, text).ok()?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).ok()?;
        std::fs::rename(&tmp, &path).ok()?;
    }
    Some(dir.join(format!("{name}.txt")))
}

/// The line a refusal or warning starts with: the verdict and the worst
/// check (what udisks shows of a refused mount).
pub fn first_line(report: &Report) -> String {
    match report.problems().first() {
        Some(c) => format!("{}: {}: {}", report.verdict().as_str().to_uppercase(), c.id, c.summary),
        None => report.verdict().as_str().to_uppercase(),
    }
}

/// What the commands that only read do with a volume that is not
/// healthy: they read it, and say so.
pub fn warn(report: &Report) {
    if report.verdict() != Verdict::Healthy {
        eprintln!("warning: {} (refs check --quick tells more)", first_line(report));
    }
}

/// What writing needs: a healthy volume.
pub fn require_healthy(report: &Report) -> anyhow::Result<()> {
    if report.verdict() != Verdict::Healthy {
        anyhow::bail!(
            "{}; writing needs a healthy volume (refs check --quick tells more)",
            first_line(report)
        );
    }
    Ok(())
}
