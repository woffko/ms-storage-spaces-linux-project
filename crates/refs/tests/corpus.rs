//! Every ReFS volume of the corpus (testdata/refs/NAME: disk.img and
//! manifest.json, tools/vm/New-RefsVolume.ps1 and tools/fetch-refs.sh)
//! read back as Windows listed it: every file and directory with its kind,
//! size, data (SHA-256), attributes, times and named streams. Volumes not
//! at hand are skipped.

mod common;

use std::fs::File;
use std::path::{Path, PathBuf};

use refs::Volume;

fn corpus() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/refs");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    dirs.retain(|d| d.join("disk.img").exists() && d.join("manifest.json").exists());
    dirs.sort();
    dirs
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
        let vol = Volume::open(File::open(dir.join("disk.img")).unwrap(), offset).unwrap();
        if !common::compare(&vol, &manifest).is_empty() {
            failed.push(manifest["name"].as_str().unwrap().to_owned());
        }
    }
    assert!(failed.is_empty(), "volumes not read as Windows listed them: {failed:?}");
}
