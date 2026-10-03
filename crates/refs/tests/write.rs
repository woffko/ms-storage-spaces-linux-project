//! Writing (Track B4) on the fixtures, in memory: commits change what was
//! asked and nothing else, every page the new checkpoint reaches is marked
//! used in its allocator, and nothing the old checkpoint reaches is
//! written before the new checkpoint, which is written last, between
//! flushes (so a crash leaves the old volume or the new one).

mod common;

use std::collections::BTreeSet;
use std::fs::File;
use std::path::Path;
use std::sync::{Arc, Mutex};

use refs::node::Node;
use refs::page::{PAGE_HEADER_SIZE, PageRef};
use refs::volume::{ROOT_DIRECTORY, SUPERBLOCK_LCN};
use refs::{Times, Volume};
use storage_spaces::io::{DeviceEvent, Overlay, ReadAt, Recorder, SparseImage};

/// A fixture, its manifest and the objects it leaves out.
fn load(name: &str) -> (SparseImage, serde_json::Value, Vec<u64>) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let manifest = common::manifest(&std::fs::read_to_string(dir.join("manifest.json")).unwrap());
    let image = SparseImage::read_from(File::open(dir.join("disk.fixture")).unwrap()).unwrap();
    let skip = manifest["fixture_excluded_objects"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|v| v.as_u64().unwrap())
        .collect();
    (image, manifest, skip)
}

/// Every page cluster the current checkpoint reaches, with the allocator
/// that should hold it (None: physical tables, not in an allocator).
fn pages<D: ReadAt>(vol: &Volume<D>, skip: &[u64]) -> Vec<(u64, Option<usize>)> {
    fn walk<D: ReadAt>(
        vol: &Volume<D>,
        r: &PageRef,
        physical: bool,
        allocator: Option<usize>,
        out: &mut Vec<(u64, Option<usize>)>,
    ) {
        for &lcn in &r.lcns[..(vol.page_size / vol.cluster) as usize] {
            out.push((if physical { lcn } else { vol.translate(lcn).unwrap() }, allocator));
        }
        let page = vol.read_page(r, physical).unwrap();
        let node = Node::at(&page, PAGE_HEADER_SIZE).unwrap();
        if !node.is_leaf() {
            for row in node.rows() {
                walk(
                    vol,
                    &PageRef::parse(row.unwrap().value).unwrap(),
                    physical,
                    allocator,
                    out,
                );
            }
        }
    }
    let mut out = Vec::new();
    for (i, r) in vol.checkpoint.roots.iter().enumerate() {
        let (physical, allocator) = match i {
            7 | 8 | 12 => (true, None),
            1 | 2 | 6 | 11 => (false, Some(2)),
            _ => (false, Some(1)),
        };
        walk(vol, r, physical, allocator, &mut out);
    }
    for oid in vol.object_ids().collect::<Vec<_>>() {
        // Objects 7 and 8 name the container tables' physical clusters.
        if oid != 7 && oid != 8 && !skip.contains(&oid) {
            walk(vol, &vol.object(oid).unwrap().clone(), false, Some(1), &mut out);
        }
    }
    out
}

/// Checks every page the checkpoint reaches the way Windows does: the row
/// area parses row by row (8-aligned sizes, removed rows included) up to
/// its end, every key index entry names a live row, the count matches,
/// and the free bytes are the area less the live rows.
fn assert_pages_valid<D: ReadAt>(vol: &Volume<D>, skip: &[u64], what: &str) {
    fn check(page: &[u8], at: &str) {
        let u32_at = |o: usize| u32::from_le_bytes(page[o..o + 4].try_into().unwrap()) as usize;
        let h = PAGE_HEADER_SIZE + u32_at(PAGE_HEADER_SIZE);
        let (start, end, free, index, count) = (
            u32_at(h),
            u32_at(h + 4),
            u32_at(h + 8),
            u32_at(h + 0x10),
            u32_at(h + 0x14),
        );
        let mut rows = std::collections::BTreeMap::new();
        let mut o = start;
        while o < end {
            let size = u32_at(h + o);
            assert!(
                size >= 0x10 && size % 8 == 0 && o + size <= end,
                "{at}: row at {o:#x} of {size:#x} bytes"
            );
            rows.insert(
                o,
                (
                    size,
                    u16::from_le_bytes(page[h + o + 8..h + o + 10].try_into().unwrap()),
                ),
            );
            o += size;
        }
        assert_eq!(o, end, "{at}: rows end at {o:#x}, the header says {end:#x}");
        let mut live = 0;
        let index_node = page[h + 0x0c] > 0;
        for i in 0..count {
            let e = u32_at(h + index + 4 * i) & 0xffff;
            let (size, flags) = rows
                .get(&e)
                .copied()
                .unwrap_or_else(|| panic!("{at}: entry {i} names no row ({e:#x})"));
            assert_eq!(flags & 4, 0, "{at}: entry {i} names a removed row");
            // Index nodes: the last row, and only it, has flag 2 (Windows
            // takes a node without it for a damaged page).
            if index_node {
                assert_eq!(
                    flags & 2 != 0,
                    i + 1 == count,
                    "{at}: entry {i} of {count}: row flags {flags:#x}"
                );
            }
            live += size;
        }
        assert_eq!(free, index - start - live, "{at}: free bytes");
    }
    fn walk<D: ReadAt>(vol: &Volume<D>, r: &PageRef, physical: bool, at: &str) {
        let page = vol.read_page(r, physical).unwrap();
        check(&page, at);
        let node = Node::at(&page, PAGE_HEADER_SIZE).unwrap();
        if !node.is_leaf() {
            for row in node.rows() {
                walk(vol, &PageRef::parse(row.unwrap().value).unwrap(), physical, at);
            }
        }
    }
    for (i, r) in vol.checkpoint.roots.iter().enumerate() {
        walk(vol, r, matches!(i, 7 | 8 | 12), &format!("{what}: root {i}"));
    }
    for oid in vol.object_ids().collect::<Vec<_>>() {
        if oid != 7 && oid != 8 && !skip.contains(&oid) {
            walk(
                vol,
                &vol.object(oid).unwrap().clone(),
                false,
                &format!("{what}: object {oid:#x}"),
            );
        }
    }
}

/// The clusters an allocator marks used.
fn used<D: ReadAt>(vol: &Volume<D>, allocator: usize) -> BTreeSet<u64> {
    let mut out = BTreeSet::new();
    vol.walk(&vol.checkpoint.roots[allocator].clone(), false, &mut |row| {
        let v = row.value;
        let (start, count) = (
            u64::from_le_bytes(v[0..8].try_into().unwrap()),
            u64::from_le_bytes(v[8..16].try_into().unwrap()),
        );
        match u16::from_le_bytes(v[0x12..0x14].try_into().unwrap()) {
            1 => {
                for j in 0..count {
                    if v[0x18 + (j / 8) as usize] >> (j % 8) & 1 != 0 {
                        out.insert(start + j);
                    }
                }
            }
            // A range without a bitmap: 0xffff at 0x16 when wholly used.
            _ if v[0x16..0x18] == [0xff, 0xff] => out.extend(start..start + count),
            _ => {}
        }
        Ok(())
    })
    .unwrap();
    out
}

fn assert_allocated<D: ReadAt>(vol: &Volume<D>, skip: &[u64], what: &str) {
    let by = [(1, used(vol, 1)), (2, used(vol, 2))];
    for (lcn, allocator) in pages(vol, skip) {
        if let Some(a) = allocator {
            assert!(
                by[a - 1].1.contains(&lcn),
                "{what}: page cluster {lcn:#x} not used in allocator {a}"
            );
        }
    }
}

