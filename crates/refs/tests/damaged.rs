//! Volumes of every ReFS version with bytes changed read to the end or fail
//! with an error: whatever a damaged or hostile disk holds, no panic.
//! (Test builds check every page's checksum, so most changes stop at the
//! page they hit; the fuzz targets, which accept every checksum, go deeper.
//! The parsers behind the checksums get random bytes below.)

use std::fs::File;
use std::path::Path;

use refs::node::Node;
use refs::page::{PageHeader, PageRef};
use refs::volume::ROOT_DIRECTORY;
use refs::{Content, Target, Volume};
use storage_spaces::io::SparseImage;

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
