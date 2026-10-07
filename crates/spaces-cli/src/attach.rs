//! Attaching spaces as block devices and detaching them again.
//!
//! Every attached space appears as `/dev/mapper/ss-<pool>-<space>` with one
//! `…-p<N>` device per partition, whatever backend serves it:
//!
//! * `dm`: a device-mapper table straight over the member devices (kernel
//!   only, no process); used when the space has a linear description and
//!   its sector size matches the members'.
//! * `ublk`, `nbd`, `fuse`: a `spaces serve-*` process run as a transient
//!   systemd unit, wrapped by a linear device-mapper device.
//!
//! Spaces are attached read-only unless `--rw` is given. Writable spaces
//! must pass the checks of the space writer (clean pool, healthy space, a
//! resiliency writes can keep consistent). dm serves them only for simple
//! spaces that are fully allocated and whose write-back cache holds
//! nothing (dm cannot log to the cache, and its zero targets would drop
//! writes into unallocated rows); others are served by `serve-ublk --rw`
//! or `serve-nbd --rw`, which flush when they are stopped. FUSE is
//! read-only.
//!
//! Every space is checked first (`spaces check`): only healthy ones are
//! attached as asked, others read-only with `--degraded` or `--force`, and
//! failed ones not at all. The report goes to
//! `/run/storage-spaces/reports/<space guid>.{txt,json}`.
//!
//! State lives in `/run/storage-spaces/<space guid>.state`.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use storage_spaces::gpt::read_partitions;
use storage_spaces::report::{Report, Verdict};
use storage_spaces::segments::SegmentKind;
use storage_spaces::{Pool, Space, SpaceReader};

pub const STATE_DIR: &str = "/run/storage-spaces";
/// Where the reports of the checks go, one per space.
pub const REPORT_DIR: &str = "/run/storage-spaces/reports";
/// Where attach leaves udev the properties of the devices it creates
/// (`<name>.props`, imported by contrib/udev/69-storage-spaces.rules).
pub const UDEV_DIR: &str = "/run/storage-spaces/udev";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Backend {
    Auto,
    Dm,
    Ublk,
    Nbd,
    Fuse,
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Backend::Auto => "auto",
            Backend::Dm => "dm",
            Backend::Ublk => "ublk",
            Backend::Nbd => "nbd",
            Backend::Fuse => "fuse",
        }
    }
}

/// What was set up for one space; removed in reverse order.
#[derive(Debug, Default)]
pub struct State {
    pub space_guid: String,
    pub pool_guid: String,
    pub backend: String,
    /// systemd unit of the serving process, if any.
    pub unit: Option<String>,
    /// Kernel device of the backend (ublk, nbd or loop device).
    pub device: Option<String>,
    pub fuse_mount: Option<String>,
    /// Device-mapper names, whole space first.
    pub dm: Vec<String>,
    /// Attached read-write.
    pub rw: bool,
    /// The verdict of the checks when it was attached.
    pub verdict: Option<String>,
    /// Attached read-only past a verdict other than healthy (--degraded,
    /// --force).
    pub forced: bool,
    /// The report of the checks.
    pub report: Option<String>,
}

impl State {
    fn path(space_guid: &str) -> PathBuf {
        Path::new(STATE_DIR).join(format!("{space_guid}.state"))
    }

    fn save(&self) -> Result<()> {
        ensure_state_dir()?;
        let mut text = format!(
            "space={}\npool={}\nbackend={}\n",
            self.space_guid, self.pool_guid, self.backend
        );
        for (key, value) in [
            ("unit", &self.unit),
            ("device", &self.device),
            ("fuse_mount", &self.fuse_mount),
            ("verdict", &self.verdict),
            ("report", &self.report),
        ] {
            if let Some(v) = value {
                text += &format!("{key}={v}\n");
            }
        }
        for d in &self.dm {
            text += &format!("dm={d}\n");
        }
        if self.rw {
            text += "mode=rw\n";
        }
        if self.forced {
            text += "forced=1\n";
        }
        let tmp = Self::path(&self.space_guid).with_extension("tmp");
        fs::write(&tmp, text)?;
        fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
        fs::rename(tmp, Self::path(&self.space_guid))?;
        Ok(())
    }

    fn parse(text: &str) -> State {
        let mut s = State::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.to_string();
            match key {
                "space" => s.space_guid = value,
                "pool" => s.pool_guid = value,
                "backend" => s.backend = value,
                "unit" => s.unit = Some(value),
                "device" => s.device = Some(value),
                "fuse_mount" => s.fuse_mount = Some(value),
                "dm" => s.dm.push(value),
                "mode" => s.rw = value == "rw",
                "verdict" => s.verdict = Some(value),
                "forced" => s.forced = value == "1",
                "report" => s.report = Some(value),
                _ => {}
            }
        }
        s
    }

    /// The attached spaces. A state directory this user may not read is an
    /// error, not "none attached".
    pub fn load_all() -> Result<Vec<State>> {
        load_states(Path::new(STATE_DIR))
    }
}