/// Everything the current checkpoint reaches (pages, file data, the
/// superblocks and the checkpoint itself).
fn reachable<D: ReadAt>(vol: &Volume<D>, skip: &[u64]) -> BTreeSet<u64> {
    let mut out: BTreeSet<u64> = pages(vol, skip).into_iter().map(|p| p.0).collect();
    let total = vol.boot.volume_size() / vol.cluster;
    out.extend([SUPERBLOCK_LCN, total - 2, total - 3, vol.checkpoint.lcn]);
    let mut dirs = vec![ROOT_DIRECTORY];
    while let Some(oid) = dirs.pop() {
        for e in vol.read_dir(oid).unwrap() {
            if let refs::Target::Directory(child) = e.target
                && e.attributes & 0x400 == 0
                && !skip.contains(&child)
            {
                dirs.push(child);
            }
            let file = vol.open_file(&e).unwrap();
            for s in file
                .data
                .iter()
                .chain(file.streams.iter().chain(&file.snapshots).map(|(_, s)| s))
            {
                if let refs::Content::Extents(x) = &s.content {
                    for x in x.iter().filter(|x| x.written) {
                        let lcn = vol.translate(x.vlcn).unwrap();
                        out.extend(lcn..lcn + x.clusters);
                    }
                }
            }
        }
    }
    out
}

#[test]
fn windows_volumes_mark_every_page_used() {
    for name in [
        "r314basic4k",
        "r314basic64k",
        "r314sha",
        "r314integ",
        "r314feat",
        "r314empty",
    ] {
        let (image, manifest, skip) = load(name);
        let vol = Volume::open(&image, manifest["partition_offset"].as_u64().unwrap()).unwrap();
        assert_allocated(&vol, &skip, name);
        assert_pages_valid(&vol, &skip, name);
    }
}

#[test]
fn setting_times_and_attributes_commits_copy_on_write() {
    for (name, path) in [
        ("r314basic4k", "/names/plain.txt"),
        ("r314basic64k", "/sizes/size_100.bin"),
    ] {
        let (image, mut manifest, skip) = load(name);
        let offset = manifest["partition_offset"].as_u64().unwrap();
        let overlay = Overlay::new(&image);
        let log = Arc::new(Mutex::new(Vec::new()));
        let dev = Recorder::new(&overlay, 0, log.clone());
        let mut vol = Volume::open(&dev, offset).unwrap();
        let (cluster, before, old_checkpoint, clock) = (
            vol.cluster,
            reachable(&vol, &skip),
            vol.checkpoint.lcn,
            vol.checkpoint.clock,
        );

        let times = Times {
            created: 126_211_698_840_000_000,
            modified: 126_211_698_850_000_000,
            changed: 126_211_698_860_000_000,
            accessed: 126_211_698_870_000_000,
        };
        vol.set_times(path, &times).unwrap();

        // Pages to free clusters, flush, the other checkpoint slot, flush.
        let events = log.lock().unwrap().clone();
        let n = events.len();
        assert!(
            matches!(events[n - 1], DeviceEvent::Flush { .. }),
            "{name}: ends with a flush"
        );
        let DeviceEvent::Write {
            offset: at, ref data, ..
        } = events[n - 2]
        else {
            panic!("{name}: the checkpoint is the last write")
        };
        let slot = (at - offset) / cluster;
        assert_eq!(data.len() as u64, cluster, "{name}: one checkpoint cluster");
        assert!(
            slot != old_checkpoint && vol.checkpoint_lcns.contains(&slot),
            "{name}: the other checkpoint slot"
        );
        assert!(
            matches!(events[n - 3], DeviceEvent::Flush { .. }),
            "{name}: pages flushed before the checkpoint"
        );
        for e in &events[..n - 3] {
            let DeviceEvent::Write { offset: at, data, .. } = e else {
                panic!("{name}: one flush before the checkpoint")
            };
            for lcn in (at - offset) / cluster..(at - offset + data.len() as u64).div_ceil(cluster) {
                assert!(
                    !before.contains(&lcn),
                    "{name}: cluster {lcn:#x} of the old volume written"
                );
            }
        }

        // The new volume: as asked, consistent, the next clock.
        assert_eq!(vol.checkpoint.lcn, slot);
        assert_eq!(vol.checkpoint.clock, clock + 1);
        assert_eq!(vol.lookup(path).unwrap().times, times, "{name}");
        assert_allocated(&vol, &skip, name);
        assert_pages_valid(&vol, &skip, name);
        let fresh = Volume::open(&overlay, offset).unwrap();
        let listed = path.trim_start_matches('/');
        for e in manifest["entries"].as_array_mut().unwrap() {
            if e["path"] == listed {
                e["created"] = times.created.into();
                e["written"] = times.modified.into();
            }
        }
        assert!(
            common::compare(&fresh, &manifest).is_empty(),
            "{name}: only the times changed"
        );

        // Again (attributes now): the checkpoints alternate.
        vol.set_attributes(path, 0x21).unwrap();
        assert_eq!(vol.checkpoint.lcn, old_checkpoint);
        assert_eq!(vol.checkpoint.clock, clock + 2);
        assert_eq!(vol.lookup(path).unwrap().attributes, 0x21);
        assert_allocated(&vol, &skip, name);
    }
}

fn read_all<D: ReadAt>(vol: &Volume<D>, path: &str) -> Vec<u8> {
    let data = vol.open_file(&vol.lookup(path).unwrap()).unwrap().data.unwrap();
    let mut buf = vec![0u8; data.size as usize];
    assert_eq!(vol.read_stream(&data, 0, &mut buf).unwrap(), buf.len());
    buf
}

#[test]
fn overwriting_data_in_place() {
    let (image, mut manifest, skip) = load("r314basic4k");
    let offset = manifest["partition_offset"].as_u64().unwrap();
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, offset).unwrap();
    let now = 133_000_000_000_000_000;
    // In extents (across a cluster boundary), and inline in the record.
    for (path, at, len) in [("/sizes/size_65537.bin", 4000, 3000), ("/sizes/size_100.bin", 10, 20)] {
        let mut expected = read_all(&vol, path);
        let inline = matches!(
            vol.open_file(&vol.lookup(path).unwrap()).unwrap().data.unwrap().content,
            refs::Content::Inline(_)
        );
        assert_eq!(inline, path.ends_with("100.bin"), "{path}: where its data is");
        let bytes = vec![0x77u8; len];
        expected[at..at + len].copy_from_slice(&bytes);
        vol.overwrite(path, at as u64, &bytes, now).unwrap();
        assert_eq!(read_all(&vol, path), expected, "{path}");
        let times = vol.lookup(path).unwrap().times;
        assert_eq!((times.modified, times.changed), (now, now), "{path}");
        let listed = path.trim_start_matches('/');
        for e in manifest["entries"].as_array_mut().unwrap() {
            if e["path"] == listed {
                let hex: String = refs::checksum::sha256(&expected)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                e["sha256"] = hex.into();
                e["written"] = now.into();
            }
        }
    }
    assert_allocated(&vol, &skip, "r314basic4k");
    assert_pages_valid(&vol, &skip, "overwritten");
    let fresh = Volume::open(&overlay, offset).unwrap();
    assert!(
        common::compare(&fresh, &manifest).is_empty(),
        "only the two files changed"
    );
    // Beyond the end, and an integrity stream: refused, nothing written.
    let err = vol.overwrite("/sizes/size_100.bin", 95, &[1; 10], now).unwrap_err();
    assert!(matches!(err, refs::Error::Unsupported(_)), "{err}");
    let (image, manifest, _) = load("r314integ");
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, manifest["partition_offset"].as_u64().unwrap()).unwrap();
    let err = vol.overwrite("/sizes/size_16385.bin", 0, &[1; 10], now).unwrap_err();
    assert!(matches!(err, refs::Error::Unsupported(_)), "{err}");
    assert!(overlay.written_pages().is_empty());
}

