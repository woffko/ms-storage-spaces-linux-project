//! The fixtures (tests/fixtures/NAME: disk.fixture and manifest.json, made
//! from corpus volumes by `refs fixture`) read back as Windows listed them:
//! every file and directory, the data of the small files.

mod common;

use std::fs::File;
use std::path::Path;

use refs::Volume;
use refs::volume::{ROOT_DIRECTORY, SUPERBLOCK_LCN};
use storage_spaces::io::{ReadAt, SparseImage};

#[test]
fn fixtures_read_back_as_windows_listed_them() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut dirs: Vec<_> = std::fs::read_dir(root).unwrap().map(|e| e.unwrap().path()).collect();
    dirs.sort();
    assert!(!dirs.is_empty());
    let mut failed = Vec::new();
    for dir in dirs {
        let manifest = common::manifest(&std::fs::read_to_string(dir.join("manifest.json")).unwrap());
        let image = SparseImage::read_from(File::open(dir.join("disk.fixture")).unwrap()).unwrap();
        let vol = Volume::open(image, manifest["partition_offset"].as_u64().unwrap()).unwrap();
        if !common::compare(&vol, &manifest).is_empty() {
            failed.push(manifest["name"].as_str().unwrap().to_owned());
        }
    }
    assert!(
        failed.is_empty(),
        "fixtures not read as Windows listed them: {failed:?}"
    );
}

fn load(name: &str) -> (SparseImage, u64) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let manifest = common::manifest(&std::fs::read_to_string(dir.join("manifest.json")).unwrap());
    let image = SparseImage::read_from(File::open(dir.join("disk.fixture")).unwrap()).unwrap();
    (image, manifest["partition_offset"].as_u64().unwrap())
}

/// Flips the bits of `mask` in the byte at `at`.
fn damage(image: &mut SparseImage, at: u64, mask: u8) {
    let mut b = [0u8];
    image.read_exact_at(&mut b, at).unwrap();
    image.insert(at, &[b[0] ^ mask]);
}

fn root_names<D: ReadAt>(vol: &Volume<D>) -> Vec<String> {
    vol.read_dir(ROOT_DIRECTORY)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect()
}

#[test]
fn a_damaged_page_is_an_error() {
    for name in ["r314basic4k", "r314basic64k", "r314sha"] {
        let (mut image, offset) = load(name);
        let vol = Volume::open(&image, offset).unwrap();
        let root = vol.object(ROOT_DIRECTORY).unwrap().clone();
        let at = offset + vol.translate(root.lcns[0]).unwrap() * vol.cluster + 0x123;
        drop(vol);
        damage(&mut image, at, 0x10);
        let vol = Volume::open(&image, offset).unwrap();
        let err = vol.read_dir(ROOT_DIRECTORY).unwrap_err().to_string();
        assert!(err.contains("checksum"), "{name}: {err}");
    }
}

#[test]
fn the_superblock_copies_stand_in_for_a_damaged_one() {
    for name in ["r314basic64k", "r314sha"] {
        let (mut image, offset) = load(name);
        let vol = Volume::open(&image, offset).unwrap();
        let (cluster, names) = (vol.cluster, root_names(&vol));
        let clusters = vol.boot.volume_size() / cluster;
        drop(vol);
        damage(&mut image, offset + SUPERBLOCK_LCN * cluster + 0x51, 1);
        assert_eq!(root_names(&Volume::open(&image, offset).unwrap()), names, "{name}");
        damage(&mut image, offset + (clusters - 2) * cluster + 0x51, 1);
        assert_eq!(root_names(&Volume::open(&image, offset).unwrap()), names, "{name}");
        damage(&mut image, offset + (clusters - 3) * cluster + 0x51, 1);
        let err = Volume::open(&image, offset).err().unwrap().to_string();
        assert!(err.contains("superblock"), "{name}: {err}");
    }
}

#[test]
fn a_damaged_boot_sector_is_refused() {
    let (mut image, offset) = load("r314empty");
    damage(&mut image, offset + 0x30, 4);
    let err = Volume::open(&image, offset).err().unwrap().to_string();
    assert!(err.contains("checksum"), "{err}");
}
