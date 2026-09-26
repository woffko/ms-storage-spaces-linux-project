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

fn dm_create(name: &str, table: &str) -> Result<()> {
    run("dmsetup", &["create", "--readonly", name], Some(table))?;
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

/// Picks the backend for a space.
fn choose(requested: Backend, reader: &SpaceReader<'_, File>, paths: &[PathBuf], sector: u32) -> Result<Backend> {
    if requested != Backend::Auto {
        if requested == Backend::Dm {
            reader.segments().context("the dm backend cannot map this space")?;
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
    if simple && reader.segments().is_ok() && sectors_match && available(Backend::Dm) {
        return Ok(Backend::Dm);
    }
    for b in [Backend::Ublk, Backend::Nbd, Backend::Fuse] {
        if available(b) {
            return Ok(b);
        }
    }
    bail!("no usable backend (install dmsetup, or load ublk_drv or nbd)")
}

fn wait_for(path: &Path, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while !path.exists() {
        if start.elapsed() > timeout {
            bail!("timed out waiting for {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Starts `spaces serve-<kind>` as a transient systemd unit and waits for
/// its ready file.
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
        "--".into(),
        exe.display().to_string(),
    ];
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
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run("systemd-run", &refs, None)?;
    if let Err(e) = wait_for(&ready, Duration::from_secs(30)) {
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

/// Attaches one space. Returns the state describing what was created.
pub fn attach_space(pool: &Pool<File>, space: &Space, paths: &[PathBuf], requested: Backend) -> Result<State> {
    let guid = space.info.guid.to_string();
    let reader = crate::open_space(pool, space.id())?;
    let sector = pool.logical_sector_size;
    let backend = choose(requested, &reader, paths, sector)?;
    let name = dm_name(&pool.name, space.name());
    let unit = format!("storage-spaces-{guid}");
    let mut state = State {
        space_guid: guid.clone(),
        pool_guid: pool.guid.to_string(),
        backend: backend.name().into(),
        ..Default::default()
    };
    let sectors = reader.size() / 512;
    let result = (|| -> Result<()> {
        match backend {
            Backend::Dm | Backend::Auto => dm_create(&name, &dm_table(&reader, paths)?)?,
            Backend::Ublk => {
                state.unit = Some(unit.clone());
                let dev = start_server(&unit, "ublk", paths, &guid, &[])?;
                dm_create(&name, &format!("0 {sectors} linear {dev} 0\n"))?;
                state.device = Some(dev);
            }
            Backend::Nbd => {
                state.unit = Some(unit.clone());
                let socket = Path::new(STATE_DIR).join(format!("{unit}.sock"));
                let _ = fs::remove_file(&socket);
                start_server(&unit, "nbd", paths, &guid, &["--socket", &socket.display().to_string()])?;
                let dev = free_nbd()?;
                let bs = sector.to_string();
                run(
                    "nbd-client",
                    &[
                        "-N",
                        space.name(),
                        "-u",
                        &socket.display().to_string(),
                        &dev,
                        "-b",
                        &bs,
                        "-R",
                    ],
                    None,
                )?;
                state.device = Some(dev.clone());
                dm_create(&name, &format!("0 {sectors} linear {dev} 0\n"))?;
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
                dm_create(&name, &format!("0 {sectors} linear {dev} 0\n"))?;
            }
        }
        state.dm.push(name.clone());
        for p in read_partitions(&reader, sector as u64)? {
            let part = format!("{name}-p{}", p.number);
            let table = format!("0 {} linear /dev/mapper/{name} {}\n", p.length / 512, p.offset / 512);
            dm_create(&part, &table)?;
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
