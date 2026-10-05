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
//! State lives in `/run/storage-spaces/<space guid>.state`.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use storage_spaces::gpt::read_partitions;
use storage_spaces::segments::SegmentKind;
use storage_spaces::{Pool, Space, SpaceReader};

pub const STATE_DIR: &str = "/run/storage-spaces";

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
}

impl State {
    fn path(space_guid: &str) -> PathBuf {
        Path::new(STATE_DIR).join(format!("{space_guid}.state"))
    }

    fn save(&self) -> Result<()> {
        fs::create_dir_all(STATE_DIR)?;
        let mut text = format!(
            "space={}\npool={}\nbackend={}\n",
            self.space_guid, self.pool_guid, self.backend
        );
        for (key, value) in [
            ("unit", &self.unit),
            ("device", &self.device),
            ("fuse_mount", &self.fuse_mount),
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
        let tmp = Self::path(&self.space_guid).with_extension("tmp");
        fs::write(&tmp, text)?;
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
                _ => {}
            }
        }
        s
    }

    pub fn load_all() -> Vec<State> {
        let Ok(entries) = fs::read_dir(STATE_DIR) else {
            return Vec::new();
        };
        let mut states: Vec<State> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "state"))
            .filter_map(|e| fs::read_to_string(e.path()).ok())
            .map(|t| State::parse(&t))
            .collect();
        states.sort_by(|a, b| a.dm.first().cmp(&b.dm.first()));
        states
    }
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
    fs::create_dir_all(STATE_DIR)?;
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
    fs::create_dir_all(STATE_DIR)?;
    let file = File::create(Path::new(STATE_DIR).join("lock"))?;
    file.lock()?;
    Ok(file)
}

/// Whether a space is attached already.
pub fn is_attached(space: &Space) -> bool {
    State::path(&space.info.guid.to_string()).exists()
}

/// Attaches one space (read-write with `rw`). Returns the state describing
/// what was created.
pub fn attach_space(
    pool: &Pool<File>,
    space: &Space,
    paths: &[PathBuf],
    requested: Backend,
    rw: bool,
) -> Result<State> {
    let guid = space.info.guid.to_string();
    if rw && let Some(why) = pool.write_refusal(space.id())? {
        bail!("it cannot be written: {why}");
    }
    let reader = crate::open_space(pool, space.id())?;
    let sector = pool.logical_sector_size;
    let backend = choose(requested, &reader, paths, sector, rw)?;
    let name = dm_name(&pool.name, space.name());
    let unit = format!("storage-spaces-{guid}");
    let mut state = State {
        space_guid: guid.clone(),
        pool_guid: pool.guid.to_string(),
        backend: backend.name().into(),
        rw,
        ..Default::default()
    };
    let rw_arg: &[&str] = if rw { &["--rw"] } else { &[] };
    let sectors = reader.size() / 512;
    let result = (|| -> Result<()> {
        match backend {
            Backend::Dm | Backend::Auto => dm_create(&name, &dm_table(&reader, paths)?, rw)?,
            Backend::Ublk => {
                state.unit = Some(unit.clone());
                let dev = start_server(&unit, "ublk", paths, &guid, rw_arg)?;
                dm_create(&name, &format!("0 {sectors} linear {dev} 0\n"), rw)?;
                state.device = Some(dev);
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
                dm_create(&name, &format!("0 {sectors} linear {dev} 0\n"), rw)?;
            }
            Backend::Fuse => {
                state.unit = Some(unit.clone());
                let mount = Path::new(STATE_DIR).join(format!("{unit}.fuse"));
                fs::create_dir_all(&mount)?;
                state.fuse_mount = Some(mount.display().to_string());
                let file = start_server(
                    &unit,
                    "fuse",
                    paths,
                    &guid,
                    &["--mountpoint", &mount.display().to_string()],
                )?;
                let bs = sector.to_string();
                let dev = run("losetup", &["-r", "-b", &bs, "-f", "--show", &file], None)?
                    .trim()
                    .to_string();
                state.device = Some(dev.clone());
                dm_create(&name, &format!("0 {sectors} linear {dev} 0\n"), false)?;
            }
        }
        state.dm.push(name.clone());
        for p in read_partitions(&reader, sector as u64)? {
            let part = format!("{name}-p{}", p.number);
            let table = format!("0 {} linear /dev/mapper/{name} {}\n", p.length / 512, p.offset / 512);
            dm_create(&part, &table, rw)?;
            state.dm.push(part);
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