#[test]
fn creating_files() {
    let (image, manifest, skip) = load("r314basic4k");
    let offset = manifest["partition_offset"].as_u64().unwrap();
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, offset).unwrap();
    let now = 133_100_000_000_000_000;
    let mut listed = manifest["entries"].as_array().unwrap().clone();
    for (path, data) in [
        ("/new file.txt", b"made on Linux\n".to_vec()),
        ("/names/Zebra", Vec::new()),
        ("/deep/a/b/aa.bin", vec![0xa5; 1000]),
    ] {
        vol.create_file(path, &data, now).unwrap();
        let e = vol.lookup(path).unwrap();
        assert_eq!(
            (e.size, e.attributes, e.times.modified, e.times.created),
            (data.len() as u64, 0x20, now, now),
            "{path}"
        );
        assert_eq!(read_all(&vol, path), data, "{path}");
        let hex: String = refs::checksum::sha256(&data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        listed.push(serde_json::json!({
            "path": path.trim_start_matches('/'), "kind": "file", "attributes": 0x20,
            "created": now, "written": now, "size": data.len(), "sha256": hex,
        }));
    }
    // The directories written to got the new times.
    for dir in ["", "names", "deep/a/b"] {
        for e in listed.iter_mut().filter(|e| e["path"] == dir) {
            e["written"] = now.into();
        }
    }
    let mut changed = manifest.clone();
    changed["entries"] = listed.into();
    assert_allocated(&vol, &skip, "created");
    assert_pages_valid(&vol, &skip, "created");
    let fresh = Volume::open(&overlay, offset).unwrap();
    let problems = common::compare(&fresh, &changed);
    assert!(problems.is_empty(), "{problems:?}");
    // Refused before anything is written.
    let written = overlay.written_pages().len();
    for (path, why) in [
        ("/new file.txt", "an existing name"),
        ("/кириллица2.txt", "a name that is not ASCII"),
    ] {
        let err = vol.create_file(path, b"x", now).unwrap_err();
        assert!(matches!(err, refs::Error::Unsupported(_)), "{why}: {err}");
    }
    assert_eq!(overlay.written_pages().len(), written);
}

#[test]
fn deleting_and_renaming_files() {
    let (image, manifest, skip) = load("r314basic4k");
    let offset = manifest["partition_offset"].as_u64().unwrap();
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, offset).unwrap();
    let now = 133_200_000_000_000_000;
    vol.delete_file("/sizes/size_1.bin", now).unwrap();
    vol.rename("/sizes/size_100.bin", "renamed (100).bin", now).unwrap();
    // Many rounds in one directory: the holes rows leave are reused.
    for i in 0..40 {
        let path = format!("/sizes/round {i}.txt");
        vol.create_file(&path, &vec![i as u8; 900], now).unwrap();
        vol.rename(&path, &format!("round {i} renamed.txt"), now).unwrap();
        vol.delete_file(&format!("/sizes/round {i} renamed.txt"), now).unwrap();
    }
    let mut entries: Vec<serde_json::Value> = manifest["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["path"] != "sizes/size_1.bin")
        .cloned()
        .collect();
    for e in entries.iter_mut() {
        if e["path"] == "sizes/size_100.bin" {
            e["path"] = "sizes/renamed (100).bin".into();
        }
        if e["path"] == "sizes" {
            e["written"] = now.into();
        }
    }
    let mut changed = manifest.clone();
    changed["entries"] = entries.into();
    let fresh = Volume::open(&overlay, offset).unwrap();
    let problems = common::compare(&fresh, &changed);
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(fresh.lookup("/sizes/renamed (100).bin").unwrap().times.changed, now);
    assert_allocated(&fresh, &skip, "deleted and renamed");
    assert_pages_valid(&fresh, &skip, "deleted and renamed");
    // Data in clusters Windows counted as one used block (a uniform
    // allocator row): the block becomes a bitmap row, the clusters free.
    vol.delete_file("/sizes/size_65537.bin", now).unwrap();
    assert_allocated(&vol, &skip, "freed from a uniform row");
    assert_pages_valid(&vol, &skip, "freed from a uniform row");
    // Refused: cloned clusters, an existing target name.
    for err in [
        vol.delete_file("/clones/copy1.bin", now).unwrap_err(),
        vol.rename("/sizes/size_0.bin", "size_1000.bin", now).unwrap_err(),
    ] {
        assert!(matches!(err, refs::Error::Unsupported(_)), "{err}");
    }
}

#[test]
fn creating_files_in_extents() {
    let (image, manifest, skip) = load("r314basic4k");
    let offset = manifest["partition_offset"].as_u64().unwrap();
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, offset).unwrap();
    let before = reachable(&vol, &skip);
    let now = 133_300_000_000_000_000;
    let data: Vec<u8> = (0..300 * 1024u32).map(|i| (i * 7 + i / 4096) as u8).collect();
    for (path, len) in [("/sizes/five thousand.bin", 5000), ("/names/large.bin", data.len())] {
        vol.create_file(path, &data[..len], now).unwrap();
        assert_eq!(read_all(&vol, path), &data[..len], "{path}");
        let file = vol.open_file(&vol.lookup(path).unwrap()).unwrap();
        let refs::Content::Extents(extents) = &file.data.unwrap().content else {
            panic!("{path}: inline")
        };
        let used = used(&vol, 1);
        for x in extents {
            // Windows' write path needs runs that do not cross the file's
            // cluster 64 (nor, as Windows writes them, a multiple of 256).
            let end = x.vcn + x.clusters;
            assert!(
                !(x.vcn < 64 && end > 64) && x.vcn / 256 == (end - 1) / 256,
                "{path}: run {x:?}"
            );
            let lcn = vol.translate(x.vlcn).unwrap();
            for c in lcn..lcn + x.clusters {
                assert!(
                    used.contains(&c),
                    "{path}: data cluster {c:#x} not used in the allocator"
                );
                assert!(!before.contains(&c), "{path}: data cluster {c:#x} was in use");
            }
        }
    }
    assert_allocated(&vol, &skip, "created in extents");
    assert_pages_valid(&vol, &skip, "created in extents");
    // Deleted again on a volume with cloned files: their clusters are
    // not shared, so they become free.
    let file = vol.open_file(&vol.lookup("/names/large.bin").unwrap()).unwrap();
    let refs::Content::Extents(extents) = &file.data.unwrap().content else {
        panic!("large.bin: inline")
    };
    let large: Vec<u64> = extents
        .iter()
        .flat_map(|x| {
            let lcn = vol.translate(x.vlcn).unwrap();
            lcn..lcn + x.clusters
        })
        .collect();
    vol.delete_file("/names/large.bin", now).unwrap();
    let used1 = used(&vol, 1);
    assert!(large.iter().all(|c| !used1.contains(c)), "large.bin's clusters freed");
    assert_allocated(&vol, &skip, "deleted in extents");
    assert_pages_valid(&vol, &skip, "deleted in extents");
}

