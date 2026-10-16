//! Changes to a fixture volume as a hostile disk can make them: a page
//! changed in memory, and every reference on the way to it from the
//! checkpoint stored again with the new checksum, the checkpoint sealed
//! again. So the changes reach the parsers in a normal build (the fuzz
//! targets accept every checksum instead).

use std::fs::File;
use std::path::Path;

use refs::Volume;
use refs::checksum::{crc32c, crc64, sha256};
use refs::node::Node;
use refs::page::{PageRef, store_reference};
use storage_spaces::io::{ReadAt, SparseImage};

#[derive(Clone)]
pub struct Forge {
    pub image: SparseImage,
    pub offset: u64,
}

/// The tree a change is in: a root of the checkpoint, or an object of the
/// object table.
#[derive(Clone, Copy)]
pub enum Tree {
    Root(usize),
    Object(u64),
}

/// A reference on the way to a page: where it is in the page above (or the
/// checkpoint; at byte `at`, `len` bytes) and the page it names.
struct Link {
    at: usize,
    len: usize,
    child: PageRef,
    physical: bool,
}

pub fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}

pub fn le32(b: &[u8], at: usize) -> usize {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize
}

pub fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// Where `part`, a slice of `whole`, starts in it.
pub fn place(whole: &[u8], part: &[u8]) -> usize {
    part.as_ptr() as usize - whole.as_ptr() as usize
}

/// Where the checkpoint keeps its roots' references.
fn root_offsets(chkp: &[u8]) -> Vec<usize> {
    let array = if le32(chkp, 0x78) & 0x200 != 0 {
        le32(chkp, 0x94)
    } else {
        0x94
    };
    (0..le32(chkp, 0x90)).map(|i| le32(chkp, array + 4 * i)).collect()
}

/// Seals a checkpoint's reference to itself (at 0x58, its length at 0x5c):
/// the checksum of its first `unit` bytes with the reference zeroed.
fn seal(page: &mut [u8], unit: usize) {
    let (at, len) = (le32(page, 0x58), le32(page, 0x5c));
    let (kind, sum_at, sum_len) = (
        page[at + 0x22],
        at + 0x20 + page[at + 0x23] as usize,
        le32(page, at + 0x24),
    );
    let mut copy = page[..unit].to_vec();
    copy[at..at + len].fill(0);
    let sum = match kind {
        1 => crc32c(&copy).to_le_bytes().to_vec(),
        2 => crc64(&copy).to_le_bytes().to_vec(),
        4 => sha256(&copy).to_vec(),
        other => panic!("checksum kind {other}"),
    };
    page[sum_at..sum_at + sum_len].copy_from_slice(&sum[..sum_len]);
}

impl Forge {
    pub fn fixture(name: &str) -> Self {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
        let image = SparseImage::read_from(File::open(dir.join("disk.fixture")).unwrap()).unwrap();
        let text = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap();
        let offset = manifest["partition_offset"].as_u64().unwrap();
        Forge { image, offset }
    }

    pub fn volume(&self) -> refs::Result<Volume<&SparseImage>> {
        Volume::open(&self.image, self.offset)
    }

    fn read(&self, clusters: &[u64], cluster: u64) -> Vec<u8> {
        let mut page = vec![0u8; clusters.len() * cluster as usize];
        for (part, &c) in page.chunks_mut(cluster as usize).zip(clusters) {
            self.image.read_exact_at(part, self.offset + c * cluster).unwrap();
        }
        page
    }

    fn write(&mut self, clusters: &[u64], cluster: u64, page: &[u8]) {
        for (part, &c) in page.chunks(cluster as usize).zip(clusters) {
            self.image.insert(self.offset + c * cluster, part);
        }
    }

