//! Volumes of every ReFS version with bytes changed read to the end or fail
//! with an error: whatever a damaged or hostile disk holds, no panic.
//! (Test builds check every page's checksum, so most changes stop at the
//! page they hit; the fuzz targets, which accept every checksum, go deeper.
//! The parsers behind the checksums get random bytes below.)

mod forge;

use std::fs::File;
use std::path::Path;

use forge::{Forge, Tree, le16, le32, le64, place};
use refs::node::Node;
use refs::page::{PageHeader, PageRef};
use refs::volume::ROOT_DIRECTORY;
use refs::{Content, Target, Volume};
use storage_spaces::io::{ReadAt, SparseImage};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 24
    }
}

/// Reads whatever the volume offers: directories, files, streams, and the
/// consistency check.
fn exercise(image: &SparseImage, offset: u64) {
    let Ok(vol) = Volume::open(image, offset) else {
        return;
    };
    let mut dirs = vec![ROOT_DIRECTORY];
    let mut seen = 0;
    let mut buf = vec![0u8; 4096];
    while let Some(oid) = dirs.pop() {
        let Ok(entries) = vol.read_dir(oid) else {
            continue;
        };
        for e in entries {
            seen += 1;
            if seen > 400 {
                return;
            }
            if let Target::Directory(child) = e.target
                && dirs.len() < 16
            {
                dirs.push(child);
            }
            let Ok(file) = vol.open_file(&e) else {
                continue;
            };
            for s in file.data.iter().chain(file.streams.iter().map(|(_, s)| s)) {
                let _ = vol.read_stream(s, 0, &mut buf);
                if let Content::Extents(x) = &s.content {
                    let _ = x.len();
                }
            }
        }
    }
    let _ = vol.check(&[]);
}

#[test]
fn changed_bytes_never_panic() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for name in ["r12small", "r31presmall", "r34small", "r37small", "r314small"] {
        let dir = root.join(name);
        let manifest = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
        let offset = serde_json::from_str::<serde_json::Value>(manifest.trim_start_matches('\u{feff}')).unwrap()
            ["partition_offset"]
            .as_u64()
            .unwrap();
        let base = SparseImage::read_from(File::open(dir.join("disk.fixture")).unwrap()).unwrap();
        let ranges = base.ranges();
        let stored: u64 = ranges.iter().map(|r| r.1 as u64).sum();
        let mut rng = Rng(0x5eed ^ name.len() as u64);
        for round in 0..150 {
            let mut image = base.clone();
            for _ in 0..1 + round % 4 {
                let mut left = rng.next() % stored;
                for &(at, n) in &ranges {
                    if left < n as u64 {
                        let bytes: Vec<u8> = (0..1 + rng.next() % 8).map(|_| rng.next() as u8).collect();
                        image.insert(at + left, &bytes);
                        break;
                    }
                    left -= n as u64;
                }
            }
            exercise(&image, offset);
        }
    }
}

#[test]
fn random_bytes_never_panic_the_parsers() {
    let mut rng = Rng(0xbad5eed);
    for round in 0..3000 {
        let len = [0, 1, 7, 0x20, 0x30, 0x60, 0x100, 0x400, 0x4000][round % 9];
        let mut b: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        // Half of them look like a node: a small descriptor offset, counts
        // that nearly fit.
        if round % 2 == 0 && len >= 0x60 {
            b[..4].copy_from_slice(&0x30u32.to_le_bytes());
            b[0x30 + 0x10..0x30 + 0x14].copy_from_slice(&0x20u32.to_le_bytes());
            b[0x30 + 0x14..0x30 + 0x18].copy_from_slice(&((rng.next() % 4) as u32).to_le_bytes());
        }
        let _ = PageRef::parse(&b);
        let _ = PageRef::parse_v1(&b);
        let _ = PageHeader::parse(&b);
        for node in [
            Node::at(&b, 0),
            Node::at_v1(&b, 0),
            Node::at(&b, 0x30),
            Node::at_v1(&b, 0x30),
        ]
        .into_iter()
        .flatten()
        {
            for row in node.rows().take(100) {
                let Ok(row) = row else { break };
                let _ = PageRef::parse(row.value);
                let _ = PageRef::parse_v1(row.value);
            }
        }
    }
}