#[test]
fn deleting_and_renaming_files_in_extents() {
    let (image, manifest, skip) = load("r314small");
    let offset = manifest["partition_offset"].as_u64().unwrap();
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, offset).unwrap();
    let now = 133_400_000_000_000_000;
    let runs = |vol: &Volume<&Overlay<&SparseImage>>, path: &str| -> Vec<u64> {
        let file = vol.open_file(&vol.lookup(path).unwrap()).unwrap();
        let refs::Content::Extents(x) = file.data.unwrap().content else {
            panic!("{path}: inline")
        };
        x.iter()
            .flat_map(|x| {
                let lcn = vol.translate(x.vlcn).unwrap();
                lcn..lcn + x.clusters
            })
            .collect()
    };
    // A file Windows wrote: renamed, then deleted; its clusters are free.
    vol.rename("/dir/inner.txt", "inner renamed.txt", now).unwrap();
    let clusters = runs(&vol, "/mid.bin");
    vol.delete_file("/mid.bin", now).unwrap();
    let used1 = used(&vol, 1);
    assert!(clusters.iter().all(|c| !used1.contains(c)), "mid.bin's clusters freed");
    assert!(matches!(vol.lookup("/mid.bin"), Err(refs::Error::NotFound(_))));
    // Created and deleted again: the allocators are as before.
    let before = (used(&vol, 1), used(&vol, 2).len());
    vol.create_file("/dir/scratch.bin", &vec![7u8; 300_000], now).unwrap();
    vol.delete_file("/dir/scratch.bin", now).unwrap();
    assert_eq!(used(&vol, 1).len(), before.0.len(), "medium allocator as before");
    assert_eq!(used(&vol, 2).len(), before.1, "container allocator as before");
    assert_allocated(&vol, &skip, "small");
    assert_pages_valid(&vol, &skip, "small");
    let fresh = Volume::open(&overlay, offset).unwrap();
    assert_eq!(
        fresh
            .read_dir(ROOT_DIRECTORY)
            .unwrap()
            .iter()
            .filter(|e| e.name == "mid.bin")
            .count(),
        0
    );
    let inner = fresh.lookup("/dir/inner renamed.txt").unwrap();
    assert_eq!(read_all(&fresh, "/dir/inner renamed.txt").len() as u64, inner.size);
}

#[test]
fn created_files_take_ids_past_the_directory_counter() {
    let (image, manifest, _) = load("r314small");
    let offset = manifest["partition_offset"].as_u64().unwrap();
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, offset).unwrap();
    let counter = |vol: &Volume<&Overlay<&SparseImage>>, table: usize| {
        let mut c = 0;
        vol.walk(&vol.checkpoint.roots[table].clone(), false, &mut |row| {
            if u64::from_le_bytes(row.key[8..16].try_into().unwrap()) == ROOT_DIRECTORY {
                c = u64::from_le_bytes(row.value[0x50..0x58].try_into().unwrap());
            }
            Ok(())
        })
        .unwrap();
        c
    };
    let id = |vol: &Volume<&Overlay<&SparseImage>>, path: &str| match vol.lookup(path).unwrap().target {
        refs::Target::Embedded(r) => u64::from_le_bytes(r[0x80..0x88].try_into().unwrap()),
        _ => panic!("{path}"),
    };
    let before = counter(&vol, 0);
    // Deleting the file with the highest id does not free its id.
    vol.delete_file("/last.txt", 1).unwrap();
    vol.create_file("/a.txt", b"a", 1).unwrap();
    vol.create_file("/b.txt", b"b", 1).unwrap();
    assert_eq!((id(&vol, "/a.txt"), id(&vol, "/b.txt")), (before + 1, before + 2));
    assert_eq!(
        (counter(&vol, 0), counter(&vol, 5)),
        (before + 2, before + 2),
        "both object tables"
    );
}

#[test]
fn creating_directories() {
    let (image, manifest, skip) = load("r314small");
    let offset = manifest["partition_offset"].as_u64().unwrap();
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, offset).unwrap();
    let now = 133_500_000_000_000_000;
    let objects = vol.object_ids().count();
    vol.create_directory("/made on linux", now).unwrap();
    vol.create_directory("/made on linux/inner", now).unwrap();
    vol.create_file("/made on linux/a.txt", b"inside", now).unwrap();
    vol.create_file("/made on linux/inner/b.bin", &vec![3u8; 70_000], now)
        .unwrap();
    vol.rename("/made on linux/a.txt", "a renamed.txt", now).unwrap();
    vol.delete_file("/made on linux/inner/b.bin", now).unwrap();
    assert_eq!(vol.object_ids().count(), objects + 2, "two new objects");
    let e = vol.lookup("/made on linux").unwrap();
    assert!(e.is_dir() && e.times.created == now, "{e:?}");
    let names: Vec<String> = vol
        .read_dir(vol_dir(&vol, "/made on linux"))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["a renamed.txt", "inner"]);
    assert_eq!(read_all(&vol, "/made on linux/a renamed.txt"), b"inside");
    assert!(vol.read_dir(vol_dir(&vol, "/made on linux/inner")).unwrap().is_empty());
    // The parent-child table names the new directories under their parents.
    let mut links = Vec::new();
    vol.walk(&vol.checkpoint.roots[4].clone(), false, &mut |row| {
        links.push((
            u64::from_le_bytes(row.key[8..16].try_into().unwrap()),
            u64::from_le_bytes(row.key[24..32].try_into().unwrap()),
        ));
        Ok(())
    })
    .unwrap();
    let outer = vol_dir(&vol, "/made on linux");
    assert!(
        links.contains(&(ROOT_DIRECTORY, outer)) && links.contains(&(outer, vol_dir(&vol, "/made on linux/inner")))
    );
    assert_allocated(&vol, &skip, "directories");
    assert_pages_valid(&vol, &skip, "directories");
    // Everything Windows wrote reads as before.
    let fresh = Volume::open(&overlay, offset).unwrap();
    let mut changed = manifest.clone();
    let entries = changed["entries"].as_array_mut().unwrap();
    entries.push(
        serde_json::json!({"path": "made on linux", "kind": "dir", "attributes": 0x10, "created": now, "written": now}),
    );
    entries.push(serde_json::json!({"path": "made on linux/inner", "kind": "dir", "attributes": 0x10, "created": now, "written": now}));
    entries.push(serde_json::json!({"path": "made on linux/a renamed.txt", "kind": "file", "attributes": 0x20, "created": now, "written": now, "size": 6,
        "sha256": refs::checksum::sha256(b"inside").iter().map(|b| format!("{b:02x}")).collect::<String>()}));
    let problems = common::compare(&fresh, &changed);
    assert!(problems.is_empty(), "{problems:?}");
}

fn vol_dir<D: ReadAt>(vol: &Volume<D>, path: &str) -> u64 {
    match vol.lookup(path).unwrap().target {
        refs::Target::Directory(oid) => oid,
        _ => panic!("{path}: no directory"),
    }
}