    /// Changes, with `change`, the first page of `tree` (depth first, in
    /// key order) that `pick` chooses, and stores the references on the
    /// way to it again.
    pub fn patch(&mut self, tree: Tree, pick: impl Fn(&[u8]) -> bool, change: impl FnOnce(&mut Vec<u8>)) {
        let vol = self.volume().unwrap();
        let (cluster, size) = (vol.cluster, vol.checkpoint.reference_size);
        let per = (vol.page_size / cluster) as usize;
        let clusters = |r: &PageRef, physical: bool| -> Vec<u64> {
            r.lcns[..per]
                .iter()
                .map(|&l| if physical { l } else { vol.translate(l).unwrap() })
                .collect()
        };
        let chkp_lcn = vol.checkpoint.lcn;
        let mut chkp = self.read(&(chkp_lcn..chkp_lcn + per as u64).collect::<Vec<_>>(), cluster);
        let roots = root_offsets(&chkp);
        let root = match tree {
            Tree::Root(i) => i,
            Tree::Object(_) => refs::volume::ROOT_OBJECTS,
        };
        let top = Link {
            at: roots[root],
            len: size,
            child: vol.checkpoint.roots[root].clone(),
            physical: matches!(root, 7 | 8 | 12),
        };
        let chain = match tree {
            Tree::Root(_) => self.descend(&vol, top, &pick, &clusters),
            Tree::Object(oid) => {
                // The object table's leaf with the object's row, then the
                // object's root (its reference at 0x20 of the row's value).
                let has = |page: &[u8]| {
                    Node::at(page, vol.node_offset()).is_ok_and(|n| {
                        n.is_leaf() && n.rows().flatten().any(|r| r.key.len() >= 16 && le64(r.key, 8) == oid)
                    })
                };
                let mut chain = self.descend(&vol, top, &has, &clusters).expect("the object's row");
                let leaf = chain.last().unwrap();
                let at = clusters(&leaf.child, leaf.physical);
                let page = self.read(&at, cluster);
                let node = Node::at(&page, vol.node_offset()).unwrap();
                let value = node
                    .rows()
                    .flatten()
                    .find(|r| r.key.len() >= 16 && le64(r.key, 8) == oid)
                    .unwrap()
                    .value;
                let below = Link {
                    at: place(&page, value) + 0x20,
                    len: size,
                    child: PageRef::parse(&value[0x20..0x20 + size]).unwrap(),
                    physical: false,
                };
                self.descend(&vol, below, &pick, &clusters).map(|rest| {
                    chain.extend(rest);
                    chain
                })
            }
        }
        .expect("a page to change");
        let places: Vec<Vec<u64>> = chain.iter().map(|l| clusters(&l.child, l.physical)).collect();
        let mut pages: Vec<Vec<u8>> = places.iter().map(|at| self.read(at, cluster)).collect();
        drop(vol);
        change(pages.last_mut().unwrap());
        // Bottom up: each page written, its reference stored in its holder.
        for (k, link) in chain.iter().enumerate().rev() {
            let page = pages[k].clone();
            self.write(&places[k], cluster, &page);
            let holder = if k == 0 { &mut chkp } else { &mut pages[k - 1] };
            store_reference(&mut holder[link.at..link.at + link.len], &link.child.lcns[..per], &page).unwrap();
        }
        seal(&mut chkp, cluster as usize);
        self.write(&(chkp_lcn..chkp_lcn + per as u64).collect::<Vec<_>>(), cluster, &chkp);
    }

    /// The references from `link` down to the first page `pick` chooses.
    fn descend(
        &self,
        vol: &Volume<&SparseImage>,
        link: Link,
        pick: &dyn Fn(&[u8]) -> bool,
        clusters: &dyn Fn(&PageRef, bool) -> Vec<u64>,
    ) -> Option<Vec<Link>> {
        let at = clusters(&link.child, link.physical);
        let page = self.read(&at, vol.cluster);
        if pick(&page) {
            return Some(vec![link]);
        }
        let node = Node::at(&page, vol.node_offset()).ok()?;
        if node.is_leaf() {
            return None;
        }
        for row in node.rows() {
            let value = row.ok()?.value;
            let next = Link {
                at: place(&page, value),
                len: value.len(),
                child: vol.page_ref(value).ok()?,
                physical: link.physical,
            };
            if let Some(mut below) = self.descend(vol, next, pick, clusters) {
                below.insert(0, link);
                return Some(below);
            }
        }
        None
    }

    /// Sets the physical start of container `id` (the u64 16 bytes before
    /// the end of its row in the container table).
    pub fn set_container_start(&mut self, id: u64, start: u64) {
        let row_of = move |page: &[u8]| {
            let node = Node::at(page, 0x50).ok()?;
            if !node.is_leaf() {
                return None;
            }
            node.rows()
                .flatten()
                .find(|r| r.key.len() >= 8 && le64(r.key, 0) == id)
                .map(|r| place(page, r.value) + r.value.len() - 16)
        };
        self.patch(
            Tree::Root(refs::volume::ROOT_CONTAINERS),
            |page| row_of(page).is_some(),
            |page| {
                let at = row_of(page).unwrap();
                page[at..at + 8].copy_from_slice(&start.to_le_bytes());
            },
        );
    }
}