fn load_states(dir: &Path) -> Result<Vec<State>> {
    let denied = |e: &std::io::Error| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            " (run as root)"
        } else {
            ""
        }
    };
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => bail!("cannot read {}: {e}{}", dir.display(), denied(&e)),
    };
    let mut states = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().is_some_and(|x| x == "state") {
            match fs::read_to_string(&path) {
                Ok(text) => states.push(State::parse(&text)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => bail!("cannot read {}: {e}{}", path.display(), denied(&e)),
            }
        }
    }
    states.sort_by(|a, b| a.dm.first().cmp(&b.dm.first()));
    Ok(states)
}

/// Creates the state directory, readable by everyone (`spaces status`
/// works without root) whatever the umask (the attach unit runs with 0077),
/// and makes an existing one so. What it holds is not secret: which spaces
/// are attached and how; the NBD sockets in it are 0600 themselves.
fn ensure_state_dir() -> Result<()> {
    ensure_readable_dir(Path::new(STATE_DIR)).with_context(|| format!("cannot make {STATE_DIR}"))
}

fn ensure_readable_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    fs::DirBuilder::new().recursive(true).mode(0o755).create(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755))
}

/// Writes `report` (on one space) as `<space guid>.txt` and `.json` into
/// the report directory, readable by everyone as the state directory is;
/// returns the text's path.
pub fn write_report(report: &Report) -> Result<PathBuf> {
    write_report_in(Path::new(REPORT_DIR), report).with_context(|| format!("cannot write a report into {REPORT_DIR}"))
}

fn write_report_in(dir: &Path, report: &Report) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let guid = &report.space.as_ref().context("a report on no space")?.1;
    ensure_readable_dir(dir)?;
    for (ext, text) in [("txt", report.to_text()), ("json", report.to_json())] {
        let path = dir.join(format!("{guid}.{ext}"));
        let tmp = dir.join(format!("{guid}.{ext}.tmp"));
        fs::write(&tmp, text)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644))?;
        fs::rename(&tmp, &path)?;
    }
    Ok(dir.join(format!("{guid}.txt")))
}

/// Removes the report on space `guid`.
pub fn remove_report(guid: &str) {
    for ext in ["txt", "json"] {
        let _ = fs::remove_file(Path::new(REPORT_DIR).join(format!("{guid}.{ext}")));
    }
}

/// The spaces checked and not attached: each report's path and first line
/// (the space, its verdict and why it was refused).
pub fn refused(attached: &[State]) -> Vec<(PathBuf, String)> {
    refused_in(Path::new(REPORT_DIR), attached)
}

fn refused_in(dir: &Path, attached: &[State]) -> Vec<(PathBuf, String)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(PathBuf, String)> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "txt"))
        .filter(|p| {
            let guid = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            !attached.iter().any(|s| s.space_guid.eq_ignore_ascii_case(guid))
        })
        .filter_map(|p| {
            let first = fs::read_to_string(&p).ok()?.lines().next()?.to_string();
            Some((p, first))
        })
        .collect();
    out.sort();
    out
}

/// What the udev rule imports for a device of attached space `state`:
/// whose it is, its verdict and whether it was attached past it, and
/// SS_BACKEND=1 for the device the space is served through (ublk, nbd or
/// loop: it and its kernel partitions duplicate the space's devices).
fn props_text(state: &State, backend: bool) -> String {
    let mut s = format!(
        "SS_SPACE={}\nSS_POOL={}\nSS_VERDICT={}\nSS_FORCED={}\n",
        state.space_guid,
        state.pool_guid,
        state.verdict.as_deref().unwrap_or("healthy"),
        u8::from(state.forced)
    );
    if backend {
        s += "SS_BACKEND=1\n";
    }
    s
}

fn write_props(name: &str, state: &State, backend: bool) -> Result<()> {
    write_props_in(Path::new(UDEV_DIR), name, state, backend).with_context(|| format!("cannot write into {UDEV_DIR}"))
}

fn write_props_in(dir: &Path, name: &str, state: &State, backend: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    ensure_readable_dir(dir)?;
    let path = dir.join(format!("{name}.props"));
    let tmp = dir.join(format!("{name}.props.tmp"));
    fs::write(&tmp, props_text(state, backend))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644))?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