#[test]
fn replacing_file_contents() {
    let (image, manifest, skip) = load("r314small");
    let offset = manifest["partition_offset"].as_u64().unwrap();
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, offset).unwrap();
    let now = 133_600_000_000_000_000;
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let mut small = read_all(&vol, "/small.txt");
    small.extend(b" and more");
    let mid_clusters: Vec<u64> = {
        let file = vol.open_file(&vol.lookup("/mid.bin").unwrap()).unwrap();
        let refs::Content::Extents(x) = file.data.unwrap().content else {
            panic!()
        };
        x.iter()
            .flat_map(|x| {
                let lcn = vol.translate(x.vlcn).unwrap();
                lcn..lcn + x.clusters
            })
            .collect()
    };
    for (path, data) in [
        ("/small.txt", small.clone()),       // inline, appended
        ("/last.txt", big[..5000].to_vec()), // inline to clusters
        ("/mid.bin", b"short now".to_vec()), // clusters to inline
        ("/dir/inner.txt", big.clone()),     // clusters, larger
    ] {
        let before = vol.lookup(path).unwrap();
        vol.write_file(path, &data, now).unwrap();
        let after = vol.lookup(path).unwrap();
        assert_eq!(read_all(&vol, path), data, "{path}");
        assert_eq!(
            (after.size, after.attributes, after.times.created, after.times.modified),
            (data.len() as u64, before.attributes, before.times.created, now),
            "{path}"
        );
    }
    let used = used(&vol, 1);
    assert!(
        mid_clusters.iter().all(|c| !used.contains(c)),
        "mid.bin's old clusters freed"
    );
    assert_allocated(&vol, &skip, "rewritten");
    assert_pages_valid(&vol, &skip, "rewritten");
}

#[test]
fn filling_directories_splits_their_pages() {
    const COUNT: usize = 150;
    let (image, manifest, skip) = load("r314basic4k");
    let offset = manifest["partition_offset"].as_u64().unwrap();
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, offset).unwrap();
    let now = 133_700_000_000_000_000;
    // A directory of one page grows to a tree (its root splits, then its
    // leaves), and a directory that is a tree already takes more names.
    let names: Vec<String> = (0..COUNT)
        .map(|i| format!("/deep/a/b/c/d/e/f/g/h/file {i:03}.txt"))
        .collect();
    for (i, path) in names.iter().enumerate() {
        vol.create_file(path, &vec![i as u8; 900], now).unwrap();
    }
    vol.create_file("/many/added.txt", b"to a large directory", now)
        .unwrap();
    for (i, path) in names.iter().enumerate() {
        assert_eq!(read_all(&vol, path), vec![i as u8; 900], "{path}");
    }
    assert_eq!(read_all(&vol, "/many/added.txt"), b"to a large directory");
    let dir = vol_dir(&vol, "/deep/a/b/c/d/e/f/g/h");
    assert_eq!(
        vol.read_dir(dir).unwrap().len(),
        COUNT + 1,
        "leaf.bin and the new files"
    );
    // Some go again (none empties a page).
    for path in names.iter().step_by(7) {
        vol.delete_file(path, now).unwrap();
    }
    assert_eq!(vol.read_dir(dir).unwrap().len(), COUNT + 1 - COUNT.div_ceil(7));
    assert_allocated(&vol, &skip, "split");
    assert_pages_valid(&vol, &skip, "split");
    let fresh = Volume::open(&overlay, offset).unwrap();
    assert_eq!(read_all(&fresh, &names[1]), vec![1u8; 900]);
    // All go: emptied pages leave the tree, its root takes the rows of its
    // last child, and their clusters are free again.
    let pages_before = used(&vol, 1).len();
    for path in names.iter().enumerate().filter(|(i, _)| i % 7 != 0).map(|(_, p)| p) {
        vol.delete_file(path, now).unwrap();
    }
    assert_eq!(vol.read_dir(dir).unwrap().len(), 1, "leaf.bin");
    let root = vol.read_page(vol.object(dir).unwrap(), false).unwrap();
    assert!(Node::at(&root, PAGE_HEADER_SIZE).unwrap().is_leaf(), "one page again");
    let below = u64::from_le_bytes(
        root[PAGE_HEADER_SIZE + 0x18..PAGE_HEADER_SIZE + 0x20]
            .try_into()
            .unwrap(),
    );
    assert_eq!(below, 0, "no pages below the root");
    assert!(used(&vol, 1).len() < pages_before, "pages freed");
    assert_allocated(&vol, &skip, "merged");
    assert_pages_valid(&vol, &skip, "merged");
    vol.create_file("/deep/a/b/c/d/e/f/g/h/again.txt", b"again", now)
        .unwrap();
    assert_eq!(read_all(&vol, "/deep/a/b/c/d/e/f/g/h/again.txt"), b"again");
}

#[test]
fn writes_wait_for_windows_to_replay_its_log() {
    // A volume detached without a checkpoint over its log (Windows would
    // replay the log over anything written now): every write is refused.
    let (image, manifest, _) = load("r314integ");
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, manifest["partition_offset"].as_u64().unwrap()).unwrap();
    assert!(vol.log_state().unwrap().needs_replay());
    let err = vol.create_file("/new.txt", b"x", 1).unwrap_err();
    assert!(matches!(err, refs::Error::Unsupported(_)), "{err}");
    let err = vol.set_attributes("/sizes/size_100.bin", 0x21).unwrap_err();
    assert!(matches!(err, refs::Error::Unsupported(_)), "{err}");
    assert!(overlay.written_pages().is_empty());
}

