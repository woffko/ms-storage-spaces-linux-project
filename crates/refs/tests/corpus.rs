//! Every ReFS volume of the corpus (testdata/refs/NAME: disk.img, or the
//! pool disks disk0.img ... of a volume inside a space, and manifest.json;
//! tools/vm/New-RefsVolume.ps1 and tools/fetch-refs.sh) read back as
//! Windows listed it: every file and directory with its kind, size, data
//! (SHA-256), attributes, times, named streams and snapshots. Volumes not
//! at hand are skipped.

mod common;

use std::fs::File;
use std::path::{Path, PathBuf};

use refs::Volume;
use serde_json::Value;
use storage_spaces::Pool;
use storage_spaces::io::ReadAt;

fn corpus() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/refs");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    dirs.retain(|d| (d.join("disk.img").exists() || d.join("disk0.img").exists()) && d.join("manifest.json").exists());
    dirs.sort();
    dirs
}

/// The device of the volume: the image, or the space of the pool.
fn device(dir: &Path, manifest: &Value) -> Box<dyn ReadAt> {
    let Some(p) = manifest.get("pool").filter(|p| !p.is_null()) else {
        return Box::new(File::open(dir.join("disk.img")).unwrap());
    };
    let disks = (0..p["disks"].as_u64().unwrap())
        .map(|i| File::open(dir.join(format!("disk{i}.img"))).unwrap())
        .collect();
    let pool: &'static Pool<File> = Box::leak(Box::new(Pool::open(disks).unwrap()));
    let space = pool.find_space(p["space"].as_str().unwrap()).unwrap();
    Box::new(pool.open_space(space.id()).unwrap())
}

#[test]
fn volumes_read_back_as_windows_listed_them() {
    let volumes = corpus();
    if volumes.is_empty() {
        eprintln!("no ReFS volumes in testdata/refs; skipped");
        return;
    }
    let mut failed = Vec::new();
    for dir in volumes {
        let manifest = common::manifest(&std::fs::read_to_string(dir.join("manifest.json")).unwrap());
        let offset = manifest["partition_offset"].as_u64().unwrap();
        let vol = Volume::open(device(&dir, &manifest), offset).unwrap();
        if !common::compare(&vol, &manifest).is_empty() {
            failed.push(manifest["name"].as_str().unwrap().to_owned());
        }
    }
    assert!(failed.is_empty(), "volumes not read as Windows listed them: {failed:?}");
}