/// A container whose physical start is near 2^64, past the volume, or on
/// another container's clusters, its row's checksums stored again: the
/// volume is refused (1.2.1 panicked adding the start in `Volume::open`,
/// before the mount's checks could say anything).
#[test]
fn containers_outside_the_volume_or_overlapping_are_refused() {
    let base = Forge::fixture("r314small");
    for id in [2u64, 3, 4, 5] {
        for start in [u64::MAX - 0x10, 0xffff_ffff_0000_0000, 1 << 52] {
            let mut f = base.clone();
            f.set_container_start(id, start);
            let e = f
                .volume()
                .err()
                .unwrap_or_else(|| panic!("container {id:#x} at {start:#x}: opened"));
            assert!(e.to_string().contains("beyond the volume"), "{e}");
        }
    }
    let vol = base.volume().unwrap();
    let other = vol
        .translate(3 << (64 - vol.clusters_per_container.leading_zeros()))
        .unwrap();
    drop(vol);
    let mut f = base.clone();
    f.set_container_start(2, other + 1);
    let e = f.volume().err().expect("overlapping containers opened");
    assert!(e.to_string().contains("overlap"), "{e}");
}

/// A name row whose key is 2 or 3 bytes long (no room for its flags and
/// name): listing the directory fails (1.2.1 panicked slicing the key).
#[test]
fn a_name_row_with_a_short_key_is_an_error() {
    for len in [2u16, 3] {
        let mut f = Forge::fixture("r314basic64k");
        let named = |page: &[u8]| -> Option<usize> {
            let node = Node::at(page, 0x50).ok()?;
            let key = node
                .rows()
                .flatten()
                .find(|r| node.is_leaf() && r.key.len() >= 4 && le16(r.key, 0) == 0x30)?
                .key;
            Some(place(page, key))
        };
        f.patch(
            Tree::Object(ROOT_DIRECTORY),
            |page| named(page).is_some(),
            |page| {
                // The row's header: its key's offset (0x10) and length.
                let row = named(page).unwrap() - 0x10;
                assert_eq!(le16(page, row + 4), 0x10);
                page[row + 6..row + 8].copy_from_slice(&len.to_le_bytes());
            },
        );
        let vol = f.volume().unwrap();
        assert!(vol.read_dir(ROOT_DIRECTORY).is_err(), "a key of {len} bytes");
    }
}

/// Which resident attribute a test changes (ReFS 3.4's named streams are
/// in pages of their records, out of reach here: a unit test of `file`).
#[derive(Clone, Copy, Debug)]
enum Resident {
    /// $DATA in the record (ReFS 3.14).
    Data,
    /// A named stream (3.14).
    Stream,
}

impl Resident {
    /// Whether a row of a record is the attribute.
    fn is(self, k: &[u8], v: &[u8]) -> bool {
        match self {
            Resident::Data => k.len() >= 16 && le32(k, 8) == 0x8000_0001 && le32(k, 12) & 0xffff == 0x80,
            Resident::Stream => {
                k.len() >= 16
                    && le32(k, 8) == 0x8000_0002
                    && le32(k, 12) & 0xffff == 0xb0
                    && v.len() >= 0x3c
                    && le16(v, 0x10) == 0
                    && le16(v, 2) & 0x1000 == 0
            }
        }
    }
}

/// A file of the volume whose record (in its name row) has the attribute:
/// (its directory, its name, the record).
fn file_with(vol: &Volume<&SparseImage>, what: Resident) -> (u64, String, Vec<u8>) {
    let mut dirs = vec![ROOT_DIRECTORY];
    while let Some(dir) = dirs.pop() {
        for e in vol.read_dir(dir).unwrap() {
            match &e.target {
                Target::Directory(child) => dirs.push(*child),
                Target::Embedded(record) => {
                    let node = Node::at(record, 0).unwrap();
                    if node.rows().flatten().any(|r| what.is(r.key, r.value)) {
                        return (dir, e.name.clone(), record.clone());
                    }
                }
                Target::Split { .. } => {}
            }
        }
    }
    panic!("no file with {what:?}");
}