fn remove_props(name: &str) {
    let _ = fs::remove_file(Path::new(UDEV_DIR).join(format!("{name}.props")));
}

/// The kernel name of a device path (/dev/ublkb0: ublkb0).
fn kernel_name(dev: &str) -> &str {
    dev.rsplit('/').next().unwrap_or(dev)
}

/// Writes the properties of the device a space is served through and has
/// udev take them in for it and its partitions (it announced them before
/// they were written).
fn mark_backend(dev: &str, state: &State) -> Result<()> {
    let kernel = kernel_name(dev);
    write_props(kernel, state, true)?;
    let names = [
        format!("--sysname-match={kernel}"),
        format!("--sysname-match={kernel}p*"),
    ];
    let args = [
        "trigger",
        "--action=change",
        "--subsystem-match=block",
        &names[0],
        &names[1],
    ];
    if run("udevadm", &[&args[..], &["--settle"]].concat(), None).is_err() {
        run("udevadm", &args, None).ok();
        run("udevadm", &["settle"], None).ok();
    }
    Ok(())
}

/// The device-mapper name a space of the pool `pool_guid` gets: `base`
/// (its `dm_name`), unless an attached space of another pool has it
/// (pools of the same name, as Windows calls every pool "Storage pool" by
/// default); then the first eight hex digits of this pool's GUID follow.
/// A device of that name without an attached space's state stays an error
/// (left over from an interrupted run).
fn unique_dm_name(base: String, pool_guid: &str, attached: &[State]) -> String {
    let taken = attached
        .iter()
        .any(|s| s.dm.first() == Some(&base) && !s.pool_guid.eq_ignore_ascii_case(pool_guid));
    if !taken {
        return base;
    }
    let short: String = pool_guid.chars().filter(char::is_ascii_hexdigit).take(8).collect();
    format!("{base}-{short}")
}

/// Whether the device-mapper name `dm` is that of a space named `space`:
/// `ss-<pool>-<space>`, or with a pool GUID's eight hex digits after it.
pub fn names_space(dm: &str, space: &str) -> bool {
    let suffix = format!("-{space}");
    let unsuffixed = match dm.rsplit_once('-') {
        Some((head, tail)) if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_hexdigit()) => head,
        _ => dm,
    };
    dm.ends_with(&suffix) || unsuffixed.ends_with(&suffix)
}

/// Device-mapper name for a space: `ss-<pool>-<space>` with unsafe characters replaced.
pub fn dm_name(pool: &str, space: &str) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "_.+".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let mut name = format!("ss-{}-{}", clean(pool), clean(space));
    name.truncate(100);
    name
}

