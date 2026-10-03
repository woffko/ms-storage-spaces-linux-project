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
fn a_damaged_compressed_unit_is_an_error() {
    // Windows compressed the files of a container (LZ4 units of 64 KiB, a
    // CRC32-C each): a byte changed in a unit's compressed bytes makes
    // reading what it holds an error, not other data.
    let (mut image, offset) = load("r314compress");
    let vol = Volume::open(&image, offset).unwrap();
    let file = vol.open_file(&vol.lookup("/text/t0.txt").unwrap()).unwrap();
    let stream = file.data.unwrap();
    let mut whole = vec![0u8; stream.size as usize];
    vol.read_stream(&stream, 0, &mut whole).unwrap();
    // The compressed bytes: container 0x60's, 5672 clusters from virtual
    // cluster 0x130000; a byte of each changed.
    let (first, cluster) = (vol.translate(0x130000).unwrap(), vol.cluster);
    drop(vol);
    for k in 0..5672 {
        damage(&mut image, offset + (first + k) * cluster + 0x100, 0x40);
    }
    let vol = Volume::open(&image, offset).unwrap();
    let err = vol.read_stream(&stream, 0, &mut whole).unwrap_err().to_string();
    assert!(err.contains("checksum"), "{err}");
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

#[test]
fn integrity_streams_refuse_damaged_data() {
    // CRC32-C per 4 KiB cluster; CRC-64 per 16 KiB of 64 KiB clusters.
    for (name, path, sums) in [
        ("r314integ", "/sizes/size_16385.bin", 5),
        ("r314integ64k", "/sizes/size_65537.bin", 8),
    ] {
        let (mut image, offset) = load(name);
        let vol = Volume::open(&image, offset).unwrap();
        let file = vol.open_file(&vol.lookup(path).unwrap()).unwrap();
        let data = file.data.unwrap();
        let refs::Content::Extents(extents) = &data.content else {
            panic!("{name}: inline data")
        };
        let checksums = extents[0].checksums.as_ref().unwrap();
        assert_eq!(checksums.values.len(), sums, "{name}");
        // Damage the second checked part of the run.
        let part = vol.cluster / checksums.per_cluster as u64;
        let at = offset + vol.translate(extents[0].vlcn).unwrap() * vol.cluster + part + 7;
        drop(vol);
        damage(&mut image, at, 0x40);
        let vol = Volume::open(&image, offset).unwrap();
        let mut buf = vec![0u8; 100];
        if part > 100 {
            assert_eq!(vol.read_stream(&data, 0, &mut buf).unwrap(), 100, "{name}");
        }
        let err = vol.read_stream(&data, part + 10, &mut buf).unwrap_err();
        assert!(matches!(err, refs::Error::Checksum(_)), "{name}: {err}");
    }
}