#[test]
fn moving_and_linking_files() {
    let (image, manifest, skip) = load("r314small");
    let offset = manifest["partition_offset"].as_u64().unwrap();
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, offset).unwrap();
    let now = 133_800_000_000_000_000;
    let (small, last) = (read_all(&vol, "/small.txt"), read_all(&vol, "/last.txt"));
    vol.move_file("/small.txt", "/dir/small moved.txt", now).unwrap();
    vol.link_file("/last.txt", "/dir/last link.txt", now).unwrap();
    assert!(matches!(vol.lookup("/small.txt"), Err(refs::Error::NotFound(_))));
    assert_eq!(read_all(&vol, "/dir/small moved.txt"), small);
    assert_eq!(read_all(&vol, "/last.txt"), last);
    assert_eq!(read_all(&vol, "/dir/last link.txt"), last);
    // Both names lead to one record (the home directory's row 0x40).
    let (a, b) = (
        vol.lookup("/last.txt").unwrap().target,
        vol.lookup("/dir/last link.txt").unwrap().target,
    );
    assert!(matches!(a, refs::Target::Split { .. }) && a == b, "{a:?} {b:?}");
    // Their names renamed, moved, added and deleted (the record stays in
    // its home, its link rows follow).
    vol.rename("/dir/small moved.txt", "small renamed.txt", now).unwrap();
    vol.move_file("/dir/small renamed.txt", "/small back.txt", now).unwrap();
    vol.rename("/dir/LAST LINK.TXT", "last renamed.txt", now).unwrap();
    vol.link_file("/dir/last renamed.txt", "/third.txt", now).unwrap();
    vol.delete_file("/last.txt", now).unwrap();
    assert_eq!(read_all(&vol, "/small back.txt"), small);
    for path in ["/dir/last renamed.txt", "/third.txt"] {
        assert_eq!(read_all(&vol, path), last, "{path}");
    }
    let names = |vol: &Volume<_>, path: &str| {
        let record = vol.record(&vol.lookup(path).unwrap()).unwrap();
        u32::from_le_bytes(record[0x98..0x9c].try_into().unwrap())
    };
    assert_eq!(names(&vol, "/third.txt"), 2);
    // Attributes and data through one name change the record both names
    // share (and the index entry of the name used).
    vol.set_attributes("/third.txt", 0x21).unwrap();
    assert_eq!(vol.lookup("/third.txt").unwrap().attributes & 1, 1);
    let record = vol.record(&vol.lookup("/dir/last renamed.txt").unwrap()).unwrap();
    assert_eq!(record[0x48] & 1, 1, "read-only in the record");
    vol.overwrite("/dir/last renamed.txt", 0, b"XY", now).unwrap();
    assert_eq!(&read_all(&vol, "/third.txt")[..2], b"XY");
    let big: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
    vol.write_file("/third.txt", &big, now).unwrap();
    assert_eq!(read_all(&vol, "/dir/last renamed.txt"), big);
    assert_eq!(vol.lookup("/third.txt").unwrap().size, 5000);
    assert_eq!(names(&vol, "/third.txt"), 2);
    vol.write_file("/dir/last renamed.txt", &last, now).unwrap();
    assert_eq!(read_all(&vol, "/third.txt"), last);
    for gone in [
        "/last.txt",
        "/dir/small moved.txt",
        "/dir/small renamed.txt",
        "/dir/last link.txt",
    ] {
        assert!(matches!(vol.lookup(gone), Err(refs::Error::NotFound(_))), "{gone}");
    }
    // A file in clusters linked, then both names deleted: its record and
    // file id row go, its clusters become free.
    let clusters = {
        let file = vol.open_file(&vol.lookup("/mid.bin").unwrap()).unwrap();
        let refs::Content::Extents(x) = file.data.unwrap().content else {
            panic!("mid.bin: inline")
        };
        x.iter()
            .flat_map(|x| {
                let lcn = vol.translate(x.vlcn).unwrap();
                lcn..lcn + x.clusters
            })
            .collect::<Vec<_>>()
    };
    vol.link_file("/mid.bin", "/dir/mid link.bin", now).unwrap();
    vol.delete_file("/mid.bin", now).unwrap();
    assert_eq!(names(&vol, "/dir/mid link.bin"), 1);
    assert!(clusters.iter().all(|c| used(&vol, 1).contains(c)), "kept while named");
    vol.delete_file("/dir/mid link.bin", now).unwrap();
    assert!(
        clusters.iter().all(|c| !used(&vol, 1).contains(c)),
        "freed with the last name"
    );
    // The root keeps the records of small.txt and last.txt (their home).
    let fresh = Volume::open(&overlay, offset).unwrap();
    let refs::Target::Directory(dir) = fresh.lookup("/dir").unwrap().target else {
        panic!("/dir")
    };
    for (oid, expected) in [(ROOT_DIRECTORY, 2), (dir, 0)] {
        let rows = fresh.object_rows(oid).unwrap();
        let records = rows.iter().filter(|(k, _)| k[0] == 0x40).count();
        let split_ids = rows
            .iter()
            .filter(|(k, v)| k[0] == 0x20 && v.len() == 24 && v[0] == 2)
            .count();
        assert_eq!(
            (records, split_ids),
            (expected, expected),
            "{oid:#x}: records apart, their id rows"
        );
    }
    assert_allocated(&vol, &skip, "moved");
    assert_pages_valid(&vol, &skip, "moved");
}

#[test]
fn deep_directories_split_and_merge_index_pages() {
    const COUNT: usize = 500;
    let (image, manifest, skip) = load("r314small");
    let offset = manifest["partition_offset"].as_u64().unwrap();
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, offset).unwrap();
    let now = 133_900_000_000_000_000;
    // Long names make large index rows: the root index fills and splits
    // (a third level), then index pages below it split.
    vol.create_directory("/wide", now).unwrap();
    let names: Vec<String> = (0..COUNT)
        .map(|i| format!("/wide/{i:04} {}", "x".repeat(245)))
        .collect();
    for (i, path) in names.iter().enumerate() {
        vol.create_file(path, format!("file {i}").as_bytes(), now).unwrap();
    }
    let dir = vol_dir(&vol, "/wide");
    let level = |vol: &Volume<_>| {
        let root = vol.read_page(vol.object(dir).unwrap(), false).unwrap();
        let node = Node::at(&root, PAGE_HEADER_SIZE).unwrap();
        let below = u64::from_le_bytes(
            root[PAGE_HEADER_SIZE + 0x18..PAGE_HEADER_SIZE + 0x20]
                .try_into()
                .unwrap(),
        );
        (node.level, below, node.len())
    };
    // A root split leaves two children; a third comes from a page below
    // the root that split.
    let (depth, pages, children) = level(&vol);
    assert!(
        depth >= 2 && children >= 3,
        "three levels or more, index pages split: level {depth}, {children} children, {pages} pages below the root"
    );
    assert_eq!(vol.read_dir(dir).unwrap().len(), COUNT);
    for (i, path) in names.iter().enumerate().step_by(37) {
        assert_eq!(read_all(&vol, path), format!("file {i}").as_bytes(), "{path}");
    }
    assert_allocated(&vol, &skip, "deep");
    assert_pages_valid(&vol, &skip, "deep");
    // Emptied again, out of order: pages merge and leave, levels go.
    let mut order: Vec<usize> = (0..COUNT).collect();
    order.sort_by_key(|&i| (i * 7919) % COUNT);
    for (n, &i) in order.iter().enumerate() {
        vol.delete_file(&names[i], now).unwrap();
        if n % 200 == 199 {
            assert_pages_valid(&vol, &skip, "emptying");
        }
    }
    assert_eq!(vol.read_dir(dir).unwrap().len(), 0);
    assert_eq!(level(&vol), (0, 0, 1), "one page again (the directory's own row)");
    assert_allocated(&vol, &skip, "emptied");
    assert_pages_valid(&vol, &skip, "emptied");
}

#[test]
fn renaming_moving_and_deleting_directories() {
    let (image, manifest, skip) = load("r314small");
    let offset = manifest["partition_offset"].as_u64().unwrap();
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, offset).unwrap();
    let now = 134_000_000_000_000_000;
    let before = (used(&vol, 1).len(), used(&vol, 2).len());
    vol.create_directory("/a", now).unwrap();
    vol.create_directory("/a/b", now).unwrap();
    vol.create_file("/a/b/f.txt", b"in b", now).unwrap();
    let b = vol_dir(&vol, "/a/b");
    // Renamed, then moved elsewhere: the object stays, its entry and its
    // parent-child row follow.
    vol.rename("/a", "a2", now).unwrap();
    vol.move_file("/a2/b", "/dir/b2", now).unwrap();
    assert_eq!(vol_dir(&vol, "/dir/b2"), b);
    assert_eq!(read_all(&vol, "/dir/b2/f.txt"), b"in b");
    for gone in ["/a", "/a2/b"] {
        assert!(matches!(vol.lookup(gone), Err(refs::Error::NotFound(_))), "{gone}");
    }
    let parent_of = |vol: &Volume<_>, oid: u64| {
        let mut parents = Vec::new();
        vol.walk(&vol.checkpoint.roots[4].clone(), false, &mut |row| {
            if u64::from_le_bytes(row.key[24..32].try_into().unwrap()) == oid {
                parents.push(u64::from_le_bytes(row.key[8..16].try_into().unwrap()));
            }
            Ok(())
        })
        .unwrap();
        parents
    };
    assert_eq!(parent_of(&vol, b), vec![vol_dir(&vol, "/dir")]);
    // Not below itself, not while it holds files.
    for err in [
        vol.move_file("/dir", "/dir/b2/dir", now).unwrap_err(),
        vol.delete_file("/dir/b2", now).unwrap_err(),
    ] {
        assert!(matches!(err, refs::Error::Unsupported(_)), "{err}");
    }
    // Emptied and deleted: no object, no parent-child row, pages free.
    vol.delete_file("/dir/b2/f.txt", now).unwrap();
    vol.delete_file("/dir/b2", now).unwrap();
    vol.delete_file("/a2", now).unwrap();
    assert!(matches!(vol.lookup("/dir/b2"), Err(refs::Error::NotFound(_))));
    assert!(vol.object(b).is_err());
    assert!(parent_of(&vol, b).is_empty());
    assert_eq!(
        (used(&vol, 1).len(), used(&vol, 2).len()),
        before,
        "allocators as before"
    );
    assert_allocated(&vol, &skip, "directories");
    assert_pages_valid(&vol, &skip, "directories");
    let fresh = Volume::open(&overlay, offset).unwrap();
    assert_eq!(read_all(&fresh, "/dir/inner.txt").len(), 3000);
}

