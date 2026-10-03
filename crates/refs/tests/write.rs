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