fn run(program: &str, args: &[&str], stdin: Option<&str>) -> Result<String> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = cmd.spawn().with_context(|| format!("cannot run {program}"))?;
    if let Some(input) = stdin {
        child.stdin.take().unwrap().write_all(input.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn dm_create(name: &str, table: &str, rw: bool) -> Result<()> {
    if Path::new("/dev/mapper").join(name).exists() {
        bail!(
            "device-mapper device {name} already exists (left over from an interrupted run?); remove it with `dmsetup remove {name}`"
        );
    }
    let args: &[&str] = if rw {
        &["create", name]
    } else {
        &["create", "--readonly", name]
    };
    run("dmsetup", args, Some(table))?;
    run("udevadm", &["settle"], None).ok();
    Ok(())
}

fn dm_remove(name: &str) -> Result<()> {
    if Path::new("/dev/mapper").join(name).exists() {
        run("dmsetup", &["remove", "--retry", name], None)?;
    }
    Ok(())
}

/// Logical block size of a block device (through its parent for partitions).
fn logical_block_size(dev: &Path) -> Option<u32> {
    // /dev/mapper/<name> and /dev/disk/by-* are links to the kernel name.
    let dev = fs::canonicalize(dev).ok()?;
    let name = dev.file_name()?.to_str()?;
    let sys = fs::canonicalize(Path::new("/sys/class/block").join(name)).ok()?;
    let queue = if sys.join("partition").exists() {
        sys.parent()?.join("queue")
    } else {
        sys.join("queue")
    };
    fs::read_to_string(queue.join("logical_block_size"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn dm_table(reader: &SpaceReader<'_, File>, paths: &[PathBuf]) -> Result<String> {
    let mut table = String::new();
    for seg in reader.segments()? {
        let (start, length) = (seg.start / 512, seg.length / 512);
        let line = match seg.kind {
            SegmentKind::Zero => format!("{start} {length} zero"),
            SegmentKind::Striped { stripes, .. } if stripes.len() == 1 => {
                format!(
                    "{start} {length} linear {} {}",
                    paths[stripes[0].device].display(),
                    stripes[0].offset / 512
                )
            }
            SegmentKind::Striped { chunk, stripes } => {
                let devs: Vec<String> = stripes
                    .iter()
                    .map(|s| format!("{} {}", paths[s.device].display(), s.offset / 512))
                    .collect();
                format!(
                    "{start} {length} striped {} {} {}",
                    stripes.len(),
                    chunk / 512,
                    devs.join(" ")
                )
            }
        };
        table += &line;
        table.push('\n');
    }
    Ok(table)
}

fn available(backend: Backend) -> bool {
    let has = |program: &str| run("sh", &["-c", &format!("command -v {program}")], None).is_ok();
    let module = |m: &str| Path::new("/sys/module").join(m).exists() || run("modprobe", &[m], None).is_ok();
    match backend {
        Backend::Dm | Backend::Auto => has("dmsetup"),
        Backend::Ublk => cfg!(feature = "ublk") && module("ublk_drv") && Path::new("/dev/ublk-control").exists(),
        Backend::Nbd => has("nbd-client") && module("nbd"),
        Backend::Fuse => cfg!(feature = "fuse") && Path::new("/dev/fuse").exists() && has("losetup"),
    }
}

/// Why dm cannot serve the space read-write, if it cannot.
fn dm_write_refusal(reader: &SpaceReader<'_, File>) -> Option<&'static str> {
    if reader.layout().resiliency != storage_spaces::format::Resiliency::Simple {
        return Some("dm writes one copy only; mirror and parity spaces need their logs kept");
    }
    if reader.cache().is_some_and(|c| c.cached_chunks() > 0) {
        return Some("the write-back cache holds data, which dm would not see");
    }
    match reader.segments() {
        Ok(segments) if segments.iter().any(|s| matches!(s.kind, SegmentKind::Zero)) => {
            Some("the space has unallocated rows, whose writes dm would drop")
        }
        Ok(_) => None,
        Err(_) => Some("dm cannot map this space"),
    }
}

/// Picks the backend for a space.
fn choose(
    requested: Backend,
    reader: &SpaceReader<'_, File>,
    paths: &[PathBuf],
    sector: u32,
    rw: bool,
) -> Result<Backend> {
    if rw && requested == Backend::Fuse {
        bail!("the fuse backend is read-only");
    }
    if requested != Backend::Auto {
        if requested == Backend::Dm {
            reader.segments().context("the dm backend cannot map this space")?;
            if let Some(why) = dm_write_refusal(reader).filter(|_| rw) {
                bail!("the dm backend cannot serve this space read-write: {why}");
            }
        }
        if !available(requested) {
            bail!("backend {} is not available on this system", requested.name());
        }
        return Ok(requested);
    }
    // dm maps one copy only, so it is used for simple spaces, where no
    // redundancy is lost; mirror and parity spaces keep failing over to
    // other copies when served by a process.
    let sectors_match = paths.iter().all(|p| logical_block_size(p) == Some(sector));
    let simple = reader.layout().resiliency == storage_spaces::format::Resiliency::Simple;
    let dm_ok = if rw {
        dm_write_refusal(reader).is_none()
    } else {
        reader.segments().is_ok()
    };
    if simple && dm_ok && sectors_match && available(Backend::Dm) {
        return Ok(Backend::Dm);
    }
    let fallbacks: &[Backend] = if rw {
        &[Backend::Ublk, Backend::Nbd]
    } else {
        &[Backend::Ublk, Backend::Nbd, Backend::Fuse]
    };
    for &b in fallbacks {
        if available(b) {
            return Ok(b);
        }
    }
    bail!("no usable backend (install dmsetup, or load ublk_drv or nbd)")
}

/// Waits for `path` to appear while the systemd unit `unit` runs, at most
/// `timeout`.
fn wait_for(path: &Path, unit: &str, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    let mut checked = Instant::now();
    while !path.exists() {
        if start.elapsed() > timeout {
            bail!("timed out waiting for {}", path.display());
        }
        if checked.elapsed() > Duration::from_secs(2) {
            checked = Instant::now();
            if run("systemctl", &["is-active", "--quiet", unit], None).is_err() && !path.exists() {
                bail!("the serving process ended before it was ready");
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Starts `spaces serve-<kind>` as a transient systemd unit and waits for
/// its ready file.
/// What a server parsing a pool's disks (hostile input, run as root) may
/// not do. Nothing here changes what it can open or mount.
const SERVER_RESTRICTIONS: &[&str] = &[
    "NoNewPrivileges=yes",
    "ProtectControlGroups=yes",
    "ProtectKernelLogs=yes",
    "ProtectHostname=yes",
    "RestrictNamespaces=yes",
    "RestrictRealtime=yes",
    "RestrictSUIDSGID=yes",
    "LockPersonality=yes",
    "SystemCallArchitectures=native",
    "MemoryDenyWriteExecute=yes",
    "UMask=0077",
    "RestrictAddressFamilies=AF_UNIX AF_NETLINK",
    "CapabilityBoundingSet=~CAP_SYS_PTRACE CAP_SYS_BOOT CAP_SYS_TIME CAP_SYS_PACCT CAP_SYS_TTY_CONFIG \
        CAP_SYSLOG CAP_NET_ADMIN CAP_NET_RAW CAP_AUDIT_CONTROL CAP_AUDIT_READ CAP_AUDIT_WRITE \
        CAP_LINUX_IMMUTABLE CAP_MAC_ADMIN CAP_MAC_OVERRIDE CAP_WAKE_ALARM CAP_BLOCK_SUSPEND \
        CAP_PERFMON CAP_BPF CAP_CHECKPOINT_RESTORE CAP_SETPCAP CAP_SYS_CHROOT CAP_SETUID CAP_SETGID \
        CAP_KILL",
];

/// Restrictions that give the server its own view of the file system: not
/// for the FUSE server, whose mount must be seen by the host. Home
/// directories stay readable (pool disks may be image files there).
const SERVER_PRIVATE_VIEW: &[&str] = &[
    "ProtectSystem=strict",
    "ReadWritePaths=/run",
    "ProtectHome=read-only",
    "PrivateTmp=yes",
    "ProtectKernelTunables=yes",
    "ProtectProc=invisible",
];

fn start_server(unit: &str, kind: &str, paths: &[PathBuf], space: &str, extra: &[&str]) -> Result<String> {
    ensure_state_dir()?;
    let ready = Path::new(STATE_DIR).join(format!("{unit}.ready"));
    let _ = fs::remove_file(&ready);
    let exe = std::env::current_exe()?;
    // The attach service runs early at boot (before local-fs-pre.target);
    // with default dependencies the server unit would wait for
    // sysinit.target and deadlock with it.
    let mut args: Vec<String> = vec![
        format!("--unit={unit}"),
        "--collect".into(),
        "--quiet".into(),
        "--property=DefaultDependencies=no".into(),
        "--property=Conflicts=shutdown.target".into(),
        "--property=Before=shutdown.target".into(),
    ];
    let private = if kind == "fuse" { &[][..] } else { SERVER_PRIVATE_VIEW };
    let restrictions: Vec<String> = SERVER_RESTRICTIONS
        .iter()
        .chain(private)
        .map(|p| format!("--property={p}"))
        .collect();
    let head = std::mem::take(&mut args);
    args.extend(["--".into(), exe.display().to_string()]);
    args.push(format!("serve-{kind}"));
    args.extend(paths.iter().map(|p| p.display().to_string()));
    args.extend([
        "--space".into(),
        space.into(),
        "--ready-file".into(),
        ready.display().to_string(),
    ]);
    args.extend(extra.iter().map(|s| s.to_string()));
    args.extend(crate::inherited_args());
    let launch = |limits: &[String]| {
        let all: Vec<&str> = head.iter().chain(limits).chain(&args).map(String::as_str).collect();
        run("systemd-run", &all, None)
    };
    // A systemd that does not know one of the properties (they date from
    // 231 to 247) refuses the whole unit: start the server without limits
    // rather than not at all, and say so.
    if let Err(e) = launch(&restrictions) {
        eprintln!("warning: starting {unit} with limits failed ({e:#}); starting it without");
        launch(&[])?;
    }
    // Opening a space read-write destages its write-back cache first, which
    // can take minutes.
    let timeout = Duration::from_secs(if extra.contains(&"--rw") { 3600 } else { 30 });
    if let Err(e) = wait_for(&ready, unit, timeout) {
        let _ = run("systemctl", &["stop", unit], None);
        let log = run("journalctl", &["-u", unit, "-n", "20", "--no-pager"], None).unwrap_or_default();
        bail!("{e}; unit log:\n{log}");
    }
    let content = fs::read_to_string(&ready)?;
    let _ = fs::remove_file(&ready);
    Ok(content.trim().to_string())
}

fn free_nbd() -> Result<String> {
    for i in 0..128 {
        let size = fs::read_to_string(format!("/sys/block/nbd{i}/size")).unwrap_or_default();
        if size.trim() == "0" && !Path::new(&format!("/sys/block/nbd{i}/pid")).exists() {
            return Ok(format!("/dev/nbd{i}"));
        }
    }
    bail!("no free NBD device")
}

/// Serializes attach and detach runs (udev can start one while another runs).
pub fn lock() -> Result<File> {
    ensure_state_dir()?;
    let file = File::create(Path::new(STATE_DIR).join("lock"))?;
    file.lock()?;
    Ok(file)
}

/// Whether a space is attached already.
pub fn is_attached(space: &Space) -> bool {
    State::path(&space.info.guid.to_string()).exists()
}

/// Attaches one space (read-write with `rw`) that the checks gave
/// `verdict` (their report at `report`). Returns the state describing
/// what was created.
pub fn attach_space(
    pool: &Pool<File>,
    space: &Space,
    paths: &[PathBuf],
    requested: Backend,
    rw: bool,
    verdict: Verdict,
    report: Option<&Path>,
) -> Result<State> {
    if verdict != Verdict::Healthy && rw {
        bail!("writing needs a healthy space, and this one is {}", verdict.as_str());
    }
    let guid = space.info.guid.to_string();
    if rw && let Some(why) = pool.write_refusal(space.id())? {
        bail!("it cannot be written: {why}");
    }
    let reader = crate::open_space(pool, space.id())?;
    let sector = pool.logical_sector_size;
    let backend = choose(requested, &reader, paths, sector, rw)?;
    let name = unique_dm_name(
        dm_name(&pool.name, space.name()),
        &pool.guid.to_string(),
        &State::load_all()?,
    );
    let unit = format!("storage-spaces-{guid}");
    let mut state = State {
        space_guid: guid.clone(),
        pool_guid: pool.guid.to_string(),
        backend: backend.name().into(),
        rw,
        verdict: Some(verdict.as_str().into()),
        forced: verdict != Verdict::Healthy,
        report: report.map(|p| p.display().to_string()),
        ..Default::default()
    };
    // The server checks the space too, and needs the same flag to read
    // one that is not healthy.
    let mut flags: Vec<&str> = if rw { vec!["--rw"] } else { Vec::new() };
    flags.extend(verdict.flag());
    let rw_arg: &[&str] = &flags;
    let sectors = reader.size() / 512;
    let result = (|| -> Result<()> {
        // udev learns whose each device is before it appears (the backend
        // device right after: it appears when its server starts).
        state.dm.push(name.clone());
        write_props(&name, &state, false)?;
        match backend {
            Backend::Dm | Backend::Auto => dm_create(&name, &dm_table(&reader, paths)?, rw)?,
            Backend::Ublk => {
                state.unit = Some(unit.clone());
                let dev = start_server(&unit, "ublk", paths, &guid, rw_arg)?;
                state.device = Some(dev.clone());
                mark_backend(&dev, &state)?;
                dm_create(&name, &format!("0 {sectors} linear {dev} 0\n"), rw)?;
            }
            Backend::Nbd => {
                state.unit = Some(unit.clone());
                let socket = Path::new(STATE_DIR).join(format!("{unit}.sock"));
                let _ = fs::remove_file(&socket);
                let socket_arg = socket.display().to_string();
                let mut extra = vec!["--socket", socket_arg.as_str()];
                extra.extend(rw_arg);
                start_server(&unit, "nbd", paths, &guid, &extra)?;
                let dev = free_nbd()?;
                let bs = sector.to_string();
                let mut args = vec![
                    "-N",
                    space.name(),
                    "-u",
                    socket_arg.as_str(),
                    dev.as_str(),
                    "-b",
                    bs.as_str(),
                ];
                if !rw {
                    args.push("-R");
                }
                run("nbd-client", &args, None)?;
                state.device = Some(dev.clone());
                mark_backend(&dev, &state)?;
                dm_create(&name, &format!("0 {sectors} linear {dev} 0\n"), rw)?;
            }
            Backend::Fuse => {
                state.unit = Some(unit.clone());
                let mount = Path::new(STATE_DIR).join(format!("{unit}.fuse"));
                fs::create_dir_all(&mount)?;
                state.fuse_mount = Some(mount.display().to_string());
                let mountpoint = mount.display().to_string();
                let mut extra = vec!["--mountpoint", mountpoint.as_str()];
                extra.extend(verdict.flag());
                let file = start_server(&unit, "fuse", paths, &guid, &extra)?;
                let bs = sector.to_string();
                let dev = run("losetup", &["-r", "-b", &bs, "-f", "--show", &file], None)?
                    .trim()
                    .to_string();
                state.device = Some(dev.clone());
                mark_backend(&dev, &state)?;
                dm_create(&name, &format!("0 {sectors} linear {dev} 0\n"), false)?;
            }
        }
        // Partitions become devices only when they lie inside the space
        // and apart (an attach forced past the partition check gets the
        // whole space only).
        let parts = read_partitions(&reader, sector as u64)?;
        if let Err(e) = storage_spaces::gpt::check_partitions(&parts, reader.size()) {
            eprintln!("{:?}: no devices for its partitions: {e}", space.name());
        } else {
            for p in parts {
                let part = format!("{name}-p{}", p.number);
                let table = format!("0 {} linear /dev/mapper/{name} {}\n", p.length / 512, p.offset / 512);
                state.dm.push(part.clone());
                write_props(&part, &state, false)?;
                dm_create(&part, &table, rw)?;
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = teardown(&state);
        return Err(e);
    }
    state.save()?;
    Ok(state)
}

/// Removes what `state` describes, most dependent parts first.
pub fn teardown(state: &State) -> Result<()> {
    let mut errors = Vec::new();
    for name in state.dm.iter().rev() {
        if let Err(e) = dm_remove(name) {
            errors.push(e.to_string());
        }
        remove_props(name);
    }
    match state.backend.as_str() {
        "nbd" => {
            if let Some(dev) = &state.device {
                let _ = run("nbd-client", &["-d", dev], None);
            }
        }
        "fuse" => {
            if let Some(dev) = &state.device {
                let _ = run("losetup", &["-d", dev], None);
            }
            if let Some(mount) = &state.fuse_mount {
                let _ = run("umount", &[mount], None);
            }
        }
        _ => {}
    }
    if let Some(dev) = &state.device {
        remove_props(kernel_name(dev));
    }
    if let Some(unit) = &state.unit {
        let _ = run("systemctl", &["stop", unit], None);
    }
    if let Some(mount) = &state.fuse_mount {
        let _ = fs::remove_dir(mount);
    }
    if let Some(unit) = &state.unit {
        let _ = fs::remove_file(Path::new(STATE_DIR).join(format!("{unit}.sock")));
    }
    if errors.is_empty() {
        let _ = fs::remove_file(State::path(&state.space_guid));
        remove_report(&state.space_guid);
        Ok(())
    } else {
        bail!("{}", errors.join("; "))
    }
}

#[cfg(test)]
mod hardening_tests {
    use super::*;

    #[test]
    fn server_properties_are_systemd_assignments() {
        // They go to `systemd-run --property=`: a typo would make attach
        // fail at boot, where nobody sees it.
        for p in SERVER_RESTRICTIONS.iter().chain(SERVER_PRIVATE_VIEW) {
            let (key, value) = p.split_once('=').unwrap_or_else(|| panic!("{p}"));
            assert!(
                key.chars().all(|c| c.is_ascii_alphanumeric()) && !value.is_empty(),
                "{p}"
            );
        }
        // The FUSE server must not get a private view of the file system:
        // its mount has to be visible to the host.
        assert!(SERVER_PRIVATE_VIEW.iter().any(|p| p.starts_with("ProtectSystem")));
        assert!(
            !SERVER_RESTRICTIONS
                .iter()
                .any(|p| p.starts_with("Protect") && p.contains("System"))
        );
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spaces-state-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// Whether this process reads what its permissions forbid (root).
    fn overrides_permissions(dir: &Path) -> bool {
        let probe = dir.join("probe");
        fs::write(&probe, b"x").unwrap();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o000)).unwrap();
        let read = fs::read(&probe).is_ok();
        fs::remove_file(&probe).unwrap();
        read
    }

    /// The state directory ends up readable by everyone, also when it was
    /// made 0700 before (the attach unit's umask in 1.1.0).
    #[test]
    fn the_state_directory_is_readable() {
        let dir = scratch("mode");
        ensure_readable_dir(&dir).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o755);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        ensure_readable_dir(&dir).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o755);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// What attach records of the checks reads back.
    #[test]
    fn the_verdict_and_report_are_kept_in_the_state() {
        let s = State {
            space_guid: "g".into(),
            dm: vec!["ss-p-a".into()],
            verdict: Some("suspect".into()),
            forced: true,
            report: Some("/run/storage-spaces/reports/g.txt".into()),
            ..Default::default()
        };
        let dir = scratch("verdict");
        fs::create_dir_all(&dir).unwrap();
        // As save writes it.
        let text = format!(
            "space=g\npool=\nbackend=\nverdict=suspect\nreport={}\ndm=ss-p-a\nforced=1\n",
            s.report.as_deref().unwrap()
        );
        fs::write(dir.join("g.state"), text).unwrap();
        let back = &load_states(&dir).unwrap()[0];
        assert_eq!(
            (back.verdict.as_deref(), back.forced, back.report.as_deref()),
            (s.verdict.as_deref(), true, s.report.as_deref())
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// What udev imports for a device of an attached space.
    #[test]
    fn udev_properties_say_whose_device_and_its_verdict() {
        let state = State {
            space_guid: "g".into(),
            pool_guid: "p".into(),
            verdict: Some("suspect".into()),
            forced: true,
            ..Default::default()
        };
        assert_eq!(
            props_text(&state, false),
            "SS_SPACE=g\nSS_POOL=p\nSS_VERDICT=suspect\nSS_FORCED=1\n"
        );
        assert!(props_text(&state, true).ends_with("SS_BACKEND=1\n"));
        let dir = scratch("udev");
        write_props_in(&dir, "ss-p-a", &state, false).unwrap();
        let path = dir.join("ss-p-a.props");
        assert_eq!(fs::read_to_string(&path).unwrap(), props_text(&state, false));
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Reports are files everyone may read; those of spaces not attached
    /// are what was refused.
    #[test]
    fn reports_are_written_and_refused_ones_listed() {
        let dir = scratch("reports");
        let mut r = Report::new("space \"data\" (g1) of pool \"p\" (pg)");
        r.space = Some(("data".into(), "g1".into()));
        r.action = Some("not attached: a suspect space needs --force (and is then read-only)".into());
        let path = write_report_in(&dir, &r).unwrap();
        assert_eq!(path, dir.join("g1.txt"));
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
        assert!(fs::read_to_string(dir.join("g1.json")).unwrap().contains("\"space\""));
        r.space = Some(("other".into(), "g2".into()));
        write_report_in(&dir, &r).unwrap();
        let attached = State {
            space_guid: "G2".into(),
            ..Default::default()
        };
        let refused = refused_in(&dir, &[attached]);
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].0, dir.join("g1.txt"));
        assert!(
            refused[0]
                .1
                .ends_with("not attached: a suspect space needs --force (and is then read-only)")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// No state directory: nothing attached. One this user may not read:
    /// an error that says so, not "none attached".
    #[test]
    fn unreadable_state_is_an_error() {
        let dir = scratch("denied");
        assert!(load_states(&dir).unwrap().is_empty());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.state"), "space=a\ndm=ss-p-a\n").unwrap();
        assert_eq!(load_states(&dir).unwrap()[0].dm, ["ss-p-a"]);
        if overrides_permissions(&dir) {
            eprintln!("permissions do not apply to this user (root): skipped");
        } else {
            fs::set_permissions(dir.join("a.state"), fs::Permissions::from_mode(0o000)).unwrap();
            let err = load_states(&dir).unwrap_err().to_string();
            assert!(err.contains("a.state") && err.contains("run as root"), "{err}");
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o300)).unwrap();
            let err = load_states(&dir).unwrap_err().to_string();
            assert!(err.contains("cannot read") && err.contains("run as root"), "{err}");
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod name_tests {
    use super::*;

    fn attached(dm: &str, pool: &str) -> State {
        State {
            pool_guid: pool.into(),
            dm: vec![dm.into(), format!("{dm}-p1")],
            ..Default::default()
        }
    }

    /// Two pools of the same name with a space of the same name: the second
    /// gets the first eight hex digits of its pool's GUID after the name;
    /// the same pool keeps its name, and other names are left alone.
    #[test]
    fn spaces_of_pools_of_the_same_name_get_apart() {
        let base = dm_name("Storage pool", "data");
        assert_eq!(base, "ss-Storage_pool-data");
        let first = "249b3eb3-30f0-4b3c-8261-cd3732f7f7ae";
        let other = "0735434C-B460-4A90-A3BA-B04FCE198982";
        assert_eq!(
            unique_dm_name(base.clone(), other, &[attached(&base, first)]),
            "ss-Storage_pool-data-0735434C"
        );
        assert_eq!(
            unique_dm_name(base.clone(), &first.to_uppercase(), &[attached(&base, first)]),
            base
        );
        assert_eq!(unique_dm_name(base.clone(), other, &[]), base);
        assert_eq!(
            unique_dm_name("ss-Storage_pool-games".into(), other, &[attached(&base, first)]),
            "ss-Storage_pool-games"
        );
    }

    /// `spaces detach data` finds the spaces named data, with or without
    /// the pool GUID's digits after the name.
    #[test]
    fn a_space_name_finds_its_devices() {
        assert!(names_space("ss-Storage_pool-data", "data"));
        assert!(names_space("ss-Storage_pool-data-0735434c", "data"));
        assert!(!names_space("ss-Storage_pool-metadata", "data"));
        assert!(!names_space("ss-Storage_pool-data2-0735434c", "data"));
        assert!(names_space("ss-p-backup-2024abcd", "backup-2024abcd"));
    }
}