/// Resident data whose size is near 2^64 (inline $DATA, a named stream):
/// opening the file fails (1.2.1 panicked adding the size to the data's
/// offset).
#[test]
fn resident_data_of_impossible_sizes_is_an_error() {
    for (name, what) in [("r314small", Resident::Data), ("r314basic4k", Resident::Stream)] {
        for size in [u64::MAX, u64::MAX - 0x3b] {
            let mut f = Forge::fixture(name);
            let (dir, file, record) = file_with(&f.volume().unwrap(), what);
            let at = |page: &[u8]| page.windows(record.len()).position(|w| w == record.as_slice());
            f.patch(
                Tree::Object(dir),
                |page| at(page).is_some(),
                |page| {
                    let pos = at(page).unwrap();
                    let rec = &page[pos..pos + record.len()];
                    let node = Node::at(rec, 0).unwrap();
                    let value = node.rows().flatten().find(|r| what.is(r.key, r.value)).unwrap().value;
                    let size_at = pos + place(rec, value) + 0x20;
                    page[size_at..size_at + 8].copy_from_slice(&size.to_le_bytes());
                },
            );
            let vol = f.volume().unwrap();
            let entry = vol.read_dir(dir).unwrap().into_iter().find(|e| e.name == file).unwrap();
            assert!(vol.open_file(&entry).is_err(), "{name}, {what:?} of {size:#x} bytes");
        }
    }
}

/// A device that counts what is read from it.
struct Counted<'a>(&'a SparseImage, std::sync::atomic::AtomicU64);

impl ReadAt for Counted<'_> {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        self.1.fetch_add(buf.len() as u64, std::sync::atomic::Ordering::Relaxed);
        self.0.read_exact_at(buf, offset)
    }

    fn size(&self) -> std::io::Result<u64> {
        self.0.size()
    }
}

/// An index node whose rows all name its first child (with its right
/// checksum): reading the directory fails instead of reading that page
/// once per row (and, a level further down, per row of each), and the
/// check names the page (1.2.1 listed the child's names once per row).
#[test]
fn an_index_naming_a_page_twice_is_an_error() {
    let mut f = Forge::fixture("r314basic64k");
    let rows = std::cell::Cell::new(0);
    f.patch(
        Tree::Object(0x70f),
        |_| true,
        |page| {
            let node = Node::at(page, 0x50).unwrap();
            assert!(!node.is_leaf(), "the directory's root is an index");
            let values: Vec<(usize, usize)> = node
                .rows()
                .flatten()
                .map(|r| (place(page, r.value), r.value.len()))
                .collect();
            rows.set(values.len());
            let (first, len) = values[0];
            let reference = page[first..first + len].to_vec();
            for &(at, n) in &values[1..] {
                assert_eq!(n, len);
                page[at..at + n].copy_from_slice(&reference);
            }
        },
    );
    assert!(rows.get() > 1);
    let counted = Counted(&f.image, Default::default());
    let vol = Volume::open(&counted, f.offset).unwrap();
    let opened = counted.1.swap(0, std::sync::atomic::Ordering::Relaxed);
    let e = vol.read_dir(0x70f).unwrap_err();
    assert!(e.to_string().contains("named twice"), "{e}");
    let read = counted.1.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        read <= 2 * 65536,
        "{read} bytes read for one directory ({opened} to open)"
    );
    let report = vol.check(&[]).unwrap();
    assert!(
        report.problems.iter().any(|p| p.contains("named twice")),
        "{:?}",
        report.problems
    );
}

/// A row of a table whose range of clusters passes 2^64: an
/// allocator's ends the check with an error, a reference count table's is
/// a problem (1.2.1 panicked adding the start and the count).
#[test]
fn rows_counting_past_2_64_end_the_check() {
    // A row not at cluster 0, whose count then passes 2^64.
    let leaf = |page: &[u8]| {
        Node::at(page, 0x50).is_ok_and(|n| n.is_leaf() && n.rows().flatten().any(|r| le64(r.value, 0) > 0))
    };
    let first_row = |page: &mut Vec<u8>| {
        let node = Node::at(page, 0x50).unwrap();
        let value = node.rows().flatten().find(|r| le64(r.value, 0) > 0).unwrap().value;
        let at = place(page, value);
        page[at + 8..at + 16].copy_from_slice(&u64::MAX.to_le_bytes());
    };
    // The medium allocator.
    let mut f = Forge::fixture("r314small");
    f.patch(Tree::Root(1), leaf, first_row);
    let e = f.volume().unwrap().check(&[]).unwrap_err();
    assert!(e.to_string().contains("allocator 1"), "{e}");
    // The block reference count table of a volume with block clones.
    let mut f = Forge::fixture("r314feat");
    f.patch(Tree::Root(6), leaf, first_row);
    let report = f.volume().unwrap().check(&[]).unwrap();
    assert!(
        report.problems.iter().any(|p| p.contains("reference count row")),
        "{:?}",
        report.problems
    );
}