#[test]
fn writing_named_streams() {
    let (image, manifest, skip) = load("r314small");
    let offset = manifest["partition_offset"].as_u64().unwrap();
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, offset).unwrap();
    let now = 134_100_000_000_000_000;
    let streams = |vol: &Volume<_>, path: &str| -> Vec<(String, Vec<u8>)> {
        let file = vol.open_file(&vol.lookup(path).unwrap()).unwrap();
        file.streams
            .iter()
            .map(|(n, s)| {
                let refs::Content::Inline(b) = &s.content else {
                    panic!("{n}: in clusters")
                };
                (n.clone(), b.clone())
            })
            .collect()
    };
    let data = read_all(&vol, "/small.txt");
    vol.write_stream("/small.txt", "s1", &[1; 100], now).unwrap();
    vol.write_stream("/small.txt", "Zz", b"zz", now).unwrap();
    vol.write_stream("/small.txt", "S1", &[2; 300], now).unwrap();
    assert_eq!(
        streams(&vol, "/small.txt"),
        vec![("s1".into(), vec![2; 300]), ("Zz".into(), b"zz".to_vec())],
        "replaced without case, in name order"
    );
    assert_eq!(read_all(&vol, "/small.txt"), data);
    // They stay with the file through renames, moves and new content.
    vol.rename("/small.txt", "streams.txt", now).unwrap();
    vol.write_file("/streams.txt", &[3; 5000], now).unwrap();
    vol.move_file("/streams.txt", "/dir/streams.txt", now).unwrap();
    assert_eq!(read_all(&vol, "/dir/streams.txt"), vec![3; 5000]);
    assert_eq!(streams(&vol, "/dir/streams.txt").len(), 2);
    vol.delete_stream("/dir/streams.txt", "zz", now).unwrap();
    assert_eq!(streams(&vol, "/dir/streams.txt"), vec![("s1".into(), vec![2; 300])]);
    for err in [
        vol.delete_stream("/dir/streams.txt", "zz", now).unwrap_err(),
        vol.write_stream("/dir/streams.txt", "a:b", b"x", now).unwrap_err(),
    ] {
        assert!(
            matches!(err, refs::Error::NotFound(_) | refs::Error::Unsupported(_)),
            "{err}"
        );
    }
    vol.delete_file("/dir/streams.txt", now).unwrap();
    assert_allocated(&vol, &skip, "streams");
    assert_pages_valid(&vol, &skip, "streams");
}

#[test]
fn writing_integrity_streams() {
    let (image, manifest, skip) = load("r314small");
    let offset = manifest["partition_offset"].as_u64().unwrap();
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, offset).unwrap();
    let now = 134_200_000_000_000_000;
    let checksummed = |vol: &Volume<_>, path: &str| {
        let file = vol.open_file(&vol.lookup(path).unwrap()).unwrap();
        match file.data.unwrap().content {
            refs::Content::Extents(x) => x.iter().all(|x| x.checksums.is_some()),
            refs::Content::Inline(_) => false,
        }
    };
    // As Set-FileIntegrity does: on an empty file, then data with a
    // CRC32-C per cluster (the reader checks them).
    vol.create_file("/i.bin", b"", now).unwrap();
    vol.set_integrity("/i.bin", true).unwrap();
    assert_eq!(vol.lookup("/i.bin").unwrap().attributes & 0x8000, 0x8000);
    let mut data: Vec<u8> = (0..10_000u32).map(|i| (i * 7 % 253) as u8).collect();
    vol.write_file("/i.bin", &data, now).unwrap();
    assert!(checksummed(&vol, "/i.bin"));
    assert_eq!(read_all(&vol, "/i.bin"), data);
    // Overwritten: copied with new checksums.
    vol.overwrite("/i.bin", 5000, b"XYZ", now).unwrap();
    data[5000..5003].copy_from_slice(b"XYZ");
    assert!(checksummed(&vol, "/i.bin"));
    assert_eq!(read_all(&vol, "/i.bin"), data);
    // Small again: inline, still an integrity stream.
    vol.write_file("/i.bin", b"small", now).unwrap();
    assert_eq!(vol.lookup("/i.bin").unwrap().attributes & 0x8000, 0x8000);
    vol.write_file("/i.bin", &data, now).unwrap();
    assert!(checksummed(&vol, "/i.bin"));
    for err in [
        vol.set_integrity("/i.bin", false).unwrap_err(),
        vol.write_file("/i.bin", &vec![0; 3 << 20], now).unwrap_err(),
    ] {
        assert!(matches!(err, refs::Error::Unsupported(_)), "{err}");
    }
    let fresh = Volume::open(&overlay, offset).unwrap();
    assert_eq!(read_all(&fresh, "/i.bin"), data);
    assert_allocated(&vol, &skip, "integrity");
    assert_pages_valid(&vol, &skip, "integrity");
}

#[test]
fn deleting_named_streams_in_clusters() {
    let (image, manifest, skip) = load("r314basic4k");
    let offset = manifest["partition_offset"].as_u64().unwrap();
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, offset).unwrap();
    let now = 134_300_000_000_000_000;
    // Windows put streams/host.txt:big (200000 bytes) in clusters, a
    // stream set of its record.
    let big_clusters = |vol: &Volume<_>| {
        let file = vol.open_file(&vol.lookup("/streams/host.txt").unwrap()).unwrap();
        let (_, s) = file.streams.iter().find(|(n, _)| n == "big").unwrap();
        let refs::Content::Extents(x) = &s.content else {
            panic!("big: inline")
        };
        x.iter()
            .flat_map(|x| {
                let lcn = vol.translate(x.vlcn).unwrap();
                lcn..lcn + x.clusters
            })
            .collect::<Vec<_>>()
    };
    let clusters = big_clusters(&vol);
    let data = read_all(&vol, "/streams/host.txt");
    // The file keeps it through a rename and new content.
    vol.rename("/streams/host.txt", "host2.txt", now).unwrap();
    vol.rename("/streams/host2.txt", "host.txt", now).unwrap();
    vol.write_file("/streams/host.txt", &data, now).unwrap();
    assert_eq!(big_clusters(&vol), clusters);
    vol.delete_stream("/streams/host.txt", "BIG", now).unwrap();
    let file = vol.open_file(&vol.lookup("/streams/host.txt").unwrap()).unwrap();
    assert_eq!(
        file.streams.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        ["small"]
    );
    let used1 = used(&vol, 1);
    assert!(
        clusters.iter().all(|c| !used1.contains(c)),
        "the stream's clusters are free"
    );
    // A file with a stream in clusters deleted: those clusters go too.
    vol.write_stream("/streams/host.txt", "small", b"x", now).unwrap();
    assert_allocated(&vol, &skip, "streams");
    assert_pages_valid(&vol, &skip, "streams");
}

