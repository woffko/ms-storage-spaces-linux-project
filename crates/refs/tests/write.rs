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
        for i in 0..count {
            let e = u32_at(h + index + 4 * i) & 0xffff;
            let (size, flags) = rows
                .get(&e)
                .copied()
                .unwrap_or_else(|| panic!("{at}: entry {i} names no row ({e:#x})"));
            assert_eq!(flags & 4, 0, "{at}: entry {i} names a removed row");
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
        ("/many/new.txt", "a directory of several pages"),
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
    // Refused: data in extents, an existing target name.
    for err in [
        vol.delete_file("/sizes/size_65537.bin", now).unwrap_err(),
        vol.rename("/sizes/size_0.bin", "size_1000.bin", now).unwrap_err(),
    ] {
        assert!(matches!(err, refs::Error::Unsupported(_)), "{err}");
    }
}