#[test]
fn interrupted_writes_leave_the_old_or_the_new_volume() {
    // Each operation's writes, replayed up to every point (and the last
    // write torn in half), as a power cut would leave them: the volume
    // opens as it was before or, once the checkpoint is down, after.
    let (image, manifest, skip) = load("r314small");
    let offset = manifest["partition_offset"].as_u64().unwrap();
    let now = 134_400_000_000_000_000;
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    type Op<'a> = Box<dyn Fn(&mut Volume<&Recorder<&Overlay<&SparseImage>>>) + 'a>;
    let ops: Vec<(&str, Op)> = vec![
        ("create", Box::new(|v| v.create_file("/new.bin", &big, now).unwrap())),
        ("mkdir", Box::new(move |v| v.create_directory("/made", now).unwrap())),
        ("delete", Box::new(move |v| v.delete_file("/mid.bin", now).unwrap())),
        (
            "link",
            Box::new(move |v| v.link_file("/last.txt", "/dir/l.txt", now).unwrap()),
        ),
        (
            "stream",
            Box::new(move |v| v.write_stream("/small.txt", "s", b"stream", now).unwrap()),
        ),
    ];
    let listing = |vol: &Volume<&Overlay<&SparseImage>>| {
        let mut names: Vec<String> = Vec::new();
        for dir in [ROOT_DIRECTORY, vol_dir(vol, "/dir")] {
            names.extend(vol.read_dir(dir).unwrap().into_iter().map(|e| e.name));
        }
        names.sort();
        names
    };
    for (what, op) in ops {
        let overlay = Overlay::new(&image);
        let log = Arc::new(Mutex::new(Vec::new()));
        let dev = Recorder::new(&overlay, 0, log.clone());
        let mut vol = Volume::open(&dev, offset).unwrap();
        let before = (listing(&Volume::open(&overlay, offset).unwrap()), vol.checkpoint.clock);
        op(&mut vol);
        let after = (listing(&Volume::open(&overlay, offset).unwrap()), vol.checkpoint.clock);
        assert_ne!(before, after, "{what}: changed something");
        let writes: Vec<(u64, Arc<[u8]>)> = log
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                DeviceEvent::Write { offset, data, .. } => Some((*offset, data.clone())),
                DeviceEvent::Flush { .. } => None,
            })
            .collect();
        for k in 0..=writes.len() {
            for torn in [false, true] {
                if torn && k == writes.len() {
                    continue;
                }
                let replay = Overlay::new(&image);
                for (at, data) in &writes[..k] {
                    storage_spaces::io::WriteAt::write_all_at(&replay, data, *at).unwrap();
                }
                if torn {
                    let (at, data) = &writes[k];
                    storage_spaces::io::WriteAt::write_all_at(&replay, &data[..data.len() / 2], *at).unwrap();
                }
                let vol = Volume::open(&replay, offset).unwrap_or_else(|e| {
                    panic!(
                        "{what}: {k} of {} writes{}: {e}",
                        writes.len(),
                        if torn { " and half" } else { "" }
                    )
                });
                let state = (listing(&vol), vol.checkpoint.clock);
                assert!(
                    state == before || state == after,
                    "{what}: {k} of {} writes: neither the old nor the new volume",
                    writes.len()
                );
                // New once the checkpoint is down (a torn checkpoint whose
                // written half holds all that differs counts as down).
                let down = k == writes.len() || (torn && k + 1 == writes.len());
                assert!(
                    down || state == before,
                    "{what}: new after {k} of {} writes",
                    writes.len()
                );
                assert!(k < writes.len() || state == after, "{what}: old after all writes");
                if k % 7 == 0 || k + 1 >= writes.len() {
                    assert_pages_valid(&vol, &skip, what);
                    assert_allocated(&vol, &skip, what);
                }
            }
        }
    }
}

#[test]
fn writing_named_streams_in_clusters() {
    let (image, manifest, skip) = load("r314small");
    let offset = manifest["partition_offset"].as_u64().unwrap();
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, offset).unwrap();
    let now = 134_500_000_000_000_000;
    let stream = |vol: &Volume<_>, path: &str, name: &str| -> (Vec<u8>, Vec<u64>) {
        let file = vol.open_file(&vol.lookup(path).unwrap()).unwrap();
        let (_, s) = file.streams.iter().find(|(n, _)| n == name).unwrap();
        let mut data = vec![0u8; s.size as usize];
        vol.read_stream(s, 0, &mut data).unwrap();
        let clusters = match &s.content {
            refs::Content::Extents(x) => x
                .iter()
                .flat_map(|x| {
                    let lcn = vol.translate(x.vlcn).unwrap();
                    lcn..lcn + x.clusters
                })
                .collect(),
            refs::Content::Inline(_) => Vec::new(),
        };
        (data, clusters)
    };
    let set_counter = |vol: &Volume<_>, path: &str| {
        let record = vol.record(&vol.lookup(path).unwrap()).unwrap();
        u32::from_le_bytes(record[0x9c..0xa0].try_into().unwrap())
    };
    let a: Vec<u8> = (0..20_000u32).map(|i| (i % 249) as u8).collect();
    let b: Vec<u8> = (0..7_000u32).map(|i| (i % 13) as u8).collect();
    vol.write_stream("/small.txt", "a", &a, now).unwrap();
    assert_eq!(set_counter(&vol, "/small.txt"), 0xf000);
    vol.write_stream("/small.txt", "b", &b, now).unwrap();
    assert_eq!(set_counter(&vol, "/small.txt"), 0xf001);
    let (got, a_clusters) = stream(&vol, "/small.txt", "a");
    assert_eq!(got, a);
    assert!(!a_clusters.is_empty());
    assert_eq!(stream(&vol, "/small.txt", "b").0, b);
    // Rewritten (shorter, still in clusters): the old clusters go free, the
    // set id stays; then small enough to stay in the record.
    vol.write_stream("/small.txt", "A", &a[..5000], now).unwrap();
    assert_eq!(stream(&vol, "/small.txt", "a").0, &a[..5000]);
    let used1 = used(&vol, 1);
    assert!(a_clusters.iter().all(|c| !used1.contains(c)), "old clusters freed");
    assert_eq!(set_counter(&vol, "/small.txt"), 0xf001);
    vol.write_stream("/small.txt", "b", b"tiny", now).unwrap();
    assert_eq!(stream(&vol, "/small.txt", "b"), (b"tiny".to_vec(), Vec::new()));
    // The file keeps them through new content and a move, and takes their
    // clusters along when it goes.
    vol.write_file("/small.txt", &[9; 3000], now).unwrap();
    vol.move_file("/small.txt", "/dir/streams.txt", now).unwrap();
    let (got, clusters) = stream(&vol, "/dir/streams.txt", "a");
    assert_eq!(got, &a[..5000]);
    vol.delete_file("/dir/streams.txt", now).unwrap();
    let used1 = used(&vol, 1);
    assert!(clusters.iter().all(|c| !used1.contains(c)), "freed with the file");
    assert_allocated(&vol, &skip, "streams in clusters");
    assert_pages_valid(&vol, &skip, "streams in clusters");
}
