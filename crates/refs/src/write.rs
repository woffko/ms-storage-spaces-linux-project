//! Writing (Track B4): copy-on-write transactions.
//!
//! A transaction edits copies of the B+-tree pages of the current
//! checkpoint. Committing gives every changed page, and every page above
//! it, new clusters from the allocators and frees its old ones (which
//! changes allocator pages, so this repeats until every changed page has
//! new clusters), fills in the references child first with their
//! checksums, writes the pages to their new clusters, flushes, and then
//! writes a new checkpoint into the older of the two checkpoint slots and
//! flushes again. Nothing the current checkpoint references is written
//! before that last cluster, so a crash at any point leaves either the
//! old volume or the new one (docs/refs-format.md, "Writing").
//!
//! Which allocator serves a table (as Windows does it): the container
//! allocator (root 2) the allocator tables themselves, the block reference
//! counts and the integrity state (roots 1, 2, 6, 11); the medium
//! allocator (root 1) everything else. Both are bitmaps of physical
//! clusters. No log records are written: the commit keeps the log's
//! sequence number, which Windows takes as nothing to replay.

use std::collections::HashMap;

use storage_spaces::io::WriteAt;

use crate::checksum::crc32c;
use crate::error::{Error, Result, format_err};
use crate::file::{LIVE_STREAM, Target, Times};
use crate::node::Node;
use crate::page::{PAGE_HEADER_SIZE, PageRef, store_reference};
use crate::util::{le16, le32, le64, utf16};
use crate::volume::{ROOT_DIRECTORY, ROOT_OBJECTS, Volume};

const ROOT_MEDIUM_ALLOCATOR: usize = 1;
const ROOT_CONTAINER_ALLOCATOR: usize = 2;
const ROOT_OBJECTS_COPY: usize = 5;
/// Allocator rows with a bitmap (u16 at 0x12; 2 marks a range without
/// one, wholly used or wholly free).
const ALLOCATOR_BITMAP: u16 = 1;
const ALLOCATOR_HEADER: usize = 0x18;

/// A table: a checkpoint root's tree or an object's tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Tree {
    Root(usize),
    Object(u64),
}

impl Tree {
    /// The allocator whose clusters this table's pages take.
    fn allocator(self) -> Result<usize> {
        match self {
            Tree::Root(1 | 2 | 6 | 11) => Ok(ROOT_CONTAINER_ALLOCATOR),
            Tree::Root(7 | 8 | 12) => Err(Error::Unsupported(format!(
                "writing table {self:?} (physical clusters)"
            ))),
            _ => Ok(ROOT_MEDIUM_ALLOCATOR),
        }
    }

    fn physical(self) -> bool {
        matches!(self, Tree::Root(7 | 8 | 12))
    }
}

/// A copy of a page in a transaction.
struct Page {
    tree: Tree,
    depth: usize,
    /// The physical clusters it was read from.
    old: Vec<u64>,
    data: Vec<u8>,
    /// The parent page and the key of the row there that references this
    /// page (empty for the last row): rows move when pages change, keys
    /// stay.
    parent: Option<(usize, Vec<u8>)>,
    dirty: bool,
    /// The physical clusters it is written to.
    new: Option<Vec<u64>>,
}

/// A leaf row of a page in a transaction: the page and where the value is.
#[derive(Debug, Clone, Copy)]
struct RowAt {
    page: usize,
    value: usize,
    len: usize,
}

pub struct Transaction<'v, D> {
    vol: &'v Volume<D>,
    pages: Vec<Page>,
    /// Pages already copied, by table and first physical cluster.
    loaded: HashMap<(Tree, u64), usize>,
    roots: HashMap<Tree, usize>,
    /// Clusters freed by this transaction: the old checkpoint still uses
    /// them, so they are not taken again before the commit.
    freed: std::collections::HashSet<u64>,
    /// Set while committing: allocator rows are not restructured then.
    committing: bool,
}

impl<'v, D: WriteAt> Transaction<'v, D> {
    /// A transaction on the volume; refused while the log holds records
    /// Windows would replay over the checkpoint.
    pub fn begin(vol: &'v Volume<D>) -> Result<Self> {
        if vol.log_state()?.needs_replay() {
            return Err(Error::Unsupported(
                "the volume's log has changes its checkpoint lacks (Windows replays them when it next \
                 attaches the volume): attach it to Windows once and detach it before writing"
                    .into(),
            ));
        }
        Ok(Self::new(vol))
    }

    fn new(vol: &'v Volume<D>) -> Self {
        Transaction {
            vol,
            pages: Vec::new(),
            loaded: HashMap::new(),
            roots: HashMap::new(),
            freed: Default::default(),
            committing: false,
        }
    }

    fn per_page(&self) -> usize {
        (self.vol.page_size / self.vol.cluster) as usize
    }

    /// Copies the page a reference names (once per transaction).
    fn load(&mut self, tree: Tree, r: &PageRef, depth: usize, parent: Option<(usize, Vec<u8>)>) -> Result<usize> {
        let old = r.lcns[..self.per_page()]
            .iter()
            .map(|&l| if tree.physical() { Ok(l) } else { self.vol.translate(l) })
            .collect::<Result<Vec<_>>>()?;
        if let Some(&i) = self.loaded.get(&(tree, old[0])) {
            return Ok(i);
        }
        if depth > 16 {
            return Err(format_err!("table {tree:?} deeper than 16 levels"));
        }
        let data = self.vol.read_page(r, tree.physical())?;
        self.pages.push(Page {
            tree,
            depth,
            old: old.clone(),
            data,
            parent,
            dirty: false,
            new: None,
        });
        let i = self.pages.len() - 1;
        self.loaded.insert((tree, old[0]), i);
        Ok(i)
    }

    fn root(&mut self, tree: Tree) -> Result<usize> {
        if let Some(&i) = self.roots.get(&tree) {
            return Ok(i);
        }
        let vol = self.vol;
        let r = match tree {
            Tree::Root(i) => vol
                .checkpoint
                .roots
                .get(i)
                .ok_or_else(|| format_err!("no checkpoint root {i}"))?,
            Tree::Object(oid) => vol.object(oid)?,
        };
        let i = self.load(tree, r, 0, None)?;
        self.roots.insert(tree, i);
        Ok(i)
    }

    /// Every leaf row of a table whose key `wanted` accepts, copying the
    /// pages on the way.
    fn rows(&mut self, tree: Tree, wanted: &dyn Fn(&[u8]) -> bool) -> Result<Vec<RowAt>> {
        let root = self.root(tree)?;
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(i) = stack.pop() {
            let data = &self.pages[i].data;
            let base = data.as_ptr() as usize;
            let node = Node::at(data, PAGE_HEADER_SIZE)?;
            let leaf = node.is_leaf();
            let mut children = Vec::new();
            for row in node.rows() {
                let row = row?;
                let value = row.value.as_ptr() as usize - base;
                if leaf {
                    if wanted(row.key) {
                        out.push(RowAt {
                            page: i,
                            value,
                            len: row.value.len(),
                        });
                    }
                } else {
                    children.push((row.key.to_vec(), row.value.to_vec()));
                }
            }
            for (key, r) in children.into_iter().rev() {
                stack.push(self.child(tree, i, key, &r)?);
            }
        }
        Ok(out)
    }

    fn find(&mut self, tree: Tree, wanted: &dyn Fn(&[u8]) -> bool) -> Result<RowAt> {
        self.rows(tree, wanted)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::NotFound(format!("row in table {tree:?}")))
    }

    /// A row's value to change in place (its page and those above it are
    /// copied on commit).
    fn value_mut(&mut self, at: RowAt) -> &mut [u8] {
        let mut i = Some(at.page);
        while let Some(j) = i {
            self.pages[j].dirty = true;
            i = self.pages[j].parent.as_ref().map(|p| p.0);
        }
        &mut self.pages[at.page].data[at.value..at.value + at.len]
    }

    /// The bitmap rows of an allocator: start, clusters, where.
    fn bitmaps(&mut self, allocator: usize) -> Result<Vec<(u64, u64, RowAt)>> {
        let rows = self.rows(Tree::Root(allocator), &|_| true)?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let v = &self.pages[r.page].data[r.value..r.value + r.len];
                let (start, count) = (le64(v, 0), le64(v, 8));
                (le16(v, 0x12) == ALLOCATOR_BITMAP
                    && count % 8 == 0
                    && r.len as u64 == ALLOCATOR_HEADER as u64 + count / 8)
                    .then_some((start, count, r))
            })
            .collect())
    }

    /// `count` clusters for file data from the medium allocator's bitmap
    /// row that starts at `band` (the row where data near it is), in as few
    /// runs as fit: (first physical cluster, clusters) each, marked used.
    fn take_data(&mut self, band: u64, count: u64) -> Result<Vec<(u64, u64)>> {
        let (start, len, at) = self
            .bitmaps(ROOT_MEDIUM_ALLOCATOR)?
            .into_iter()
            .find(|(start, _, _)| *start == band)
            .ok_or_else(|| format_err!("no allocator bitmap at {band:#x}"))?;
        let v = &self.pages[at.page].data[at.value..at.value + at.len];
        if (le16(v, 0x10) as u64) < count {
            return Err(Error::Unsupported(
                "no room for the data in its band (other bands are not used yet)".into(),
            ));
        }
        let used =
            |j: u64| v[ALLOCATOR_HEADER + (j / 8) as usize] >> (j % 8) & 1 != 0 || self.freed.contains(&(start + j));
        // As Windows places data: after the data already in the row (the
        // free clusters below it only when nothing is left above), in runs
        // that end at the file's clusters 1, 64, 256 and multiples of 256.
        let after = (0..len).rev().find(|&j| used(j)).map_or(0, |j| j + 1);
        let mut runs: Vec<(u64, u64)> = Vec::new();
        let mut left = count;
        let mut vcn = 0;
        for (from, to) in [(after, len), (0, after)] {
            let mut j = from;
            while left > 0 && j < to {
                if used(j) {
                    j += 1;
                    continue;
                }
                let boundary = match vcn {
                    0 => 1,
                    1..64 => 64,
                    _ => (vcn / 256 + 1) * 256,
                };
                let mut n = 0;
                while j + n < to && n < left && vcn + n < boundary && !used(j + n) {
                    n += 1;
                }
                runs.push((j, n));
                left -= n;
                vcn += n;
                j += n;
            }
        }
        if left > 0 {
            return Err(Error::Unsupported("no room for the data in its band".into()));
        }
        let v = self.value_mut(at);
        for &(j, n) in &runs {
            for k in j..j + n {
                v[ALLOCATOR_HEADER + (k / 8) as usize] |= 1 << (k % 8);
            }
        }
        let free = le16(v, 0x10) - count as u16;
        v[0x10..0x12].copy_from_slice(&free.to_le_bytes());
        Ok(runs.into_iter().map(|(j, n)| (start + j, n)).collect())
    }

    /// `n` free clusters of an allocator, aligned to `n`, marked used.
    fn take(&mut self, allocator: usize, n: usize) -> Result<Vec<u64>> {
        for (start, count, at) in self.bitmaps(allocator)? {
            let v = &self.pages[at.page].data[at.value..at.value + at.len];
            if (le16(v, 0x10) as usize) < n {
                continue;
            }
            let freed = &self.freed;
            let bit =
                |j: u64| v[ALLOCATOR_HEADER + (j / 8) as usize] >> (j % 8) & 1 != 0 || freed.contains(&(start + j));
            let Some(j) = (0..count.saturating_sub(n as u64 - 1))
                .step_by(n)
                .find(|&j| (j..j + n as u64).all(|k| !bit(k)))
            else {
                continue;
            };
            let v = self.value_mut(at);
            for k in j..j + n as u64 {
                v[ALLOCATOR_HEADER + (k / 8) as usize] |= 1 << (k % 8);
            }
            let free = le16(v, 0x10) - n as u16;
            v[0x10..0x12].copy_from_slice(&free.to_le_bytes());
            // The next-allocation hint stays above what is used.
            if le16(v, 0x16) < (j + n as u64) as u16 {
                v[0x16..0x18].copy_from_slice(&((j + n as u64) as u16).to_le_bytes());
            }
            return Ok((start + j..start + j + n as u64).collect());
        }
        Err(format_err!("allocator {allocator} has no {n} free clusters"))
    }

    /// Marks clusters free again.
    fn release(&mut self, allocator: usize, clusters: &[u64]) -> Result<()> {
        let mut bitmaps = self.bitmaps(allocator)?;
        for &c in clusters {
            if !self.committing && !bitmaps.iter().any(|(start, count, _)| *start <= c && c < start + count) {
                self.unpack(allocator, c)?;
                bitmaps = self.bitmaps(allocator)?;
            }
            let &(start, _, at) = bitmaps
                .iter()
                .find(|(start, count, _)| *start <= c && c < start + count)
                .ok_or_else(|| format_err!("cluster {c:#x} in no bitmap of allocator {allocator}"))?;
            let j = c - start;
            let v = self.value_mut(at);
            let byte = &mut v[ALLOCATOR_HEADER + (j / 8) as usize];
            if *byte >> (j % 8) & 1 == 0 {
                return Err(format_err!("cluster {c:#x} is not allocated"));
            }
            *byte &= !(1 << (j % 8));
            let free = le16(v, 0x10) + 1;
            v[0x10..0x12].copy_from_slice(&free.to_le_bytes());
            self.freed.insert(c);
        }
        Ok(())
    }

    /// Turns the block of a fully used uniform allocator row (kind 2, no
    /// free clusters) that holds cluster `c` into a bitmap row of its
    /// 0x4000 clusters, all used, so that clusters there can be freed: the
    /// row becomes up to three (uniform before, the bitmap, uniform after).
    fn unpack(&mut self, allocator: usize, c: u64) -> Result<()> {
        const BLOCK: u64 = 0x4000;
        let tree = Tree::Root(allocator);
        let at = self.find(tree, &|k| {
            k.len() >= 16 && le64(k, 0) <= c && c < le64(k, 0).saturating_add(le64(k, 8))
        })?;
        let v = self.pages[at.page].data[at.value..at.value + at.len].to_vec();
        let (start, count) = (le64(&v, 0), le64(&v, 8));
        if v.len() != 0x18 || le16(&v, 0x12) != 2 || le16(&v, 0x10) != 0 || start % BLOCK != 0 || count % BLOCK != 0 {
            return Err(format_err!(
                "cluster {c:#x} in an allocator row of kind {} with {} free (not a used uniform row)",
                le16(&v, 0x12),
                le16(&v, 0x10)
            ));
        }
        let block = start + (c - start) / BLOCK * BLOCK;
        let key = |s: u64, n: u64| [s.to_le_bytes(), n.to_le_bytes()].concat();
        let uniform = |s: u64, n: u64| {
            let mut u = v.clone();
            u[0..8].copy_from_slice(&s.to_le_bytes());
            u[8..16].copy_from_slice(&n.to_le_bytes());
            row(&key(s, n), &u, 0)
        };
        let mut bitmap = vec![0xffu8; ALLOCATOR_HEADER + (BLOCK / 8) as usize];
        bitmap[..0x18].fill(0);
        bitmap[0..8].copy_from_slice(&block.to_le_bytes());
        bitmap[8..16].copy_from_slice(&BLOCK.to_le_bytes());
        bitmap[0x12..0x14].copy_from_slice(&ALLOCATOR_BITMAP.to_le_bytes());
        bitmap[0x14..0x16].copy_from_slice(&0x218u16.to_le_bytes());
        let mut rows = vec![row(&key(block, BLOCK), &bitmap, 0)];
        if block > start {
            rows.push(uniform(start, block - start));
        }
        if block + BLOCK < start + count {
            rows.push(uniform(block + BLOCK, start + count - block - BLOCK));
        }
        let old = key(start, count);
        let page = at.page;
        self.remove_quiet(page, &|k| k == old)?;
        for r in rows {
            self.insert_sorted(tree, &r, &u64_key_order)?;
        }
        Ok(())
    }

    /// A page the transaction adds: the root of a new table (it gets
    /// clusters on commit; there are no old ones to free).
    fn new_root(&mut self, tree: Tree, data: Vec<u8>) -> usize {
        self.pages.push(Page {
            tree,
            depth: 0,
            old: Vec::new(),
            data,
            parent: None,
            dirty: true,
            new: None,
        });
        let i = self.pages.len() - 1;
        self.roots.insert(tree, i);
        i
    }

    /// A directory's last file id given out (its object table row, 0x50).
    fn last_file_id(&mut self, dir: u64) -> Result<u64> {
        let at = self.find(Tree::Root(ROOT_OBJECTS), &|k| k.len() >= 16 && le64(k, 8) == dir)?;
        let v = &self.pages[at.page].data[at.value..at.value + at.len];
        if v.len() < 0x58 {
            return Err(format_err!("object table row of {} bytes", v.len()));
        }
        Ok(le64(v, 0x50))
    }

    /// Sets a directory's last file id in both object tables.
    fn set_last_file_id(&mut self, dir: u64, id: u64) -> Result<()> {
        for table in [ROOT_OBJECTS, ROOT_OBJECTS_COPY] {
            let at = self.find(Tree::Root(table), &|k| k.len() >= 16 && le64(k, 8) == dir)?;
            let v = self.value_mut(at);
            if v.len() < 0x58 {
                return Err(format_err!("object table row of {} bytes", v.len()));
            }
            v[0x50..0x58].copy_from_slice(&id.to_le_bytes());
        }
        Ok(())
    }

    /// Puts a row into a node (leaf or index) in the free space between
    /// its rows and its key index (compacting it first when the space is
    /// in holes), and counts it in the table's descriptor when `count`
    /// (leaf rows). False: no room in the page.
    fn place(&mut self, page: usize, row: &[u8], before: &dyn Fn(&[u8]) -> bool, count: bool) -> Result<bool> {
        let d = &self.pages[page].data;
        let h = PAGE_HEADER_SIZE + le32(d, PAGE_HEADER_SIZE) as usize;
        let (end, free, index, rows) = (
            le32(d, h + 4) as usize,
            le32(d, h + 8) as usize,
            le32(d, h + 0x10) as usize,
            le32(d, h + 0x14) as usize,
        );
        let size = row.len().next_multiple_of(8);
        if free < size + 4 {
            return Ok(false);
        }
        if end + size + 4 > index {
            self.compact(page)?;
            return self.place(page, row, before, count);
        }
        // Where in the key index the row goes.
        let node = Node::at(d, PAGE_HEADER_SIZE)?;
        let mut pos = rows;
        for (i, r) in node.rows().enumerate() {
            if before(r?.key) {
                pos = i;
                break;
            }
        }
        self.mark(page);
        let d = &mut self.pages[page].data;
        d[h + end..h + end + row.len()].copy_from_slice(row);
        d[h + end + row.len()..h + end + size].fill(0);
        d.copy_within(h + index..h + index + 4 * pos, h + index - 4);
        let entry = 0xffff_0000u32 | end as u32;
        d[h + index - 4 + 4 * pos..h + index + 4 * pos].copy_from_slice(&entry.to_le_bytes());
        let put = |d: &mut Vec<u8>, at: usize, v: u32| d[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put(d, h + 4, (end + size) as u32);
        put(d, h + 8, (free - size - 4) as u32);
        put(d, h + 0x10, (index - 4) as u32);
        put(d, h + 0x14, (rows + 1) as u32);
        if count {
            self.count_rows(page, 1);
        }
        Ok(true)
    }

    /// Adds to the table's row count (its root page's descriptor, 0x20).
    fn count_rows(&mut self, page: usize, by: i64) {
        let root = self.roots[&self.pages[page].tree];
        self.mark(root);
        let d = &mut self.pages[root].data;
        let rows = (le64(d, PAGE_HEADER_SIZE + 0x20) as i64 + by) as u64;
        d[PAGE_HEADER_SIZE + 0x20..PAGE_HEADER_SIZE + 0x28].copy_from_slice(&rows.to_le_bytes());
    }

    /// Adds to the table's count of pages below its root (0x18).
    fn count_pages(&mut self, tree: Tree, by: i64) {
        let root = self.roots[&tree];
        self.mark(root);
        let d = &mut self.pages[root].data;
        let pages = le64(d, PAGE_HEADER_SIZE + 0x18).wrapping_add_signed(by);
        d[PAGE_HEADER_SIZE + 0x18..PAGE_HEADER_SIZE + 0x20].copy_from_slice(&pages.to_le_bytes());
    }

    /// Inserts a leaf row in key order (`order` compares keys; an empty
    /// key, the last row of an index node, is above every key) into a table
    /// of any depth, splitting pages that are full.
    fn insert_sorted(
        &mut self,
        tree: Tree,
        row: &[u8],
        order: &dyn Fn(&[u8], &[u8]) -> std::cmp::Ordering,
    ) -> Result<()> {
        let key = row_key(row).to_vec();
        for _ in 0..16 {
            let leaf = self.find_leaf(tree, &key, order)?;
            if self.place(leaf, row, &|k| order(&key, k).is_lt(), true)? {
                return Ok(());
            }
            self.split(tree, leaf, order)?;
        }
        Err(format_err!("no room for a row in table {tree:?}"))
    }

    /// The leaf whose key range holds `key`.
    fn find_leaf(
        &mut self,
        tree: Tree,
        key: &[u8],
        order: &dyn Fn(&[u8], &[u8]) -> std::cmp::Ordering,
    ) -> Result<usize> {
        let mut page = self.root(tree)?;
        for _ in 0..16 {
            let node = Node::at(&self.pages[page].data, PAGE_HEADER_SIZE)?;
            if node.is_leaf() {
                return Ok(page);
            }
            let mut pick = None;
            for row in node.rows() {
                let row = row?;
                pick = Some((row.key.to_vec(), row.value.to_vec()));
                if row.key.is_empty() || order(key, row.key).is_le() {
                    break;
                }
            }
            let (k, v) = pick.ok_or_else(|| format_err!("an index node without rows"))?;
            page = self.child(tree, page, k, &v)?;
        }
        Err(format_err!("table {tree:?} deeper than 16 levels"))
    }

    /// The child page an index row names: one this transaction made or
    /// already copied, else read.
    fn child(&mut self, tree: Tree, parent: usize, key: Vec<u8>, reference: &[u8]) -> Result<usize> {
        if let Some(i) = self
            .pages
            .iter()
            .position(|p| p.parent.as_ref() == Some(&(parent, key.clone())))
        {
            return Ok(i);
        }
        let depth = self.pages[parent].depth + 1;
        self.load(tree, &PageRef::parse(reference)?, depth, Some((parent, key)))
    }

    /// The rows of a node in key order: (key, the whole row).
    fn node_rows(&self, page: usize) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let d = &self.pages[page].data;
        let h = PAGE_HEADER_SIZE + le32(d, PAGE_HEADER_SIZE) as usize;
        let (index, count) = (le32(d, h + 0x10) as usize, le32(d, h + 0x14) as usize);
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let at = h + (le32(d, h + index + 4 * i) & 0xffff) as usize;
            let size = le32(d, at) as usize;
            let row = d
                .get(at..at + size)
                .ok_or_else(|| format_err!("row outside its page"))?;
            out.push((row_key(row).to_vec(), row.to_vec()));
        }
        Ok(out)
    }

    /// Lays out a node anew: `rows` from the start of its row area, the key
    /// index at the end of the page. In an index node the last row, and only
    /// it, has row flag 2 (also when it keeps a key: Windows takes a node
    /// whose last row lacks the flag for a damaged page).
    fn write_node(&mut self, page: usize, level: u8, flags: u8, rows: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        self.mark(page);
        let d = &mut self.pages[page].data;
        let h = PAGE_HEADER_SIZE + le32(d, PAGE_HEADER_SIZE) as usize;
        let area = d.len() - h;
        let used: usize = rows.iter().map(|(_, r)| r.len()).sum();
        let index = area - 4 * rows.len();
        if 0x28 + used > index {
            return Err(format_err!("{} bytes of rows for a node of {area}", used));
        }
        d[h..].fill(0);
        let put = |d: &mut Vec<u8>, at: usize, v: u32| d[at..at + 4].copy_from_slice(&v.to_le_bytes());
        let mut at = 0x28;
        for (i, (_, r)) in rows.iter().enumerate() {
            d[h + at..h + at + r.len()].copy_from_slice(r);
            if level > 0 {
                let last = if i + 1 == rows.len() { ROW_LAST } else { 0 };
                let f = (le16(r, 8) & !ROW_LAST) | last;
                d[h + at + 8..h + at + 10].copy_from_slice(&f.to_le_bytes());
            }
            put(d, h + index + 4 * i, 0xffff_0000 | at as u32);
            at += r.len();
        }
        put(d, h, 0x28);
        put(d, h + 4, at as u32);
        put(d, h + 8, (index - at) as u32);
        d[h + 0x0c] = level;
        d[h + 0x0d] = flags;
        put(d, h + 0x10, index as u32);
        put(d, h + 0x14, rows.len() as u32);
        put(d, h + 0x20, area as u32);
        Ok(())
    }

    /// A new page below `parent` (the table's root lends its header; no
    /// table descriptor: the node header follows at 0x58).
    fn new_child(&mut self, tree: Tree, parent: usize, key: Vec<u8>) -> usize {
        let root = self.roots[&tree];
        let mut data = vec![0u8; self.pages[root].data.len()];
        data[..PAGE_HEADER_SIZE].copy_from_slice(&self.pages[root].data[..PAGE_HEADER_SIZE]);
        data[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + 4].copy_from_slice(&8u32.to_le_bytes());
        let depth = self.pages[parent].depth + 1;
        self.pages.push(Page {
            tree,
            depth,
            old: Vec::new(),
            data,
            parent: Some((parent, key)),
            dirty: true,
            new: None,
        });
        self.pages.len() - 1
    }

    /// An index row for a child page (its reference, laid out like the
    /// checkpoint's, is filled on commit).
    fn index_row(&self, key: &[u8]) -> Result<Vec<u8>> {
        let (at, size) = (
            self.vol.checkpoint.root_offsets[ROOT_OBJECTS],
            self.vol.checkpoint.reference_size,
        );
        let mut reference = self.vol.read_physical(self.vol.checkpoint.lcn, 1)?[at..at + size].to_vec();
        reference[..0x20].fill(0);
        Ok(row(key, &reference, if key.is_empty() { ROW_LAST } else { 0 }))
    }

    /// Splits a full page in two by size: a root becomes an index node over
    /// two new pages; another page keeps its first half, a new page after it
    /// takes the rest, and the parent gets a row for the first half.
    fn split(&mut self, tree: Tree, page: usize, order: &dyn Fn(&[u8], &[u8]) -> std::cmp::Ordering) -> Result<()> {
        let d = &self.pages[page].data;
        let h = PAGE_HEADER_SIZE + le32(d, PAGE_HEADER_SIZE) as usize;
        let (level, flags) = (d[h + 0x0c], d[h + 0x0d]);
        let rows = self.node_rows(page)?;
        if rows.len() < 2 {
            return Err(format_err!("a row larger than half a page"));
        }
        let total: usize = rows.iter().map(|(_, r)| r.len()).sum();
        let mut k = 1;
        let mut size = rows[0].1.len();
        while k < rows.len() - 1 && size + rows[k].1.len() <= total / 2 {
            size += rows[k].1.len();
            k += 1;
        }
        let (left, right) = rows.split_at(k);
        let last_left = left.last().unwrap().0.clone();
        // Children of the page (copied in this transaction) move with their
        // rows: those of the second half, and all of them when the root
        // splits (its children go to the two new pages).
        let children: Vec<(usize, bool)> = (0..self.pages.len())
            .filter_map(|i| match &self.pages[i].parent {
                Some((parent, key)) if *parent == page => Some((i, left.iter().any(|(k, _)| k == key))),
                _ => None,
            })
            .collect();
        match self.pages[page].parent.clone() {
            None => {
                let l = self.new_child(tree, page, last_left.clone());
                let r = self.new_child(tree, page, Vec::new());
                for &(c, in_left) in &children {
                    self.pages[c].parent.as_mut().unwrap().0 = if in_left { l } else { r };
                }
                let child_flags = flags & !NODE_ROOT;
                self.write_node(l, level, child_flags, left)?;
                self.write_node(r, level, child_flags, right)?;
                let index = vec![
                    (last_left.clone(), self.index_row(&last_left)?),
                    (Vec::new(), self.index_row(&[])?),
                ];
                self.write_node(page, level + 1, NODE_INDEX | NODE_ROOT, &index)?;
                self.count_pages(tree, 2);
            }
            Some((parent, key)) => {
                // The parent takes a row for the first half; a full parent
                // splits first (the page then has a parent with room).
                let index = self.index_row(&last_left)?;
                if (le32(
                    &self.pages[parent].data,
                    PAGE_HEADER_SIZE + le32(&self.pages[parent].data, PAGE_HEADER_SIZE) as usize + 8,
                ) as usize)
                    < index.len().next_multiple_of(8) + 4
                {
                    self.split(tree, parent, order)?;
                    return self.split(tree, page, order);
                }
                let r = self.new_child(tree, parent, key);
                for &(c, in_left) in &children {
                    if !in_left {
                        self.pages[c].parent.as_mut().unwrap().0 = r;
                    }
                }
                self.write_node(page, level, flags, left)?;
                self.write_node(r, level, flags, right)?;
                self.pages[page].parent = Some((parent, last_left.clone()));
                self.count_pages(tree, 1);
                // The parent's row for the first half (its old row now names
                // the second half).
                let before = |k: &[u8]| k.is_empty() || order(&last_left, k).is_lt();
                if !self.place(parent, &index, &before, false)? {
                    return Err(format_err!("no room for an index row"));
                }
            }
        }
        Ok(())
    }

    /// Moves a leaf page's rows together (in key order) so that its free
    /// space is all at the end of the row area.
    fn compact(&mut self, page: usize) -> Result<()> {
        self.mark(page);
        let d = &mut self.pages[page].data;
        let h = PAGE_HEADER_SIZE + le32(d, PAGE_HEADER_SIZE) as usize;
        let (start, index, count) = (
            le32(d, h) as usize,
            le32(d, h + 0x10) as usize,
            le32(d, h + 0x14) as usize,
        );
        let mut rows = Vec::with_capacity(count);
        for i in 0..count {
            let at = (le32(d, h + index + 4 * i) & 0xffff) as usize;
            let size = le32(d, h + at) as usize;
            rows.push(
                d.get(h + at..h + at + size)
                    .ok_or_else(|| format_err!("row outside its page"))?
                    .to_vec(),
            );
        }
        let mut at = start;
        d[h + start..h + index].fill(0);
        for (i, r) in rows.iter().enumerate() {
            d[h + at..h + at + r.len()].copy_from_slice(r);
            let entry = 0xffff_0000u32 | at as u32;
            d[h + index + 4 * i..h + index + 4 * i + 4].copy_from_slice(&entry.to_le_bytes());
            at += r.len();
        }
        d[h + 4..h + 8].copy_from_slice(&(at as u32).to_le_bytes());
        d[h + 8..h + 12].copy_from_slice(&((index - at) as u32).to_le_bytes());
        Ok(())
    }

    /// Removes the row whose key `this` accepts from a leaf page (its space
    /// counts as free; the next compaction reuses it) and uncounts it in
    /// the table's descriptor.
    fn remove(&mut self, page: usize, this: &dyn Fn(&[u8]) -> bool) -> Result<()> {
        let node = Node::at(&self.pages[page].data, PAGE_HEADER_SIZE)?;
        if node.len() == 1 && self.pages[page].parent.is_some() {
            // Its last row: the page leaves the table.
            if !node.rows().any(|r| r.is_ok_and(|r| this(r.key))) {
                return Err(Error::NotFound("row to remove".into()));
            }
            self.count_rows(page, -1);
            return self.drop_page(page);
        }
        self.remove_quiet(page, this)?;
        if self.pages[page].parent.is_some() {
            self.merge(page)?;
        }
        Ok(())
    }

    /// Removes a leaf row as `remove` does, but leaves the page where it is
    /// (also when it empties: a row is about to take its place).
    fn remove_quiet(&mut self, page: usize, this: &dyn Fn(&[u8]) -> bool) -> Result<()> {
        let node = Node::at(&self.pages[page].data, PAGE_HEADER_SIZE)?;
        let pos = node
            .rows()
            .position(|r| r.is_ok_and(|r| this(r.key)))
            .ok_or_else(|| Error::NotFound("row to remove".into()))?;
        self.mark(page);
        let d = &mut self.pages[page].data;
        let h = PAGE_HEADER_SIZE + le32(d, PAGE_HEADER_SIZE) as usize;
        let (end, free, index, count) = (
            le32(d, h + 4) as usize,
            le32(d, h + 8) as usize,
            le32(d, h + 0x10) as usize,
            le32(d, h + 0x14) as usize,
        );
        let at = (le32(d, h + index + 4 * pos) & 0xffff) as usize;
        let size = le32(d, h + at) as usize;
        // The row stays as a tombstone (flag 4): Windows walks the row area
        // row by row and takes a zeroed hole for a damaged page.
        let flags = le16(d, h + at + 8) | ROW_DELETED;
        d[h + at + 8..h + at + 10].copy_from_slice(&flags.to_le_bytes());
        d.copy_within(h + index..h + index + 4 * pos, h + index + 4);
        d[h + index..h + index + 4].fill(0);
        let put = |d: &mut Vec<u8>, at: usize, v: u32| d[at..at + 4].copy_from_slice(&v.to_le_bytes());
        if at + size == end {
            put(d, h + 4, at as u32);
        }
        put(d, h + 8, (free + size + 4) as u32);
        put(d, h + 0x10, (index + 4) as u32);
        put(d, h + 0x14, (count - 1) as u32);
        let root = self.roots[&self.pages[page].tree];
        let d = &mut self.pages[root].data;
        let rows = le64(d, PAGE_HEADER_SIZE + 0x20) - 1;
        d[PAGE_HEADER_SIZE + 0x20..PAGE_HEADER_SIZE + 0x28].copy_from_slice(&rows.to_le_bytes());
        Ok(())
    }

    /// Merges a page holding less than a quarter of its room with a
    /// sibling (the next one, the one before for the last) when their rows
    /// fill at most three quarters of a page: the later page takes the rows
    /// of both, the earlier one leaves the table. An only child of the root
    /// moves into the root when its rows fit there.
    fn merge(&mut self, page: usize) -> Result<()> {
        let size = |rows: &[(Vec<u8>, Vec<u8>)]| 0x28 + rows.iter().map(|(_, r)| r.len() + 4).sum::<usize>();
        let area = {
            let d = &self.pages[page].data;
            d.len() - PAGE_HEADER_SIZE - le32(d, PAGE_HEADER_SIZE) as usize
        };
        let rows = self.node_rows(page)?;
        if size(&rows) >= area / 4 {
            return Ok(());
        }
        let tree = self.pages[page].tree;
        let (parent, key) = self.pages[page].parent.clone().unwrap();
        let siblings = self.node_rows(parent)?;
        let pos = siblings
            .iter()
            .position(|(k, _)| *k == key)
            .ok_or_else(|| format_err!("no index row for a page"))?;
        let (first, second) = if pos + 1 < siblings.len() {
            let (k, r) = &siblings[pos + 1];
            (page, self.child(tree, parent, k.clone(), row_value(r))?)
        } else if pos > 0 {
            let (k, r) = &siblings[pos - 1];
            (self.child(tree, parent, k.clone(), row_value(r))?, page)
        } else {
            // An only child: the root may take its rows.
            return self.collapse(parent);
        };
        let mut both = self.node_rows(first)?;
        both.extend(self.node_rows(second)?);
        if size(&both) > area * 3 / 4 {
            return Ok(());
        }
        let (level, flags) = self.node_level(second);
        self.write_node(second, level, flags, &both)?;
        for p in &mut self.pages {
            if let Some((q, _)) = &mut p.parent
                && *q == first
            {
                *q = second;
            }
        }
        self.drop_page(first)
    }

    /// Takes a page whose rows are gone out of its table: its parent loses
    /// the row naming it (when that was the parent's last row, the row
    /// before takes its key), a parent left without rows goes too, a parent
    /// left nearly empty merges, its clusters become free, and a root left
    /// with one child takes that child's rows when they fit (the table
    /// loses a level).
    fn drop_page(&mut self, page: usize) -> Result<()> {
        let tree = self.pages[page].tree;
        let (parent, key) = self.pages[page]
            .parent
            .take()
            .ok_or_else(|| format_err!("dropping the root of table {tree:?}"))?;
        self.forget(page)?;
        let mut rows = self.node_rows(parent)?;
        let pos = rows
            .iter()
            .position(|(k, _)| *k == key)
            .ok_or_else(|| format_err!("no index row for a page"))?;
        rows.remove(pos);
        if rows.is_empty() {
            if self.pages[parent].parent.is_some() {
                return self.drop_page(parent);
            }
            return self.write_node(parent, 0, NODE_ROOT, &[]);
        }
        if pos == rows.len() {
            // It was the last row: the row before takes its key (the
            // node's upper bound; none for the last node of a level).
            let (k, r) = rows.pop().unwrap();
            rows.push((key.clone(), row(&key, row_value(&r), ROW_LAST)));
            for p in &mut self.pages {
                if p.parent.as_ref() == Some(&(parent, k.clone())) {
                    p.parent = Some((parent, key.clone()));
                }
            }
        }
        let (level, flags) = self.node_level(parent);
        self.write_node(parent, level, flags, &rows)?;
        if self.pages[parent].parent.is_some() {
            self.merge(parent)
        } else {
            self.collapse(parent)
        }
    }

    /// A root with one child takes that child's rows when they fit (the
    /// table loses a level).
    fn collapse(&mut self, root: usize) -> Result<()> {
        let rows = self.node_rows(root)?;
        if self.pages[root].parent.is_some() || rows.len() != 1 || self.node_level(root).0 == 0 {
            return Ok(());
        }
        let tree = self.pages[root].tree;
        let (k, r) = &rows[0];
        let child = self.child(tree, root, k.clone(), row_value(r))?;
        let child_rows = self.node_rows(child)?;
        let (level, flags) = self.node_level(child);
        if self.write_node(root, level, flags | NODE_ROOT, &child_rows).is_ok() {
            self.pages[child].parent = None;
            self.forget(child)?;
            for p in &mut self.pages {
                if let Some((q, _)) = &mut p.parent
                    && *q == child
                {
                    *q = root;
                }
            }
        }
        Ok(())
    }

    /// A whole table leaves the volume (a deleted directory's): its pages
    /// are not written and their clusters become free.
    fn drop_tree(&mut self, tree: Tree) -> Result<()> {
        self.rows(tree, &|_| false)?;
        let allocator = tree.allocator()?;
        for i in 0..self.pages.len() {
            if self.pages[i].tree == tree {
                self.pages[i].dirty = false;
                self.pages[i].parent = None;
                let old = std::mem::take(&mut self.pages[i].old);
                if !old.is_empty() {
                    self.release(allocator, &old)?;
                }
            }
        }
        self.roots.remove(&tree);
        Ok(())
    }

    /// A page left out of its table: not written, its clusters free, one
    /// page fewer below the root.
    fn forget(&mut self, page: usize) -> Result<()> {
        let tree = self.pages[page].tree;
        self.pages[page].dirty = false;
        let old = std::mem::take(&mut self.pages[page].old);
        if !old.is_empty() {
            self.release(tree.allocator()?, &old)?;
        }
        self.count_pages(tree, -1);
        Ok(())
    }

    /// A node's level and flags.
    fn node_level(&self, page: usize) -> (u8, u8) {
        let d = &self.pages[page].data;
        let h = PAGE_HEADER_SIZE + le32(d, PAGE_HEADER_SIZE) as usize;
        (d[h + 0x0c], d[h + 0x0d])
    }

    /// Where, in an index page, the reference of the row with `key` is.
    fn child_reference(&self, page: usize, key: &[u8]) -> Result<usize> {
        let data = &self.pages[page].data;
        let base = data.as_ptr() as usize;
        for row in Node::at(data, PAGE_HEADER_SIZE)?.rows() {
            let row = row?;
            if row.key == key {
                return Ok(row.value.as_ptr() as usize - base);
            }
        }
        Err(format_err!("no index row for a child page"))
    }

    /// Removes the leaf row whose key `this` accepts, wherever in the table.
    fn remove_row(&mut self, tree: Tree, this: &dyn Fn(&[u8]) -> bool) -> Result<()> {
        let at = self.find(tree, this)?;
        self.remove(at.page, this)
    }

    /// Marks a page and those above it changed.
    fn mark(&mut self, page: usize) {
        let mut i = Some(page);
        while let Some(j) = i {
            self.pages[j].dirty = true;
            i = self.pages[j].parent.as_ref().map(|p| p.0);
        }
    }

    /// Writes the changed pages and the new checkpoint.
    pub fn commit(mut self) -> Result<()> {
        self.committing = true;
        let vol = self.vol;
        let reference_size = vol.checkpoint.reference_size;
        // Object trees whose root changes: their rows in both object tables.
        let objects: Vec<u64> = self
            .roots
            .iter()
            .filter_map(|(tree, &i)| match tree {
                Tree::Object(oid) if self.pages[i].dirty => Some(*oid),
                _ => None,
            })
            .collect();
        let mut object_rows = Vec::new();
        for &oid in &objects {
            for table in [ROOT_OBJECTS, ROOT_OBJECTS_COPY] {
                let at = self.find(Tree::Root(table), &|k| k.len() >= 16 && le64(k, 8) == oid)?;
                if at.len < 0x20 + reference_size {
                    return Err(format_err!("object table row of {} bytes", at.len));
                }
                self.value_mut(at);
                object_rows.push((oid, at));
            }
        }
        // New clusters until every changed page has them: other tables
        // first, then the allocators (whose pages change while allocating).
        loop {
            let mut todo: Vec<usize> = (0..self.pages.len())
                .filter(|&i| self.pages[i].dirty && self.pages[i].new.is_none())
                .collect();
            if todo.is_empty() {
                break;
            }
            todo.sort_by_key(|&i| self.pages[i].tree.allocator().unwrap_or(usize::MAX));
            for i in todo {
                let allocator = self.pages[i].tree.allocator()?;
                let new = self.take(allocator, self.per_page())?;
                let old = self.pages[i].old.clone();
                self.release(allocator, &old)?;
                self.pages[i].new = Some(new);
            }
        }
        // Headers: the page's new virtual clusters and the new clock.
        let clock = vol.checkpoint.clock + 1;
        let mut names = HashMap::new();
        for (i, p) in self.pages.iter_mut().enumerate().filter(|(_, p)| p.dirty) {
            let new = p.new.as_ref().unwrap();
            let named = new
                .iter()
                .map(|&l| if p.tree.physical() { Ok(l) } else { vol.virtual_of(l) })
                .collect::<Result<Vec<_>>>()?;
            for k in 0..4 {
                let l = named.get(k).copied().unwrap_or(0);
                p.data[0x20 + 8 * k..0x28 + 8 * k].copy_from_slice(&l.to_le_bytes());
            }
            p.data[0x10..0x18].copy_from_slice(&clock.to_le_bytes());
            names.insert(i, named);
        }
        // References, child first: object trees, the object tables' rows
        // naming their roots, then the checkpoint's tables.
        let mut order: Vec<usize> = (0..self.pages.len()).filter(|&i| self.pages[i].dirty).collect();
        order.sort_by_key(|&i| {
            (
                matches!(self.pages[i].tree, Tree::Root(_)),
                std::cmp::Reverse(self.pages[i].depth),
            )
        });
        let mut objects_done = false;
        for &i in &order {
            if !objects_done && matches!(self.pages[i].tree, Tree::Root(_)) {
                for &(oid, at) in &object_rows {
                    let root = self.roots[&Tree::Object(oid)];
                    let data = self.pages[root].data.clone();
                    let r = &mut self.pages[at.page].data[at.value + 0x20..at.value + 0x20 + reference_size];
                    store_reference(r, &names[&root], &data)?;
                }
                objects_done = true;
            }
            if let Some((parent, key)) = self.pages[i].parent.clone() {
                let data = self.pages[i].data.clone();
                let at = self.child_reference(parent, &key)?;
                store_reference(&mut self.pages[parent].data[at..at + reference_size], &names[&i], &data)?;
            }
        }
        // The pages, then the checkpoint.
        let cluster = vol.cluster as usize;
        for &i in &order {
            let p = &self.pages[i];
            for (k, &lcn) in p.new.as_ref().unwrap().iter().enumerate() {
                vol.dev
                    .write_all_at(&p.data[k * cluster..(k + 1) * cluster], vol.offset + lcn * vol.cluster)?;
            }
        }
        vol.dev.flush()?;
        let current = vol.checkpoint.lcn;
        let slot = *vol
            .checkpoint_lcns
            .iter()
            .find(|&&l| l != current)
            .ok_or_else(|| format_err!("one checkpoint slot only"))?;
        let mut new = vol.read_physical(current, 1)?;
        for (tree, &i) in &self.roots {
            if let Tree::Root(r) = tree
                && self.pages[i].dirty
            {
                let at = vol.checkpoint.root_offsets[*r];
                let data = self.pages[i].data.clone();
                store_reference(&mut new[at..at + reference_size], &names[&i], &data)?;
            }
        }
        new[0x10..0x18].copy_from_slice(&clock.to_le_bytes());
        new[0x60..0x68].copy_from_slice(&clock.to_le_bytes());
        let generation = le64(&new, 0x68) + 1;
        new[0x68..0x70].copy_from_slice(&generation.to_le_bytes());
        for k in 0..4 {
            let l = if k == 0 { slot } else { 0 };
            new[0x20 + 8 * k..0x28 + 8 * k].copy_from_slice(&l.to_le_bytes());
        }
        let own = le32(&new, 0x58) as usize;
        let own_size = le32(&new, 0x5c) as usize;
        let mut zeroed = new.clone();
        zeroed
            .get_mut(own..own + own_size)
            .ok_or_else(|| format_err!("checkpoint reference outside the page"))?
            .fill(0);
        store_reference(&mut new[own..own + own_size], &[slot], &zeroed)?;
        vol.dev.write_all_at(&new, vol.offset + slot * vol.cluster)?;
        vol.dev.flush()?;
        Ok(())
    }
}

/// The user-settable attribute bits (read-only, hidden, system, archive,
/// temporary, offline, not content indexed).
const SETTABLE_ATTRIBUTES: u32 = 0x1 | 0x2 | 0x4 | 0x20 | 0x100 | 0x1000 | 0x2000;

impl<D: WriteAt> Volume<D> {
    /// Sets a file's four times (FILETIME).
    pub fn set_times(&mut self, path: &str, times: &Times) -> Result<()> {
        self.change_record(path, |v| {
            for (at, t) in [
                (0x28, times.created),
                (0x30, times.modified),
                (0x38, times.changed),
                (0x40, times.accessed),
            ] {
                v[at..at + 8].copy_from_slice(&t.to_le_bytes());
            }
        })
    }

    /// Turns integrity streams on or off for an empty file, as Windows
    /// allows (Set-FileIntegrity): the attribute, and the checksum kind
    /// of its inline $DATA (1, CRC32-C; 4 KiB clusters only for now).
    pub fn set_integrity(&mut self, path: &str, on: bool) -> Result<()> {
        let entry = self.lookup(path.trim_end_matches('/'))?;
        if entry.size != 0 || matches!(entry.target, Target::Directory(_)) || self.cluster != 4096 {
            return Err(Error::Unsupported(format!(
                "{path}: integrity is set on empty files (of volumes with 4 KiB clusters) only"
            )));
        }
        let record = self.record(&entry)?;
        if inline_data(&record).is_none() {
            return Err(Error::Unsupported(format!("{path}: no inline data")));
        }
        self.change_record(path, |v| {
            if let Some(at) = inline_data(v) {
                v[at - 2..at].copy_from_slice(&u16::from(on).to_le_bytes());
                let a = (le32(v, 0x48) & !INTEGRITY) | if on { INTEGRITY } else { 0 };
                v[0x48..0x4c].copy_from_slice(&a.to_le_bytes());
            }
        })
    }

    /// Sets a file's attributes (the settable bits; the others stay).
    pub fn set_attributes(&mut self, path: &str, attributes: u32) -> Result<()> {
        self.change_record(path, |v| {
            let a = (le32(v, 0x48) & !SETTABLE_ATTRIBUTES) | (attributes & SETTABLE_ATTRIBUTES);
            v[0x48..0x4c].copy_from_slice(&a.to_le_bytes());
        })
    }

    /// Overwrites bytes of a file's data where they are (as Windows does
    /// for streams without integrity checksums), then sets its modification
    /// and change times to `now` (FILETIME). The bytes must lie within the
    /// file and, for data in extents, in written clusters. An integrity
    /// stream's data is copied on write instead (the whole stream, with new
    /// checksums, for now).
    pub fn overwrite(&mut self, path: &str, offset: u64, bytes: &[u8], now: u64) -> Result<()> {
        let entry = self.lookup(path)?;
        let file = self.open_file(&entry)?;
        let data = file
            .data
            .ok_or_else(|| Error::Unsupported(format!("{path}: no data stream")))?;
        if let crate::file::Content::Extents(extents) = &data.content
            && extents.iter().any(|x| x.checksums.is_some())
        {
            let end = offset
                .checked_add(bytes.len() as u64)
                .filter(|&e| e <= data.size)
                .ok_or_else(|| Error::Unsupported(format!("{path}: writing beyond its {} bytes", data.size)))?;
            if data.size > MAX_INTEGRITY as u64 {
                return Err(Error::Unsupported(format!(
                    "{path}: integrity streams of more than {MAX_INTEGRITY} bytes"
                )));
            }
            let mut all = vec![0u8; data.size as usize];
            let mut at = 0;
            while at < all.len() {
                let n = self.read_stream(&data, at as u64, &mut all[at..])?;
                if n == 0 {
                    break;
                }
                at += n;
            }
            all[offset as usize..end as usize].copy_from_slice(bytes);
            return self.write_file(path, &all, now);
        }
        let end = offset
            .checked_add(bytes.len() as u64)
            .filter(|&e| e <= data.size)
            .ok_or_else(|| Error::Unsupported(format!("{path}: writing beyond its {} bytes", data.size)))?;
        let inline = match &data.content {
            crate::file::Content::Inline(_) => true,
            crate::file::Content::Extents(extents) => {
                let cluster = self.cluster;
                let mut covered = offset;
                for x in extents {
                    let (start, stop) = (x.vcn * cluster, (x.vcn + x.clusters) * cluster);
                    if stop <= offset || start >= end {
                        continue;
                    }
                    if !x.written || start > covered {
                        return Err(Error::Unsupported(format!(
                            "{path}: writing into a sparse or unwritten range"
                        )));
                    }
                    if x.checksums.is_some() {
                        return Err(Error::Unsupported(format!("{path}: integrity stream")));
                    }
                    covered = covered.max(stop);
                }
                if covered < end {
                    return Err(Error::Unsupported(format!("{path}: writing into a sparse range")));
                }
                for x in extents {
                    let (start, stop) = (x.vcn * cluster, (x.vcn + x.clusters) * cluster);
                    let (from, to) = (offset.max(start), end.min(stop));
                    if from >= to {
                        continue;
                    }
                    let lcn = self.translate(x.vlcn + (from - start) / cluster)?;
                    let at = self.offset + lcn * cluster + (from - start) % cluster;
                    self.dev
                        .write_all_at(&bytes[(from - offset) as usize..(to - offset) as usize], at)?;
                }
                self.dev.flush()?;
                false
            }
        };
        self.change_record(path, |v| {
            if inline && let Some(at) = inline_data(v) {
                v[at + offset as usize..at + end as usize].copy_from_slice(bytes);
            }
            v[0x30..0x38].copy_from_slice(&now.to_le_bytes());
            v[0x38..0x40].copy_from_slice(&now.to_le_bytes());
        })
    }

    /// Creates a file with `data` (up to 1 KiB kept in its record, up to
    /// 64 MiB in clusters) and all four times `now`, as Windows does: a
    /// name row with the record and a file id row in the directory, whose
    /// times become `now`. The name must be printable ASCII for now.
    pub fn create_file(&mut self, path: &str, data: &[u8], now: u64) -> Result<()> {
        let trimmed = path.trim_end_matches('/');
        let (parent, name) = trimmed.rsplit_once('/').unwrap_or(("", trimmed));
        check_name(name)?;
        if data.len() as u64 > MAX_CREATED {
            return Err(Error::Unsupported(format!("files of more than {MAX_CREATED} bytes")));
        }
        match self.lookup(trimmed) {
            Err(Error::NotFound(_)) => {}
            Ok(_) => return Err(Error::Unsupported(format!("{path} exists"))),
            Err(e) => return Err(e),
        }
        let dir = self.directory_of(parent)?;
        let common = self.shared_security(dir)?;
        // Files take integrity streams from their directory, as on Windows.
        let own = self.own_row(dir)?;
        let integrity = own.len() >= 0x4c && le32(&own, 0x48) & INTEGRITY != 0;
        {
            let mut tx = Transaction::begin(&*self)?;
            // The next file id, and the value every record of the
            // directory carries at 0x50.
            // The next file id: past the directory's counter (its object
            // table rows, 0x50: the last id given out, never lowered) and
            // every id in use; the counter follows.
            let used = self
                .object_rows(dir)?
                .iter()
                .filter(|(k, _)| k.len() >= 16 && le16(k, 0) == ROW_FILE_ID)
                .map(|(k, _)| le64(k, 8))
                .max()
                .unwrap_or(1);
            let next_id = used.max(tx.last_file_id(dir)?) + 1;
            tx.set_last_file_id(dir, next_id)?;

            let band = if data.len() > MAX_INLINE {
                Some(self.data_band(dir)?)
            } else {
                None
            };
            let file = NewRecord {
                now,
                id: next_id,
                security: common,
                integrity,
            };
            let record = self.new_record(&mut tx, band, data, file)?;
            let utf16: Vec<u8> = name.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
            // The file id row.
            let (key, value) = file_id_row(next_id, &utf16);
            tx.insert_sorted(Tree::Object(dir), &row(&key, &value, 0), &directory_key_order)?;
            // The name row with the record.
            let mut key = vec![ROW_NAME as u8, 0, 1, 0];
            key.extend(&utf16);
            tx.insert_sorted(
                Tree::Object(dir),
                &row(&key, &record, ROW_EMBEDS_NODE),
                &directory_key_order,
            )?;
            self.touch_directory(&mut tx, parent, dir, now)?;
            tx.commit()?;
        }
        self.load()
    }

    /// A file record holding `data`: inline up to MAX_INLINE, else in
    /// clusters taken from the data band `band` and written now (they are
    /// free until the commit).
    fn new_record(
        &self,
        tx: &mut Transaction<'_, D>,
        band: Option<u64>,
        data: &[u8],
        file: NewRecord,
    ) -> Result<Vec<u8>> {
        let NewRecord {
            now,
            id,
            security,
            integrity,
        } = file;
        let Some(band) = band.filter(|_| data.len() > MAX_INLINE) else {
            let mut record = resident_record(data, now, id, security);
            if integrity {
                set_integrity_bits(&mut record)?;
            }
            return Ok(record);
        };
        if integrity && (self.cluster != 4096 || data.len() > MAX_INTEGRITY) {
            return Err(Error::Unsupported(format!(
                "integrity streams of more than {MAX_INTEGRITY} bytes, or on volumes of other than 4 KiB clusters"
            )));
        }
        let (extents, allocated) = self.write_data(tx, band, data, integrity)?;
        let mut record = extent_record(data.len() as u64, allocated, &extents, now, id, security);
        if integrity {
            let a = le32(&record, 0x48) | INTEGRITY;
            record[0x48..0x4c].copy_from_slice(&a.to_le_bytes());
        }
        Ok(record)
    }

    /// Writes `data` to clusters taken from the data band `band` (free until
    /// the commit): its extents (first cluster in the stream, virtual
    /// cluster, clusters, and with `integrity` a CRC32-C per cluster) and
    /// the bytes allocated.
    #[allow(clippy::type_complexity)]
    fn write_data(
        &self,
        tx: &mut Transaction<'_, D>,
        band: u64,
        data: &[u8],
        integrity: bool,
    ) -> Result<(Vec<(u64, u64, u64, Vec<u32>)>, u64)> {
        let clusters = (data.len() as u64).div_ceil(self.cluster);
        let runs = tx.take_data(band, clusters)?;
        let mut at = 0usize;
        let mut extents = Vec::new();
        for &(lcn, n) in &runs {
            let len = ((n * self.cluster) as usize).min(data.len() - at);
            let mut buf = vec![0u8; (n * self.cluster) as usize];
            buf[..len].copy_from_slice(&data[at..at + len]);
            self.dev.write_all_at(&buf, self.offset + lcn * self.cluster)?;
            let sums = if integrity {
                buf.chunks(self.cluster as usize).map(crc32c).collect()
            } else {
                Vec::new()
            };
            extents.push(((at as u64) / self.cluster, self.virtual_of(lcn)?, n, sums));
            at += len;
        }
        self.dev.flush()?;
        Ok((extents, clusters * self.cluster))
    }

    /// Replaces a file's whole content with `data` (appending, truncating
    /// and growing alike), as a new record that keeps the file's id,
    /// creation and access times, attributes and security; its modification
    /// and change times become `now`. Files with named streams, snapshots,
    /// integrity checksums or a reparse point are refused for now, and so
    /// are files with data clusters on volumes with shared clusters.
    pub fn write_file(&mut self, path: &str, data: &[u8], now: u64) -> Result<()> {
        if data.len() as u64 > MAX_CREATED {
            return Err(Error::Unsupported(format!("files of more than {MAX_CREATED} bytes")));
        }
        let FileAt {
            parent,
            name,
            dir,
            record: old,
            runs,
            home,
            kept,
            ..
        } = self.file_at(path)?;
        if kept {
            return Err(Error::Unsupported(format!("{path}: stream snapshots or a link")));
        }
        let entry = self.lookup(path)?;
        let file = self.open_file(&entry)?;
        if !file.snapshots.is_empty() {
            return Err(Error::Unsupported(format!("{path}: snapshots")));
        }
        // Integrity streams stay so (their checksums are computed anew).
        let integrity = le32(&old, 0x48) & INTEGRITY != 0
            || matches!(&file.data, Some(crate::file::Stream {
                content: crate::file::Content::Extents(x),
                ..
            }) if x.iter().any(|x| x.checksums.is_some()));
        if self.refcounted(&runs)? {
            return Err(Error::Unsupported(format!(
                "{path}: shared (cloned or deduplicated) clusters"
            )));
        }
        let band = if data.len() > MAX_INLINE {
            Some(self.data_band(dir)?)
        } else {
            None
        };
        let (id, security) = (le64(&old, 0x80), le64(&old, 0x50));
        {
            let mut tx = Transaction::begin(&*self)?;
            // The old data clusters become free (kept until the commit).
            let clusters: Vec<u64> = runs.iter().flat_map(|&(lcn, n)| lcn..lcn + n).collect();
            tx.release(ROOT_MEDIUM_ALLOCATOR, &clusters)?;
            let file = NewRecord {
                now,
                id,
                security,
                integrity,
            };
            let mut record = self.new_record(&mut tx, band, data, file)?;
            // Kept: creation and access times, attributes (but the bits
            // that describe the content).
            record[0x28..0x30].copy_from_slice(&old[0x28..0x30]);
            record[0x40..0x48].copy_from_slice(&old[0x40..0x48]);
            let attributes = (le32(&old, 0x48) & !SPARSE) | (le32(&record, 0x48) & SPARSE);
            record[0x48..0x4c].copy_from_slice(&attributes.to_le_bytes());
            // Named streams stay, with the stream sets of those in clusters.
            let streams: Vec<Vec<u8>> = embedded_rows(&old)?
                .into_iter()
                .filter(|r| is_named_stream(r) || is_set_row(r))
                .collect();
            if !streams.is_empty() {
                let mut rows = embedded_rows(&record)?;
                for s in streams {
                    insert_attribute(&mut rows, s);
                }
                record = record_with_rows(&record, &rows, le32(&record, 0x98))?;
            }
            if let Some(home) = home {
                // A moved or linked file: the new record keeps the names,
                // the name written through gets a fresh index entry.
                let (names, _) = record_names(&old)?;
                let (_, rows) = record_names(&record)?;
                let record = record_with_names(&record, names, rows)?;
                put_record(&mut tx, home, id, &record)?;
                refresh_entry(&mut tx, dir, &name, &record)?;
                tx.commit()?;
                return self.load();
            }
            tx.remove_row(Tree::Object(dir), &|k| is_name_row(k, &name))?;
            let utf16: Vec<u8> = name.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
            let mut key = vec![ROW_NAME as u8, 0, 1, 0];
            key.extend(&utf16);
            tx.insert_sorted(
                Tree::Object(dir),
                &row(&key, &record, ROW_EMBEDS_NODE),
                &directory_key_order,
            )?;
            self.touch_directory(&mut tx, &parent, dir, now)?;
            tx.commit()?;
        }
        self.load()
    }

    /// The security descriptor reference (record 0x50) a new file in
    /// directory `dir` takes: that of a file beside it, else of the first
    /// file found from the root down (files Windows creates by default
    /// share one).
    fn shared_security(&self, dir: u64) -> Result<u64> {
        let mut dirs = std::collections::VecDeque::from([dir, ROOT_DIRECTORY]);
        let mut seen = 0;
        while let Some(d) = dirs.pop_front() {
            seen += 1;
            if seen > 256 {
                break;
            }
            for e in self.read_dir(d)? {
                match &e.target {
                    Target::Embedded(record) if record.len() >= 0x58 && le64(record, 0x50) != 0 => {
                        return Ok(le64(record, 0x50));
                    }
                    Target::Directory(child) if e.attributes & 0x400 == 0 && e.attributes & 0x4 == 0 => {
                        dirs.push_back(*child)
                    }
                    _ => {}
                }
            }
        }
        Err(Error::Unsupported("no file to share a security descriptor with".into()))
    }

    /// Deletes a name of a file: for a file whose record is in its
    /// directory entry its name row and file id row, and its data clusters
    /// (refused on volumes with shared clusters); for a moved or linked
    /// file see `unlink`.
    pub fn delete_file(&mut self, path: &str, now: u64) -> Result<()> {
        if let Target::Directory(oid) = self.lookup(path.trim_end_matches('/'))?.target {
            return self.delete_directory(path, oid, now);
        }
        let file = self.file_at(path)?;
        if let Some(home) = file.home {
            return self.unlink(file, home, now);
        }
        if file.kept {
            return Err(Error::Unsupported(format!("{path}: stream snapshots or a link")));
        }
        let FileAt {
            parent,
            name,
            dir,
            record,
            mut runs,
            stream_runs,
            ..
        } = file;
        runs.extend(stream_runs);
        let id = le64(&record, 0x80);
        if self.refcounted(&runs)? {
            return Err(Error::Unsupported(format!(
                "{path}: shared (cloned or deduplicated) clusters"
            )));
        }
        {
            let mut tx = Transaction::begin(&*self)?;
            // Its data clusters become free (kept until the commit).
            let clusters: Vec<u64> = runs.iter().flat_map(|&(lcn, n)| lcn..lcn + n).collect();
            tx.release(ROOT_MEDIUM_ALLOCATOR, &clusters)?;
            tx.remove_row(Tree::Object(dir), &|k| is_name_row(k, &name))?;
            tx.remove_row(Tree::Object(dir), &|k| {
                k.len() >= 16 && le16(k, 0) == ROW_FILE_ID && le64(k, 8) == id
            })?;
            self.touch_directory(&mut tx, &parent, dir, now)?;
            tx.commit()?;
        }
        self.load()
    }

    /// Renames a file or directory within its directory.
    pub fn rename(&mut self, path: &str, new_name: &str, now: u64) -> Result<()> {
        let trimmed = path.trim_end_matches('/');
        if let Target::Directory(_) = self.lookup(trimmed)?.target {
            let parent = trimmed.rsplit_once('/').map_or("", |(p, _)| p);
            return self.move_directory(path, &format!("{parent}/{new_name}"), now);
        }
        let file = self.file_at(path)?;
        check_name(new_name)?;
        let target = format!("{}/{new_name}", file.parent.trim_end_matches('/'));
        match self.lookup(&target) {
            Err(Error::NotFound(_)) => {}
            Ok(_) => return Err(Error::Unsupported(format!("{target} exists"))),
            Err(e) => return Err(e),
        }
        if let Some(home) = file.home {
            let parent = file.parent.clone();
            return self.move_name(file, home, &parent, new_name, now);
        }
        let FileAt {
            parent,
            name,
            dir,
            mut record,
            ..
        } = file;
        let id = le64(&record, 0x80);
        record[0x38..0x40].copy_from_slice(&now.to_le_bytes());
        let utf16: Vec<u8> = new_name.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        {
            let mut tx = Transaction::begin(&*self)?;
            tx.remove_row(Tree::Object(dir), &|k| is_name_row(k, &name))?;
            tx.remove_row(Tree::Object(dir), &|k| {
                k.len() >= 16 && le16(k, 0) == ROW_FILE_ID && le64(k, 8) == id
            })?;
            let (key, value) = file_id_row(id, &utf16);
            tx.insert_sorted(Tree::Object(dir), &row(&key, &value, 0), &directory_key_order)?;
            let mut key = vec![ROW_NAME as u8, 0, 1, 0];
            key.extend(&utf16);
            tx.insert_sorted(
                Tree::Object(dir),
                &row(&key, &record, ROW_EMBEDS_NODE),
                &directory_key_order,
            )?;
            self.touch_directory(&mut tx, &parent, dir, now)?;
            tx.commit()?;
        }
        self.load()
    }

    /// Whether any of these physical runs lies in a range the block
    /// reference count table (root 6: rows keyed by first virtual cluster
    /// and count) covers: clusters files may share (block clones,
    /// deduplication), which `refs` does not free yet.
    fn refcounted(&self, runs: &[(u64, u64)]) -> Result<bool> {
        let mut ranges = Vec::new();
        self.walk(&self.checkpoint.roots[6].clone(), false, &mut |row| {
            if row.key.len() >= 16 {
                ranges.push((le64(row.key, 0), le64(row.key, 8)));
            }
            Ok(())
        })?;
        if ranges.is_empty() {
            return Ok(false);
        }
        for &(lcn, n) in runs {
            for c in lcn..lcn + n {
                let v = self.virtual_of(c)?;
                if ranges
                    .iter()
                    .any(|&(first, count)| first <= v && v < first.saturating_add(count))
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Moves a file whose record is in its directory entry into another
    /// directory, as Windows does: the record stays in the directory it was
    /// made in (its home) as a row of type 0x40, now with a link row naming
    /// its new directory and name, its file id row there names the home,
    /// and the new directory gets an index entry pointing at the home.
    pub fn move_file(&mut self, path: &str, to: &str, now: u64) -> Result<()> {
        if let Target::Directory(_) = self.lookup(path.trim_end_matches('/'))?.target {
            return self.move_directory(path, to, now);
        }
        self.split_file(path, to, false, now)
    }

    /// Moves or renames a directory, as Windows does: its entry (times
    /// and all) goes to the new parent under the new name, the link row of
    /// its own record names them, its parent-child row follows, and the
    /// parents get new times. Moving a directory below itself, and junctions,
    /// are refused.
    fn move_directory(&mut self, path: &str, to: &str, now: u64) -> Result<()> {
        let trimmed = path.trim_end_matches('/');
        let parent = trimmed.rsplit_once('/').map_or("", |(p, _)| p);
        let entry = self.lookup(trimmed)?;
        let Target::Directory(oid) = entry.target else {
            return Err(Error::Unsupported(format!("{path} is not a directory")));
        };
        if entry.attributes & 0x400 != 0 || oid == ROOT_DIRECTORY {
            return Err(Error::Unsupported(format!("{path}: a junction or the root")));
        }
        let to = to.trim_end_matches('/');
        let (to_parent, to_name) = to.rsplit_once('/').unwrap_or(("", to));
        check_name(to_name)?;
        match self.lookup(to) {
            Err(Error::NotFound(_)) => {}
            Ok(_) => return Err(Error::Unsupported(format!("{to} exists"))),
            Err(e) => return Err(e),
        }
        let from = self.directory_of(parent)?;
        let target = self.directory_of(to_parent)?;
        let mut up = target;
        for _ in 0..4096 {
            if up == oid {
                return Err(Error::Unsupported(format!("{path}: moving a directory below itself")));
            }
            if up == ROOT_DIRECTORY {
                break;
            }
            up = self.parent_of(up)?;
        }
        let name = entry.name.as_str();
        {
            let mut tx = Transaction::begin(&*self)?;
            let at = tx.find(Tree::Object(from), &|k| is_entry_row(k, name))?;
            let value = tx.pages[at.page].data[at.value..at.value + at.len].to_vec();
            tx.remove_row(Tree::Object(from), &|k| is_entry_row(k, name))?;
            let mut key = vec![ROW_NAME as u8, 0, 2, 0];
            key.extend(utf16_bytes(to_name));
            tx.insert_sorted(Tree::Object(target), &row(&key, &value, 0), &directory_key_order)?;
            // Its own record names its parent and name.
            let own = (ROW_OWN as u32).to_le_bytes();
            let at = tx.find(Tree::Object(oid), &|k| k == own)?;
            let record = tx.pages[at.page].data[at.value..at.value + at.len].to_vec();
            let (_, rows) = record_names(&record)?;
            let record = record_with_names(&record, vec![(target, utf16_bytes(to_name))], rows)?;
            if record.len() == at.len {
                tx.value_mut(at).copy_from_slice(&record);
            } else {
                tx.remove_row(Tree::Object(oid), &|k| k == own)?;
                tx.insert_sorted(
                    Tree::Object(oid),
                    &row(&own, &record, ROW_EMBEDS_NODE),
                    &directory_key_order,
                )?;
            }
            if target != from {
                let link = |p: u64| {
                    let mut k = vec![0u8; 32];
                    k[8..16].copy_from_slice(&p.to_le_bytes());
                    k[24..32].copy_from_slice(&oid.to_le_bytes());
                    k
                };
                let old = link(from);
                tx.remove_row(Tree::Root(ROOT_PARENT_CHILD), &|k| k == old)?;
                let new = link(target);
                tx.insert_sorted(Tree::Root(ROOT_PARENT_CHILD), &row(&new, &new, 0), &u64_key_order)?;
            }
            self.touch_directory(&mut tx, parent, from, now)?;
            if target != from {
                self.touch_directory(&mut tx, to_parent, target, now)?;
            }
            tx.commit()?;
        }
        self.load()
    }

    /// Deletes an empty directory, as Windows does: its entry, its rows in
    /// both object tables and the parent-child table go, its pages become
    /// free, and its parent gets new times.
    fn delete_directory(&mut self, path: &str, oid: u64, now: u64) -> Result<()> {
        let trimmed = path.trim_end_matches('/');
        let parent = trimmed.rsplit_once('/').map_or("", |(p, _)| p);
        let entry = self.lookup(trimmed)?;
        if entry.attributes & 0x400 != 0 || oid == ROOT_DIRECTORY {
            return Err(Error::Unsupported(format!("{path}: a junction or the root")));
        }
        if !self.read_dir(oid)?.is_empty() {
            return Err(Error::Unsupported(format!("{path}: the directory is not empty")));
        }
        let rows = self.object_rows(oid)?;
        if rows.iter().any(|(k, _)| k.len() >= 2 && le16(k, 0) != ROW_OWN) {
            return Err(Error::Unsupported(format!(
                "{path}: the directory holds more than its own row"
            )));
        }
        let from = self.directory_of(parent)?;
        let name = entry.name.as_str();
        {
            let mut tx = Transaction::begin(&*self)?;
            tx.remove_row(Tree::Object(from), &|k| is_entry_row(k, name))?;
            for table in [ROOT_OBJECTS, ROOT_OBJECTS_COPY] {
                tx.remove_row(Tree::Root(table), &|k| k.len() >= 16 && le64(k, 8) == oid)?;
            }
            let mut link = vec![0u8; 32];
            link[8..16].copy_from_slice(&from.to_le_bytes());
            link[24..32].copy_from_slice(&oid.to_le_bytes());
            tx.remove_row(Tree::Root(ROOT_PARENT_CHILD), &|k| k == link)?;
            tx.drop_tree(Tree::Object(oid))?;
            self.touch_directory(&mut tx, parent, from, now)?;
            tx.commit()?;
        }
        self.load()
    }

    /// The parent of a directory (the parent-child table).
    fn parent_of(&self, oid: u64) -> Result<u64> {
        let mut parent = None;
        self.walk_while(&self.checkpoint.roots[ROOT_PARENT_CHILD].clone(), false, &mut |row| {
            if row.key.len() >= 32 && le64(row.key, 24) == oid {
                parent = Some(le64(row.key, 8));
                return Ok(false);
            }
            Ok(true)
        })?;
        parent.ok_or_else(|| format_err!("no parent of directory {oid:#x}"))
    }

    /// Gives a file whose record is in its directory entry a second name
    /// (a hard link), as Windows does: the record moves into a row of type
    /// 0x40 of its home directory with a link row for each name, and both
    /// names become index entries pointing at the home.
    pub fn link_file(&mut self, path: &str, new_path: &str, now: u64) -> Result<()> {
        self.split_file(path, new_path, true, now)
    }

    fn split_file(&mut self, path: &str, to: &str, keep: bool, now: u64) -> Result<()> {
        let file = self.file_at(path)?;
        let to = to.trim_end_matches('/');
        let (to_parent, to_name) = to.rsplit_once('/').unwrap_or(("", to));
        check_name(to_name)?;
        match self.lookup(to) {
            Err(Error::NotFound(_)) => {}
            Ok(_) => return Err(Error::Unsupported(format!("{to} exists"))),
            Err(e) => return Err(e),
        }
        if let Some(home) = file.home {
            return if keep {
                self.add_name(file, home, to_parent, to_name, now)
            } else {
                self.move_name(file, home, to_parent, to_name, now)
            };
        }
        let FileAt {
            parent,
            name,
            dir: home,
            mut record,
            ..
        } = file;
        let target = self.directory_of(to_parent)?;
        if !keep && target == home {
            return self.rename(path, to_name, now);
        }
        let utf16 = |n: &str| -> Vec<u8> { n.encode_utf16().flat_map(|c| c.to_le_bytes()).collect() };
        let id = le64(&record, 0x80);
        // The names: (directory, name), the record's link rows in order.
        let mut names = vec![(target, utf16(to_name))];
        if keep {
            names.push((home, utf16(&name)));
        }
        names.sort();
        record[0x38..0x40].copy_from_slice(&now.to_le_bytes());
        let mut rows: Vec<Vec<u8>> = names.iter().map(|(d, n)| link_row(*d, n)).collect();
        rows.extend(embedded_rows(&record)?);
        let record = record_with_rows(&record, &rows, names.len() as u32)?;
        {
            let mut tx = Transaction::begin(&*self)?;
            tx.remove_row(Tree::Object(home), &|k| is_name_row(k, &name))?;
            tx.remove_row(Tree::Object(home), &|k| {
                k.len() >= 16 && le16(k, 0) == ROW_FILE_ID && le64(k, 8) == id
            })?;
            let (key, value) = split_id_row(id, home);
            tx.insert_sorted(Tree::Object(home), &row(&key, &value, 0), &directory_key_order)?;
            let mut key = vec![0u8; 24];
            key[0..4].copy_from_slice(&[ROW_RECORD as u8, 0, 0, 0x80]);
            key[8..16].copy_from_slice(&id.to_le_bytes());
            key[16..24].copy_from_slice(&home.to_le_bytes());
            tx.insert_sorted(
                Tree::Object(home),
                &row(&key, &record, ROW_EMBEDS_NODE),
                &directory_key_order,
            )?;
            for (dir, n) in &names {
                let mut key = vec![ROW_NAME as u8, 0, 2, 0];
                key.extend(n);
                tx.insert_sorted(
                    Tree::Object(*dir),
                    &row(&key, &index_entry(id, home, &record), 0),
                    &directory_key_order,
                )?;
            }
            self.touch_directory(&mut tx, &parent, home, now)?;
            if target != home {
                self.touch_directory(&mut tx, to_parent, target, now)?;
            }
            tx.commit()?;
        }
        self.load()
    }

    /// Writes a named stream (an alternate data stream) of a file, kept in
    /// its record as Windows keeps small ones (up to 1 KiB for now), in
    /// place of a stream of that name (compared without case; it keeps its
    /// name); the file's modification, change and access times become
    /// `now`.
    pub fn write_stream(&mut self, path: &str, stream: &str, data: &[u8], now: u64) -> Result<()> {
        check_name(stream)?;
        if data.len() as u64 > MAX_CREATED {
            return Err(Error::Unsupported(format!("streams of more than {MAX_CREATED} bytes")));
        }
        let mut file = self.file_at(path)?;
        // One in clusters goes first (its set and clusters with it; the new
        // one keeps its name and, in clusters, its set id, as on Windows).
        let mut kept = (stream.to_owned(), None);
        if let Some(r) = embedded_rows(&file.record)?.iter().find(|r| {
            is_named_stream(r)
                && !is_stream_row(r, None)
                && upcased(&row_key(r)[0x10..]) == upcased(&utf16_bytes(stream))
        }) {
            kept = (utf16(&row_key(r)[0x10..]), Some(le64(row_value(r), 0x3c)));
            self.delete_stream(path, stream, now)?;
            file = self.file_at(path)?;
        }
        if data.len() > MAX_INLINE {
            let stream = embedded_rows(&file.record)?
                .iter()
                .find(|r| is_stream_row(r, Some(stream)))
                .map_or(kept.0, |r| utf16(&row_key(r)[0x10..]));
            return self.write_stream_clusters(file, &stream, data, kept.1, now);
        }
        let mut rows = embedded_rows(&file.record)?;
        // A stream of that name (without case) keeps its name.
        let name = rows
            .iter()
            .find(|r| is_stream_row(r, Some(stream)))
            .map_or_else(|| stream.to_owned(), |r| utf16(&row_key(r)[0x10..]));
        rows.retain(|r| !is_stream_row(r, Some(stream)));
        insert_attribute(&mut rows, stream_row(&name, data));
        let mut record = record_with_rows(&file.record, &rows, le32(&file.record, 0x98))?;
        for at in [0x30, 0x38, 0x40] {
            record[at..at + 8].copy_from_slice(&now.to_le_bytes());
        }
        self.store_record(&file, &record, &[])
    }

    /// Writes a named stream to clusters, as Windows does: a stream row
    /// naming its stream set (set id: `set`, else past the record's
    /// counter at 0x9c, from 0xf000), the set's header row and its live
    /// level with the extents. An inline stream of that name goes.
    fn write_stream_clusters(
        &mut self,
        file: FileAt,
        stream: &str,
        data: &[u8],
        set: Option<u64>,
        now: u64,
    ) -> Result<()> {
        if le32(&file.record, 0x48) & INTEGRITY != 0 {
            return Err(Error::Unsupported(format!(
                "{}: named streams in clusters of integrity streams",
                file.name
            )));
        }
        let last = le32(&file.record, 0x9c);
        let set = set.unwrap_or(if last == 0 { 0xf000 } else { u64::from(last) + 1 });
        let band = self.data_band(file.dir)?;
        {
            let mut tx = Transaction::begin(&*self)?;
            let (extents, allocated) = self.write_data(&mut tx, band, data, false)?;
            let mut rows = embedded_rows(&file.record)?;
            rows.retain(|r| !is_stream_row(r, Some(stream)));
            insert_attribute(
                &mut rows,
                stream_clusters_row(stream, data.len() as u64, allocated, set),
            );
            let set_key = |len: usize, level: u64, header: u64| {
                let mut k = vec![0u8; 0x50];
                k[0..8].copy_from_slice(&(len as u64).to_le_bytes());
                k[8..12].copy_from_slice(&3u32.to_le_bytes());
                k[0x10..0x30].copy_from_slice(&SET_KEY_SCHEMA);
                k[0x30..0x38].copy_from_slice(&set.to_le_bytes());
                k[0x38..0x40].copy_from_slice(&level.to_le_bytes());
                k[0x40..0x48].copy_from_slice(&8u64.to_le_bytes());
                k[0x48..0x50].copy_from_slice(&header.to_le_bytes());
                k
            };
            let header = level_set_header();
            insert_attribute(&mut rows, row(&set_key(header.len(), 8, 1), &header, 0));
            let live = level_value(data.len() as u64, allocated, &extents);
            insert_attribute(
                &mut rows,
                row(&set_key(live.len(), LIVE_STREAM, 0), &live, ROW_EMBEDS_NODE),
            );
            let mut record = record_with_rows(&file.record, &rows, le32(&file.record, 0x98))?;
            record[0x9c..0xa0].copy_from_slice(&(set.max(u64::from(last)) as u32).to_le_bytes());
            for at in [0x30, 0x38, 0x40] {
                record[at..at + 8].copy_from_slice(&now.to_le_bytes());
            }
            store_in(&mut tx, &file, &record)?;
            tx.commit()?;
        }
        self.load()
    }

    /// Deletes a named stream, as Windows does: its row, and for one in
    /// clusters the rows of its stream set, whose clusters become free
    /// (refused on volumes with shared clusters); the file's change time
    /// becomes `now`.
    pub fn delete_stream(&mut self, path: &str, stream: &str, now: u64) -> Result<()> {
        let file = self.file_at(path)?;
        let rows = embedded_rows(&file.record)?;
        let named = |r: &Vec<u8>| is_named_stream(r) && upcased(&row_key(r)[0x10..]) == upcased(&utf16_bytes(stream));
        let value = rows
            .iter()
            .find(|r| named(r))
            .map(|r| row_value(r).to_vec())
            .ok_or_else(|| Error::NotFound(format!("{path}:{stream}")))?;
        let mut rows: Vec<Vec<u8>> = rows.into_iter().filter(|r| !named(r)).collect();
        let mut clusters = Vec::new();
        if value.len() >= 0x4c && le16(&value, 2) & 0x1000 != 0 {
            let set = le64(&value, 0x3c);
            let of_set = |r: &Vec<u8>| is_set_row(r) && le64(row_key(r), 0x30) == set;
            for r in rows
                .iter()
                .filter(|r| of_set(r) && le64(row_key(r), 0x38) >= LIVE_STREAM)
            {
                for x in self.extents(row_value(r))?.iter().filter(|x| x.written) {
                    let lcn = self.translate(x.vlcn)?;
                    clusters.extend(lcn..lcn + x.clusters);
                }
            }
            rows.retain(|r| !of_set(r));
            let runs: Vec<(u64, u64)> = clusters.iter().map(|&c| (c, 1)).collect();
            if self.refcounted(&runs)? {
                return Err(Error::Unsupported(format!(
                    "{path}: shared (cloned or deduplicated) clusters"
                )));
            }
        }
        let mut record = record_with_rows(&file.record, &rows, le32(&file.record, 0x98))?;
        record[0x38..0x40].copy_from_slice(&now.to_le_bytes());
        self.store_record(&file, &record, &clusters)
    }

    /// Stores a file's changed record: in its name row, or for a moved or
    /// linked file in its home (and the index entry of the name used);
    /// `free` clusters become free.
    fn store_record(&mut self, file: &FileAt, record: &[u8], free: &[u64]) -> Result<()> {
        {
            let mut tx = Transaction::begin(&*self)?;
            tx.release(ROOT_MEDIUM_ALLOCATOR, free)?;
            store_in(&mut tx, file, record)?;
            tx.commit()?;
        }
        self.load()
    }

    /// Gives one name of a file whose record is a row of type 0x40 of
    /// directory `home` a new directory and name, as Windows does: its
    /// index entry moves and its link row changes.
    fn move_name(&mut self, file: FileAt, home: u64, to_parent: &str, to_name: &str, now: u64) -> Result<()> {
        let FileAt {
            parent,
            name,
            dir,
            mut record,
            ..
        } = file;
        let target = self.directory_of(to_parent)?;
        let id = le64(&record, 0x80);
        let (mut names, rows) = record_names(&record)?;
        let old = (dir, utf16_bytes(&name));
        let at = names
            .iter()
            .position(|n| *n == old)
            .ok_or_else(|| format_err!("{parent}/{name}: no link row for the name"))?;
        names[at] = (target, utf16_bytes(to_name));
        record[0x38..0x40].copy_from_slice(&now.to_le_bytes());
        let record = record_with_names(&record, names, rows)?;
        {
            let mut tx = Transaction::begin(&*self)?;
            tx.remove_row(Tree::Object(dir), &|k| is_entry_row(k, &name))?;
            put_record(&mut tx, home, id, &record)?;
            let mut key = vec![ROW_NAME as u8, 0, 2, 0];
            key.extend(utf16_bytes(to_name));
            tx.insert_sorted(
                Tree::Object(target),
                &row(&key, &index_entry(id, home, &record), 0),
                &directory_key_order,
            )?;
            self.touch_directory(&mut tx, &parent, dir, now)?;
            if target != dir {
                self.touch_directory(&mut tx, to_parent, target, now)?;
            }
            tx.commit()?;
        }
        self.load()
    }

    /// Gives a file whose record is a row of type 0x40 of directory `home`
    /// another name: a link row and an index entry.
    fn add_name(&mut self, file: FileAt, home: u64, to_parent: &str, to_name: &str, now: u64) -> Result<()> {
        let mut record = file.record;
        let target = self.directory_of(to_parent)?;
        let id = le64(&record, 0x80);
        let (mut names, rows) = record_names(&record)?;
        names.push((target, utf16_bytes(to_name)));
        record[0x38..0x40].copy_from_slice(&now.to_le_bytes());
        let record = record_with_names(&record, names, rows)?;
        {
            let mut tx = Transaction::begin(&*self)?;
            put_record(&mut tx, home, id, &record)?;
            let mut key = vec![ROW_NAME as u8, 0, 2, 0];
            key.extend(utf16_bytes(to_name));
            tx.insert_sorted(
                Tree::Object(target),
                &row(&key, &index_entry(id, home, &record), 0),
                &directory_key_order,
            )?;
            self.touch_directory(&mut tx, to_parent, target, now)?;
            tx.commit()?;
        }
        self.load()
    }

    /// Removes one name of a file whose record is a row of type 0x40 of
    /// directory `home`: its index entry and link row; with its last name
    /// the record, its file id row and its data clusters go too (refused
    /// on volumes with shared clusters).
    fn unlink(&mut self, file: FileAt, home: u64, now: u64) -> Result<()> {
        let FileAt {
            parent,
            name,
            dir,
            mut record,
            mut runs,
            stream_runs,
            kept,
            ..
        } = file;
        runs.extend(stream_runs);
        let id = le64(&record, 0x80);
        let (mut names, rows) = record_names(&record)?;
        let old = (dir, utf16_bytes(&name));
        let at = names
            .iter()
            .position(|n| *n == old)
            .ok_or_else(|| format_err!("{parent}/{name}: no link row for the name"))?;
        names.remove(at);
        let last = names.is_empty();
        if last && kept {
            return Err(Error::Unsupported(format!(
                "{parent}/{name}: stream snapshots or a link"
            )));
        }
        if last && self.refcounted(&runs)? {
            return Err(Error::Unsupported(format!(
                "{parent}/{name}: the volume has shared (cloned or deduplicated) clusters"
            )));
        }
        {
            let mut tx = Transaction::begin(&*self)?;
            tx.remove_row(Tree::Object(dir), &|k| is_entry_row(k, &name))?;
            let key = record_key(id, home);
            if last {
                let clusters: Vec<u64> = runs.iter().flat_map(|&(lcn, n)| lcn..lcn + n).collect();
                tx.release(ROOT_MEDIUM_ALLOCATOR, &clusters)?;
                tx.remove_row(Tree::Object(home), &|k| k == key)?;
                tx.remove_row(Tree::Object(home), &|k| {
                    k.len() >= 16 && le16(k, 0) == ROW_FILE_ID && le64(k, 8) == id
                })?;
            } else {
                record[0x38..0x40].copy_from_slice(&now.to_le_bytes());
                put_record(&mut tx, home, id, &record_with_names(&record, names, rows)?)?;
            }
            self.touch_directory(&mut tx, &parent, dir, now)?;
            tx.commit()?;
        }
        self.load()
    }

    /// A file by path (see `FileAt`).
    fn file_at(&self, path: &str) -> Result<FileAt> {
        let trimmed = path.trim_end_matches('/');
        let parent = trimmed.rsplit_once('/').map_or("", |(p, _)| p);
        let entry = self.lookup(trimmed)?;
        let (record, home) = match &entry.target {
            Target::Embedded(record) => (record.clone(), None),
            Target::Split { home, .. } => (self.record(&entry)?, Some(*home)),
            Target::Directory(_) => return Err(Error::Unsupported(format!("{path} is a directory"))),
        };
        let file = self.open_file(&entry)?;
        // The physical runs of its data, and of its named streams.
        let runs_of = |s: &crate::file::Stream| -> Result<Vec<(u64, u64)>> {
            let mut runs = Vec::new();
            if let crate::file::Content::Extents(extents) = &s.content {
                for x in extents.iter().filter(|x| x.written) {
                    runs.push((self.translate(x.vlcn)?, x.clusters));
                }
            }
            Ok(runs)
        };
        let runs = file.data.as_ref().map(runs_of).transpose()?.unwrap_or_default();
        let mut stream_runs = Vec::new();
        for (_, s) in &file.streams {
            stream_runs.extend(runs_of(s)?);
        }
        Ok(FileAt {
            parent: parent.to_owned(),
            name: entry.name.clone(),
            dir: self.directory_of(parent)?,
            record,
            runs,
            stream_runs,
            home,
            kept: !file.snapshots.is_empty() || file.reparse.is_some(),
        })
    }

    /// New times for a directory written to: in its own row and in its
    /// entry in its parent (which Windows updates later).
    fn touch_directory(&self, tx: &mut Transaction<'_, D>, path: &str, dir: u64, now: u64) -> Result<()> {
        let own = tx.find(Tree::Object(dir), &|k| k.len() >= 2 && le16(k, 0) == ROW_OWN)?;
        let v = tx.value_mut(own);
        for at in [0x30, 0x38, 0x40] {
            v[at..at + 8].copy_from_slice(&now.to_le_bytes());
        }
        let trimmed = path.trim_end_matches('/');
        if dir != ROOT_DIRECTORY
            && let Some((grand, name)) = trimmed.rsplit_once('/')
        {
            let parent = self.directory_of(grand)?;
            let at = tx.find(Tree::Object(parent), &|k| {
                k.len() > 4 && le16(k, 0) == ROW_NAME && le16(k, 2) == 2 && utf16(&k[4..]) == name
            })?;
            let v = tx.value_mut(at);
            if v.len() >= 0x30 {
                for at in [0x18, 0x20, 0x28] {
                    v[at..at + 8].copy_from_slice(&now.to_le_bytes());
                }
            }
        }
        Ok(())
    }

    /// The medium allocator's bitmap row that holds the data of a file
    /// near directory `dir` (Windows keeps file data apart from metadata).
    fn data_band(&self, dir: u64) -> Result<u64> {
        let mut bands = Vec::new();
        self.walk(
            &self.checkpoint.roots[ROOT_MEDIUM_ALLOCATOR].clone(),
            false,
            &mut |row| {
                let v = row.value;
                if v.len() >= 0x18 && le16(v, 0x12) == ALLOCATOR_BITMAP {
                    bands.push((le64(v, 0), le64(v, 8)));
                }
                Ok(())
            },
        )?;
        let mut dirs = std::collections::VecDeque::from([dir, ROOT_DIRECTORY]);
        let mut seen = 0;
        while let Some(d) = dirs.pop_front() {
            seen += 1;
            if seen > 256 {
                break;
            }
            for e in self.read_dir(d)? {
                if let Target::Directory(child) = e.target {
                    if e.attributes & 0x404 == 0 {
                        dirs.push_back(child);
                    }
                    continue;
                }
                let Ok(file) = self.open_file(&e) else { continue };
                if let Some(crate::file::Stream {
                    content: crate::file::Content::Extents(x),
                    ..
                }) = &file.data
                    && let Some(x) = x.iter().find(|x| x.written)
                {
                    let lcn = self.translate(x.vlcn)?;
                    if let Some(&(start, _)) = bands.iter().find(|(s, n)| *s <= lcn && lcn < s + n) {
                        return Ok(start);
                    }
                }
            }
        }
        Err(Error::Unsupported("no file data to place new data near".into()))
    }

    /// Creates a directory, as Windows does: a new object (the object
    /// table's counter, 0x38 of its descriptor, gives the id) with a tree
    /// of one page holding its own row (its record: a link to its parent
    /// and name, an empty $I30 index), rows in both object tables, a
    /// parent-child row, and an entry in the parent directory.
    pub fn create_directory(&mut self, path: &str, now: u64) -> Result<()> {
        let trimmed = path.trim_end_matches('/');
        let (parent, name) = trimmed.rsplit_once('/').unwrap_or(("", trimmed));
        check_name(name)?;
        match self.lookup(trimmed) {
            Err(Error::NotFound(_)) => {}
            Ok(_) => return Err(Error::Unsupported(format!("{path} exists"))),
            Err(e) => return Err(e),
        }
        let parent_oid = self.directory_of(parent)?;
        let security = self.shared_directory_security(parent_oid)?;
        let utf16: Vec<u8> = name.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        {
            let mut tx = Transaction::begin(&*self)?;
            // The object id, from the object table's counter (both copies).
            let objects = tx.root(Tree::Root(ROOT_OBJECTS))?;
            let copy = tx.root(Tree::Root(ROOT_OBJECTS_COPY))?;
            let oid = le64(&tx.pages[objects].data, PAGE_HEADER_SIZE + 0x38) + 1;
            if self.object(oid).is_ok() {
                return Err(format_err!("object {oid:#x} exists already"));
            }
            for page in [objects, copy] {
                tx.mark(page);
                tx.pages[page].data[PAGE_HEADER_SIZE + 0x38..PAGE_HEADER_SIZE + 0x40]
                    .copy_from_slice(&oid.to_le_bytes());
            }
            // The directory's tree: one page, laid out like its parent's.
            let template = tx.root(Tree::Object(parent_oid))?;
            let record = directory_record(parent_oid, &utf16, now, security);
            let page = directory_page(&tx.pages[template].data, oid, &record)?;
            tx.new_root(Tree::Object(oid), page);
            // Its rows in both object tables (the parent's as a template,
            // no file id given out yet); the reference is filled on commit.
            let at = tx.find(Tree::Root(ROOT_OBJECTS), &|k| k.len() >= 16 && le64(k, 8) == parent_oid)?;
            let mut value = tx.pages[at.page].data[at.value..at.value + at.len].to_vec();
            if value.len() < 0x58 {
                return Err(format_err!("object table row of {} bytes", value.len()));
            }
            value[0x50..0x58].copy_from_slice(&1u64.to_le_bytes());
            let mut key = vec![0u8; 16];
            key[8..16].copy_from_slice(&oid.to_le_bytes());
            for page in [objects, copy] {
                let table = tx.pages[page].tree;
                tx.insert_sorted(table, &row(&key, &value, 0), &u64_key_order)?;
            }
            // The parent-child row (key and value alike).

            let mut link = vec![0u8; 32];
            link[8..16].copy_from_slice(&parent_oid.to_le_bytes());
            link[24..32].copy_from_slice(&oid.to_le_bytes());
            tx.insert_sorted(Tree::Root(ROOT_PARENT_CHILD), &row(&link, &link, 0), &u64_key_order)?;
            // The entry in the parent.

            let mut key = vec![ROW_NAME as u8, 0, 2, 0];
            key.extend(&utf16);
            let mut entry = vec![0u8; 0x54];
            entry[8..16].copy_from_slice(&oid.to_le_bytes());
            for at in [0x10, 0x18, 0x20, 0x28] {
                entry[at..at + 8].copy_from_slice(&now.to_le_bytes());
            }
            entry[0x40..0x44].copy_from_slice(&REFS_DIRECTORY_BIT.to_le_bytes());
            tx.insert_sorted(Tree::Object(parent_oid), &row(&key, &entry, 0), &directory_key_order)?;
            self.touch_directory(&mut tx, parent, parent_oid, now)?;
            tx.commit()?;
        }
        self.load()
    }

    /// The security descriptor reference of a directory beside or below
    /// `dir` (directories created by default share one; the root's
    /// differs).
    fn shared_directory_security(&self, dir: u64) -> Result<u64> {
        let mut dirs = std::collections::VecDeque::from([dir, ROOT_DIRECTORY]);
        let mut seen = 0;
        while let Some(d) = dirs.pop_front() {
            seen += 1;
            if seen > 256 {
                break;
            }
            for e in self.read_dir(d)? {
                if let Target::Directory(child) = e.target
                    && e.attributes & 0x404 == 0
                {
                    let own = self.own_row(child)?;
                    if own.len() >= 0x58 && le64(&own, 0x50) != 0 {
                        return Ok(le64(&own, 0x50));
                    }
                    dirs.push_back(child);
                }
            }
        }
        Err(Error::Unsupported(
            "no directory to share a security descriptor with".into(),
        ))
    }

    fn directory_of(&self, parent: &str) -> Result<u64> {
        if parent.trim_matches('/').is_empty() {
            return Ok(ROOT_DIRECTORY);
        }
        match self.lookup(parent)?.target {
            Target::Directory(oid) => Ok(oid),
            _ => Err(Error::NotFound(format!("directory {parent}"))),
        }
    }

    /// Changes a file's record, in one transaction.
    fn change_record(&mut self, path: &str, change: impl FnOnce(&mut [u8])) -> Result<()> {
        let trimmed = path.trim_end_matches('/');
        let parent = trimmed.rsplit_once('/').map_or("", |(p, _)| p);
        let dir = self.directory_of(parent)?;
        let entry = self.lookup(trimmed)?;
        let name = entry.name.as_str();
        {
            let mut tx = Transaction::begin(&*self)?;
            match entry.target {
                Target::Embedded(_) => {
                    let at = tx.find(Tree::Object(dir), &|k| is_name_row(k, name))?;
                    if at.len < 0x68 {
                        return Err(format_err!("{path}: record of {} bytes", at.len));
                    }
                    change(tx.value_mut(at));
                }
                // A moved or linked file: its record in its home, and the
                // index entry of the name it was changed through (Windows
                // leaves the other names' entries as they were).
                Target::Split { home, ordinal } => {
                    let key = record_key(ordinal, home);
                    let at = tx.find(Tree::Object(home), &|k| k == key)?;
                    if at.len < 0x68 {
                        return Err(format_err!("{path}: record of {} bytes", at.len));
                    }
                    change(tx.value_mut(at));
                    let record = tx.pages[at.page].data[at.value..at.value + at.len].to_vec();
                    refresh_entry(&mut tx, dir, name, &record)?;
                }
                Target::Directory(_) => return Err(Error::Unsupported(format!("{path} is a directory"))),
            }
            tx.commit()?;
        }
        self.load()
    }
}

/// Marks a record with inline data as an integrity stream: the attribute,
/// and checksum kind 1 (CRC32-C) at 0x3a of the $DATA value.
fn set_integrity_bits(record: &mut [u8]) -> Result<()> {
    let at = inline_data(record).ok_or_else(|| format_err!("a record without inline data"))?;
    record[at - 2..at].copy_from_slice(&1u16.to_le_bytes());
    let a = le32(record, 0x48) | INTEGRITY;
    record[0x48..0x4c].copy_from_slice(&a.to_le_bytes());
    Ok(())
}

/// Where the inline data of a record (its single-instance $DATA row) starts
/// in the record.
fn inline_data(record: &[u8]) -> Option<usize> {
    let base = record.as_ptr() as usize;
    let node = Node::at(record, 0).ok()?;
    for row in node.rows() {
        let row = row.ok()?;
        if le32(row.key, 8) == 0x8000_0001 && le32(row.key, 12) & 0xffff == 0x80 {
            return Some(row.value.as_ptr() as usize - base + 0x3c);
        }
    }
    None
}

const ROW_OWN: u16 = 0x10;
/// The integrity stream attribute.
const INTEGRITY: u32 = 0x8000;
/// The largest integrity stream `refs` writes (its checksums stay in the
/// record).
const MAX_INTEGRITY: usize = 2 << 20;
/// Rows of records kept apart from the names (files moved or linked).
const ROW_RECORD: u16 = 0x40;
/// The sparse file attribute.
const SPARSE: u32 = 0x200;
const ROOT_PARENT_CHILD: usize = 4;
/// ReFS's directory bit in a directory entry's attributes.
const REFS_DIRECTORY_BIT: u32 = 0x1000_0000;
/// The row flag of rows whose value embeds a node (records, extent maps).
const ROW_EMBEDS_NODE: u16 = 1;
/// The row flag of an index node's last row (no key: above every key).
const ROW_LAST: u16 = 2;
/// Node flags: an index node, the table's root.
const NODE_INDEX: u8 = 1;
const NODE_ROOT: u8 = 2;
/// The row flag of removed rows (they stay in the row area).
const ROW_DELETED: u16 = 4;
const ROW_FILE_ID: u16 = 0x20;
const ROW_NAME: u16 = 0x30;

/// A row: header (size, key offset and length, flags, value offset and
/// length), the key and the value, each 8-aligned.
fn row(key: &[u8], value: &[u8], flags: u16) -> Vec<u8> {
    let voff = (0x10 + key.len()).next_multiple_of(8);
    let size = (voff + value.len()).next_multiple_of(8);
    let mut r = vec![0u8; size];
    r[0..4].copy_from_slice(&(size as u32).to_le_bytes());
    r[4..6].copy_from_slice(&0x10u16.to_le_bytes());
    r[6..8].copy_from_slice(&(key.len() as u16).to_le_bytes());
    r[8..10].copy_from_slice(&flags.to_le_bytes());
    r[10..12].copy_from_slice(&(voff as u16).to_le_bytes());
    r[12..14].copy_from_slice(&(value.len() as u16).to_le_bytes());
    r[0x10..0x10 + key.len()].copy_from_slice(key);
    r[voff..voff + value.len()].copy_from_slice(value);
    r
}

/// The record of a file whose data is inline, as Windows writes it: the
/// attribute tree's descriptor, the times, attributes (archive), sizes,
/// the file id, and one row: the single-instance $DATA with the bytes.
fn resident_record(data: &[u8], now: u64, id: u64, common: u64) -> Vec<u8> {
    let allocated = data.len().next_multiple_of(8);
    // The $DATA value.
    let mut value = vec![0u8; 0x3c + allocated];
    value[4..8].copy_from_slice(&((0x30 + allocated) as u32).to_le_bytes());
    value[8..12].copy_from_slice(&0x0cu32.to_le_bytes());
    value[12..16].copy_from_slice(&0x30u32.to_le_bytes());
    value[0x18..0x20].copy_from_slice(&(allocated as u64).to_le_bytes());
    value[0x20..0x28].copy_from_slice(&(data.len() as u64).to_le_bytes());
    value[0x28..0x30].copy_from_slice(&(data.len() as u64).to_le_bytes());
    value[0x30..0x38].copy_from_slice(&(allocated as u64).to_le_bytes());
    value[0x38..0x3c].copy_from_slice(&2u32.to_le_bytes());
    value[0x3c..0x3c + data.len()].copy_from_slice(data);
    let mut key = vec![0u8; 16];
    key[0..8].copy_from_slice(&(value.len() as u64).to_le_bytes());
    key[8..12].copy_from_slice(&0x8000_0001u32.to_le_bytes());
    key[12..16].copy_from_slice(&0x80u32.to_le_bytes());
    let attribute = row(&key, &value, 0);
    // The record: descriptor and file fields (0xa8 bytes), then the node
    // (header, the row, 4 free bytes, the key index).
    const NODE: usize = 0xa8;
    let index = 0x28 + attribute.len() + 4;
    let mut r = vec![0u8; NODE + index + 4];
    let put32 = |r: &mut Vec<u8>, at: usize, v: u32| r[at..at + 4].copy_from_slice(&v.to_le_bytes());
    let put64 = |r: &mut Vec<u8>, at: usize, v: u64| r[at..at + 8].copy_from_slice(&v.to_le_bytes());
    put32(&mut r, 0, NODE as u32);
    r[4..8].copy_from_slice(&[0x28, 0, 1, 0]);
    put32(&mut r, 8, 1);
    put32(&mut r, 0x0c, 0x1e0);
    put32(&mut r, 0x10, 0x1e0);
    put32(&mut r, 0x14, 2);
    put64(&mut r, 0x20, 1);
    for at in [0x28, 0x30, 0x38, 0x40] {
        put64(&mut r, at, now);
    }
    put32(&mut r, 0x48, 0x20);
    put32(&mut r, 0x4c, 8);
    put64(&mut r, 0x50, common);
    put64(&mut r, 0x58, data.len() as u64);
    put64(&mut r, 0x60, allocated as u64);
    put64(&mut r, 0x80, id);
    put32(&mut r, 0x98, 1);
    let h = NODE;
    put32(&mut r, h, 0x28);
    put32(&mut r, h + 4, (0x28 + attribute.len()) as u32);
    put32(&mut r, h + 8, 4);
    r[h + 0x0c..h + 0x10].copy_from_slice(&[0, 2, 0, 0]);
    put32(&mut r, h + 0x10, index as u32);
    put32(&mut r, h + 0x14, 1);
    put32(&mut r, h + 0x20, (index + 4) as u32);
    r[h + 0x28..h + 0x28 + attribute.len()].copy_from_slice(&attribute);
    put32(&mut r, h + index, 0xffff_0028);
    r
}

/// The order of a directory's rows: by type, file id rows by id, name rows
/// by name compared without case (UTF-16 units after upcasing).
fn directory_key_order(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let (ta, tb) = (le16(a, 0), le16(b, 0));
    if ta != tb {
        return ta.cmp(&tb);
    }
    match ta {
        ROW_NAME if a.len() >= 4 && b.len() >= 4 => upcased(&a[4..]).cmp(&upcased(&b[4..])),
        ROW_FILE_ID | 0x40 if a.len() >= 16 && b.len() >= 16 => le64(a, 8).cmp(&le64(b, 8)),
        _ => a.cmp(b),
    }
}

fn upcased(name: &[u8]) -> Vec<u16> {
    name.as_chunks::<2>()
        .0
        .iter()
        .map(|c| {
            let u = u16::from_le_bytes(*c);
            match char::from_u32(u as u32).map(|c| c.to_uppercase()) {
                Some(mut up) if up.len() == 1 => {
                    let c = up.next().unwrap() as u32;
                    if c <= 0xffff { c as u16 } else { u }
                }
                _ => u,
            }
        })
        .collect()
}

/// What a new record carries besides its data: its times (all four), file
/// id, security reference (0x50) and whether it is an integrity stream.
#[derive(Clone, Copy)]
struct NewRecord {
    now: u64,
    id: u64,
    security: u64,
    integrity: bool,
}

/// A file by path: the directory's path, the name, the directory's object
/// id, the record, the physical runs of its data and of its named
/// streams, for a file whose record is a row of type 0x40 (moved or
/// linked) its home directory, and whether it has what `refs` does not
/// delete or rewrite yet (stream snapshots, a reparse point).
struct FileAt {
    parent: String,
    name: String,
    dir: u64,
    record: Vec<u8>,
    runs: Vec<(u64, u64)>,
    stream_runs: Vec<(u64, u64)>,
    home: Option<u64>,
    kept: bool,
}

/// Names `refs` creates for now: printable ASCII, no characters Windows
/// forbids.
fn check_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 255
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|b| (0x20..0x7f).contains(&b) && !b"\\/:*?\"<>|".contains(&b))
    {
        return Err(Error::Unsupported(format!(
            "{name:?}: names of printable ASCII only, for now"
        )));
    }
    Ok(())
}

fn is_name_row(key: &[u8], name: &str) -> bool {
    key.len() > 4 && le16(key, 0) == ROW_NAME && le16(key, 2) == 1 && utf16(&key[4..]) == name
}

/// The row mapping a file id to its name: key 0x20, 0x8000, the id;
/// value: the name's offset (0x0c) and length, the name.
fn file_id_row(id: u64, utf16: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut key = vec![0u8; 24];
    key[0..4].copy_from_slice(&[0x20, 0, 0, 0x80]);
    key[8..16].copy_from_slice(&id.to_le_bytes());
    let mut value = vec![0u8; 12];
    value[8..10].copy_from_slice(&12u16.to_le_bytes());
    value[10..12].copy_from_slice(&(utf16.len() as u16).to_le_bytes());
    value.extend(utf16);
    (key, value)
}

/// Files `refs create` makes keep up to this much data in their record;
/// larger ones up to the next limit get data clusters.
const MAX_INLINE: usize = 1024;
const MAX_CREATED: u64 = 64 << 20;

/// The record of a file whose data is in extents, as Windows writes it:
/// the level set's header row (id 8) and the live level (id 0x1000) whose
/// value is an extent node; `extents` are (first cluster in the file,
/// first virtual cluster, clusters).
fn extent_record(
    size: u64,
    allocated: u64,
    extents: &[(u64, u64, u64, Vec<u32>)],
    now: u64,
    id: u64,
    common: u64,
) -> Vec<u8> {
    let multi = |len: usize, level: u64, parent: u64, header: u64| {
        let mut k = vec![0u8; 0x28];
        k[0..8].copy_from_slice(&(len as u64).to_le_bytes());
        k[8..12].copy_from_slice(&0x8000_0002u32.to_le_bytes());
        k[12..16].copy_from_slice(&0x000e_0080u32.to_le_bytes());
        k[0x10..0x18].copy_from_slice(&level.to_le_bytes());
        k[0x18..0x20].copy_from_slice(&parent.to_le_bytes());
        k[0x20..0x28].copy_from_slice(&header.to_le_bytes());
        k
    };
    let set = level_set_header();
    let set_row = row(&multi(set.len(), 8, 8, 1), &set, 0);
    let v = level_value(size, allocated, extents);
    // Flag 1: the value embeds a node (as name rows embed records).
    let live_row = row(&multi(v.len(), LIVE_STREAM, 8, 0), &v, ROW_EMBEDS_NODE);
    // The record: as for inline data, with two rows and no inline flag.
    let mut r = resident_record(&[], now, id, common);
    r.truncate(0xa8);
    r[0x20..0x28].copy_from_slice(&2u64.to_le_bytes());
    r[0x4c..0x50].fill(0);
    r[0x58..0x60].copy_from_slice(&size.to_le_bytes());
    r[0x60..0x68].copy_from_slice(&allocated.to_le_bytes());
    let index = 0x28 + set_row.len() + live_row.len();
    let mut node = vec![0u8; index + 8];
    node[0..4].copy_from_slice(&0x28u32.to_le_bytes());
    node[4..8].copy_from_slice(&(index as u32).to_le_bytes());
    node[0x0c..0x10].copy_from_slice(&[0, 2, 0, 0]);
    node[0x10..0x14].copy_from_slice(&(index as u32).to_le_bytes());
    node[0x14..0x18].copy_from_slice(&2u32.to_le_bytes());
    node[0x20..0x24].copy_from_slice(&((index + 8) as u32).to_le_bytes());
    node[0x28..0x28 + set_row.len()].copy_from_slice(&set_row);
    node[0x28 + set_row.len()..index].copy_from_slice(&live_row);
    node[index..index + 4].copy_from_slice(&0xffff_0028u32.to_le_bytes());
    node[index + 4..index + 8].copy_from_slice(&(0xffff_0000u32 | (0x28 + set_row.len()) as u32).to_le_bytes());
    r.extend(node);
    r
}

/// The header row's value of a level set: the next free level id, one
/// level.
fn level_set_header() -> Vec<u8> {
    let mut set = vec![0u8; 0x28];
    set[0..4].copy_from_slice(&0x1001u32.to_le_bytes());
    set[8..16].copy_from_slice(&1u64.to_le_bytes());
    set
}

/// A live level's value (of a file's $DATA or of a stream set): a header,
/// then a node of raw extent records keyed by their first cluster in the
/// stream.
fn level_value(size: u64, allocated: u64, extents: &[(u64, u64, u64, Vec<u32>)]) -> Vec<u8> {
    const NODE: usize = 0x88;
    let n = extents.len();
    // Integrity streams: a CRC32-C per cluster after each record (records
    // padded to 8 bytes), checksum kind 1 at 0x16.
    let integrity = extents.iter().any(|x| !x.3.is_empty());
    let slots: Vec<usize> = extents
        .iter()
        .map(|x| (24 + 4 * x.3.len()).next_multiple_of(8))
        .collect();
    let used: usize = slots.iter().sum();
    // The node ends 8-aligned: free bytes before an odd key index.
    let free = 4 * n % 8;
    let index = 0x28 + used + free;
    let mut v = vec![0u8; NODE + index + 4 * n];
    let put32 = |v: &mut Vec<u8>, at: usize, x: u32| v[at..at + 4].copy_from_slice(&x.to_le_bytes());
    let put64 = |v: &mut Vec<u8>, at: usize, x: u64| v[at..at + 8].copy_from_slice(&x.to_le_bytes());
    put32(&mut v, 0, NODE as u32);
    v[4..8].copy_from_slice(&[0x28, 0, 1, 0]);
    put32(&mut v, 8, 1);
    put32(&mut v, 0x0c, 0x200);
    put32(&mut v, 0x10, 0x200);
    put32(&mut v, 0x14, 2);
    if integrity {
        v[0x16] = 1;
    }
    put64(&mut v, 0x20, n as u64);
    put32(&mut v, 0x2c, 0x28);
    put64(&mut v, 0x30, allocated);
    put64(&mut v, 0x38, size);
    put64(&mut v, 0x40, size);
    put64(&mut v, 0x48, allocated);
    put64(&mut v, 0x50, 1);
    let h = NODE;
    put32(&mut v, h, 0x28);
    put32(&mut v, h + 4, (0x28 + used) as u32);
    put32(&mut v, h + 8, free as u32);
    v[h + 0x0c..h + 0x10].copy_from_slice(&[0, 0x0e, 0, 0]);
    put32(&mut v, h + 0x10, index as u32);
    put32(&mut v, h + 0x14, n as u32);
    put32(&mut v, h + 0x20, (index + 4 * n) as u32);
    let mut at = 0x28;
    for (i, (vcn, vlcn, clusters, sums)) in extents.iter().enumerate() {
        let r = h + at;
        put64(&mut v, r, *vlcn);
        let flags: u16 = if sums.is_empty() { 0x50 } else { 0xd0 };
        v[r + 8..r + 10].copy_from_slice(&flags.to_le_bytes());
        v[r + 10..r + 12].copy_from_slice(&((24 + 4 * sums.len()) as u16).to_le_bytes());
        put32(&mut v, r + 0x0c, *vcn as u32);
        put32(&mut v, r + 0x14, *clusters as u32);
        for (k, s) in sums.iter().enumerate() {
            put32(&mut v, r + 24 + 4 * k, *s);
        }
        put32(
            &mut v,
            h + index + 4 * i,
            at as u32 | (((*vcn).min(0xffff) as u32) << 16),
        );
        at += slots[i];
    }
    v
}

/// Keys compared as a sequence of u64 (the object and parent-child tables).
fn u64_key_order(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let field = |k: &[u8], i: usize| k.get(i..i + 8).map(|f| u64::from_le_bytes(f.try_into().unwrap()));
    (0..a.len().max(b.len()).div_ceil(8))
        .map(|i| field(a, 8 * i).cmp(&field(b, 8 * i)))
        .find(|o| o.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

/// The $I30 index stub every directory's record carries (the same 140
/// bytes on every directory Windows made).
const EMPTY_I30: [u8; 0x24] = [
    0, 0, 0, 0, 0x80, 0, 0, 0, 0x0c, 0, 0, 0, 0x30, 0, 0, 0, 0x40, 2, 1, 0, 0x16, 0, 0, 0, 0x10, 0, 0, 0, 0x70, 0, 0,
    0, 0x70, 0, 0, 0,
];

/// A directory's own record, as Windows writes it for a new directory:
/// the file fields (times, no attributes, the security reference, 1 link)
/// and two rows: the link to the parent with the name (descriptor
/// 0x000d0039; its value is its key without the length, the row lets them
/// overlap), and the empty $I30 index (0x00050090).
fn directory_record(parent: u64, name_utf16: &[u8], now: u64, security: u64) -> Vec<u8> {
    let put32 = |v: &mut Vec<u8>, at: usize, x: u32| v[at..at + 4].copy_from_slice(&x.to_le_bytes());
    let put64 = |v: &mut Vec<u8>, at: usize, x: u64| v[at..at + 8].copy_from_slice(&x.to_le_bytes());
    // The link row: key = value length, marker, descriptor, parent, 0, name.
    let klen = 0x20 + name_utf16.len();
    let link_size = (0x10 + klen).next_multiple_of(8);
    let mut link = vec![0u8; link_size];
    put32(&mut link, 0, link_size as u32);
    link[4..6].copy_from_slice(&0x10u16.to_le_bytes());
    link[6..8].copy_from_slice(&(klen as u16).to_le_bytes());
    link[0x0a..0x0c].copy_from_slice(&0x18u16.to_le_bytes());
    link[0x0c..0x0e].copy_from_slice(&((klen - 8) as u16).to_le_bytes());
    put64(&mut link, 0x10, (klen - 8) as u64);
    put32(&mut link, 0x18, 0x8000_0002);
    put32(&mut link, 0x1c, 0x000d_0039);
    put64(&mut link, 0x20, parent);
    link[0x30..0x30 + name_utf16.len()].copy_from_slice(name_utf16);
    // The $I30 row.
    let mut i30 = vec![0u8; 140];
    i30[..EMPTY_I30.len()].copy_from_slice(&EMPTY_I30);
    let mut key = vec![0u8; 0x18];
    key[0..8].copy_from_slice(&(i30.len() as u64).to_le_bytes());
    key[8..12].copy_from_slice(&0x8000_0002u32.to_le_bytes());
    key[12..16].copy_from_slice(&0x0005_0090u32.to_le_bytes());
    key[16..24].copy_from_slice(&"$I30".encode_utf16().flat_map(|c| c.to_le_bytes()).collect::<Vec<_>>());
    let index = row(&key, &i30, 0);
    // The record.
    const NODE: usize = 0xa8;
    let rows = 0x28 + link.len() + index.len();
    let mut r = vec![0u8; NODE + rows + 8];
    put32(&mut r, 0, NODE as u32);
    r[4..8].copy_from_slice(&[0x28, 0, 1, 0]);
    put32(&mut r, 8, 1);
    put32(&mut r, 0x0c, 0x1f0);
    put32(&mut r, 0x10, 0x1f0);
    put32(&mut r, 0x14, 2);
    put64(&mut r, 0x20, 2);
    for at in [0x28, 0x30, 0x38, 0x40] {
        put64(&mut r, at, now);
    }
    put64(&mut r, 0x50, security);
    put32(&mut r, 0x98, 1);
    let h = NODE;
    put32(&mut r, h, 0x28);
    put32(&mut r, h + 4, rows as u32);
    r[h + 0x0c..h + 0x10].copy_from_slice(&[0, 2, 0, 0]);
    put32(&mut r, h + 0x10, rows as u32);
    put32(&mut r, h + 0x14, 2);
    put32(&mut r, h + 0x20, (rows + 8) as u32);
    r[h + 0x28..h + 0x28 + link.len()].copy_from_slice(&link);
    r[h + 0x28 + link.len()..h + rows].copy_from_slice(&index);
    put32(&mut r, h + rows, 0xffff_0028);
    put32(&mut r, h + rows + 4, 0xffff_0000 | (0x28 + link.len()) as u32);
    r
}

/// The single page of a new directory's tree: the header and the table
/// descriptor of `template` (another directory's root page; the row count
/// set to 1, the table id at 0x48 set to `oid`), and a leaf node holding
/// the own row with `record`.
fn directory_page(template: &[u8], oid: u64, record: &[u8]) -> Result<Vec<u8>> {
    let h = PAGE_HEADER_SIZE + le32(template, PAGE_HEADER_SIZE) as usize;
    let size = template.len();
    let own = row(&(ROW_OWN as u32).to_le_bytes(), record, ROW_EMBEDS_NODE);
    let index = size - h - 4;
    if h + 0x28 + own.len() > h + index {
        return Err(format_err!("a directory record of {} bytes", record.len()));
    }
    let mut p = vec![0u8; size];
    p[..h].copy_from_slice(&template[..h]);
    p[0x48..0x50].copy_from_slice(&oid.to_le_bytes());
    p[PAGE_HEADER_SIZE + 0x20..PAGE_HEADER_SIZE + 0x28].copy_from_slice(&1u64.to_le_bytes());
    let put32 = |p: &mut Vec<u8>, at: usize, x: u32| p[at..at + 4].copy_from_slice(&x.to_le_bytes());
    put32(&mut p, h, 0x28);
    put32(&mut p, h + 4, (0x28 + own.len()) as u32);
    put32(&mut p, h + 8, (index - 0x28 - own.len()) as u32);
    p[h + 0x0c..h + 0x10].copy_from_slice(&[0, 2, 0, 0]);
    put32(&mut p, h + 0x10, index as u32);
    put32(&mut p, h + 0x14, 1);
    put32(&mut p, h + 0x20, (index + 4) as u32);
    p[h + 0x28..h + 0x28 + own.len()].copy_from_slice(&own);
    put32(&mut p, h + index, 0xffff_0028);
    Ok(p)
}

/// The key of a row (its bytes).
fn row_key(row: &[u8]) -> &[u8] {
    let (at, len) = (le16(row, 4) as usize, le16(row, 6) as usize);
    row.get(at..at + len).unwrap_or(&[])
}

fn row_value(row: &[u8]) -> &[u8] {
    let (at, len) = (le16(row, 0x0a) as usize, le16(row, 0x0c) as usize);
    row.get(at..at + len).unwrap_or(&[])
}

/// A link row of a record: the directory and the name of one of its names
/// (descriptor 0x000d0039; the value is the key without its length, and
/// the row lets them overlap, as Windows writes it).
fn link_row(parent: u64, name_utf16: &[u8]) -> Vec<u8> {
    let klen = 0x20 + name_utf16.len();
    let size = (0x10 + klen).next_multiple_of(8);
    let mut r = vec![0u8; size];
    r[0..4].copy_from_slice(&(size as u32).to_le_bytes());
    r[4..6].copy_from_slice(&0x10u16.to_le_bytes());
    r[6..8].copy_from_slice(&(klen as u16).to_le_bytes());
    r[0x0a..0x0c].copy_from_slice(&0x18u16.to_le_bytes());
    r[0x0c..0x0e].copy_from_slice(&((klen - 8) as u16).to_le_bytes());
    r[0x10..0x18].copy_from_slice(&((klen - 8) as u64).to_le_bytes());
    r[0x18..0x1c].copy_from_slice(&0x8000_0002u32.to_le_bytes());
    r[0x1c..0x20].copy_from_slice(&0x000d_0039u32.to_le_bytes());
    r[0x20..0x28].copy_from_slice(&parent.to_le_bytes());
    r[0x30..0x30 + name_utf16.len()].copy_from_slice(name_utf16);
    r
}

/// The rows of a record's attribute node, as stored.
fn embedded_rows(record: &[u8]) -> Result<Vec<Vec<u8>>> {
    let h = le32(record, 0) as usize;
    let (index, count) = (le32(record, h + 0x10) as usize, le32(record, h + 0x14) as usize);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = h + (le32(record, h + index + 4 * i) & 0xffff) as usize;
        let size = le32(record, at) as usize;
        out.push(
            record
                .get(at..at + size)
                .ok_or_else(|| format_err!("record row outside the record"))?
                .to_vec(),
        );
    }
    Ok(out)
}

/// A record with `rows` (in order) as its attribute node and `links` names
/// (a u32 at 0x98; 0x9c holds the last stream set id given out).
fn record_with_rows(record: &[u8], rows: &[Vec<u8>], links: u32) -> Result<Vec<u8>> {
    let h = le32(record, 0) as usize;
    let used: usize = rows.iter().map(Vec::len).sum();
    let free = 4 * rows.len() % 8;
    let index = 0x28 + used + free;
    let mut r = record.get(..h).ok_or_else(|| format_err!("record header"))?.to_vec();
    r.resize(h + index + 4 * rows.len(), 0);
    r[0x20..0x28].copy_from_slice(&(rows.len() as u64).to_le_bytes());
    r[0x98..0x9c].copy_from_slice(&links.to_le_bytes());
    let put = |r: &mut Vec<u8>, at: usize, v: u32| r[at..at + 4].copy_from_slice(&v.to_le_bytes());
    put(&mut r, h, 0x28);
    put(&mut r, h + 4, (0x28 + used) as u32);
    put(&mut r, h + 8, free as u32);
    r[h + 0x0c..h + 0x10].copy_from_slice(&[0, 2, 0, 0]);
    put(&mut r, h + 0x10, index as u32);
    put(&mut r, h + 0x14, rows.len() as u32);
    put(&mut r, h + 0x20, (index + 4 * rows.len()) as u32);
    let mut at = 0x28;
    for (i, row) in rows.iter().enumerate() {
        r[h + at..h + at + row.len()].copy_from_slice(row);
        put(&mut r, h + index + 4 * i, 0xffff_0000 | at as u32);
        at += row.len();
    }
    Ok(r)
}

/// A name's index entry: the file id (its ordinal in its home), the home
/// directory, the times, sizes and attributes of its record.
fn index_entry(id: u64, home: u64, record: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; 0x54];
    v[0..8].copy_from_slice(&id.to_le_bytes());
    v[8..16].copy_from_slice(&home.to_le_bytes());
    v[0x10..0x30].copy_from_slice(&record[0x28..0x48]);
    v[0x30..0x38].copy_from_slice(&record[0x60..0x68]);
    v[0x38..0x40].copy_from_slice(&record[0x58..0x60]);
    v[0x40..0x44].copy_from_slice(&record[0x48..0x4c]);
    v
}

/// The key of the record of file `id` kept apart in directory `home`.
fn record_key(id: u64, home: u64) -> Vec<u8> {
    let mut key = vec![0u8; 24];
    key[0..4].copy_from_slice(&[ROW_RECORD as u8, 0, 0, 0x80]);
    key[8..16].copy_from_slice(&id.to_le_bytes());
    key[16..24].copy_from_slice(&home.to_le_bytes());
    key
}

/// Replaces the record of file `id` kept apart in directory `home`.
fn put_record<D: WriteAt>(tx: &mut Transaction<'_, D>, home: u64, id: u64, record: &[u8]) -> Result<()> {
    let key = record_key(id, home);
    tx.remove_row(Tree::Object(home), &|k| k == key)?;
    tx.insert_sorted(
        Tree::Object(home),
        &row(&key, record, ROW_EMBEDS_NODE),
        &directory_key_order,
    )
}

/// A file's names: (directory, UTF-16 name).
type Names = Vec<(u64, Vec<u8>)>;

/// A record's names, from its link rows, and its other rows as stored.
fn record_names(record: &[u8]) -> Result<(Names, Vec<Vec<u8>>)> {
    let (mut names, mut rows) = (Vec::new(), Vec::new());
    for r in embedded_rows(record)? {
        let (ko, kl) = (le16(&r, 4) as usize, le16(&r, 6) as usize);
        let key = r
            .get(ko..ko + kl)
            .ok_or_else(|| format_err!("record row key outside the row"))?;
        if kl >= 0x20 && le32(key, 8) == 0x8000_0002 && le32(key, 12) == 0x000d_0039 {
            names.push((le64(key, 0x10), key[0x20..].to_vec()));
        } else {
            rows.push(r);
        }
    }
    Ok((names, rows))
}

/// A record with `names` (sorted by directory, then name without case)
/// as its link rows before its other rows.
fn record_with_names(record: &[u8], mut names: Names, rows: Vec<Vec<u8>>) -> Result<Vec<u8>> {
    names.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| upcased(&a.1).cmp(&upcased(&b.1))));
    let mut all: Vec<Vec<u8>> = names.iter().map(|(d, n)| link_row(*d, n)).collect();
    all.extend(rows);
    record_with_rows(record, &all, names.len() as u32)
}

fn utf16_bytes(name: &str) -> Vec<u8> {
    name.encode_utf16().flat_map(|c| c.to_le_bytes()).collect()
}

/// Puts a file's changed record in place within a transaction: in its name
/// row, or for a moved or linked file in its home (and the index entry of
/// the name used).
fn store_in<D: WriteAt>(tx: &mut Transaction<'_, D>, file: &FileAt, record: &[u8]) -> Result<()> {
    match file.home {
        Some(home) => {
            put_record(tx, home, le64(record, 0x80), record)?;
            refresh_entry(tx, file.dir, &file.name, record)
        }
        None => {
            tx.remove_row(Tree::Object(file.dir), &|k| is_name_row(k, &file.name))?;
            let mut key = vec![ROW_NAME as u8, 0, 1, 0];
            key.extend(utf16_bytes(&file.name));
            tx.insert_sorted(
                Tree::Object(file.dir),
                &row(&key, record, ROW_EMBEDS_NODE),
                &directory_key_order,
            )
        }
    }
}

/// The bytes 0x10..0x30 of every stream set row's key (the same on every
/// row Windows wrote).
const SET_KEY_SCHEMA: [u8; 0x20] = [
    0, 0, 0x0c, 0, 2, 0, 0x20, 0, 0, 0, 1, 0, 8, 0, 0x28, 0, 0, 0, 0x0e, 0, 0x18, 0, 0x30, 0, 0, 2, 0, 0, 0, 0, 0, 0,
];

/// The row of a named stream in clusters: flag 0x1000, its sizes, the
/// stream set and its live level.
fn stream_clusters_row(name: &str, size: u64, allocated: u64, set: u64) -> Vec<u8> {
    let mut v = vec![0u8; 0x74];
    v[2..4].copy_from_slice(&0x1000u16.to_le_bytes());
    v[0x04..0x08].copy_from_slice(&0x68u32.to_le_bytes());
    v[0x08..0x0c].copy_from_slice(&0x0cu32.to_le_bytes());
    v[0x0c..0x10].copy_from_slice(&0x30u32.to_le_bytes());
    v[0x18..0x20].copy_from_slice(&allocated.to_le_bytes());
    v[0x20..0x28].copy_from_slice(&size.to_le_bytes());
    v[0x28..0x30].copy_from_slice(&size.to_le_bytes());
    v[0x30..0x38].copy_from_slice(&allocated.to_le_bytes());
    v[0x38..0x3c].copy_from_slice(&2u32.to_le_bytes());
    v[0x3c..0x44].copy_from_slice(&set.to_le_bytes());
    v[0x44..0x4c].copy_from_slice(&LIVE_STREAM.to_le_bytes());
    let mut key = (v.len() as u64).to_le_bytes().to_vec();
    key.extend(0x8000_0002u32.to_le_bytes());
    key.extend(0x0005_00b0u32.to_le_bytes());
    key.extend(utf16_bytes(name));
    row(&key, &v, 0)
}

/// Copies a record's times, sizes and attributes into the index entry of
/// one of its names in directory `dir`.
fn refresh_entry<D: WriteAt>(tx: &mut Transaction<'_, D>, dir: u64, name: &str, record: &[u8]) -> Result<()> {
    let at = tx.find(Tree::Object(dir), &|k| is_entry_row(k, name))?;
    let v = tx.value_mut(at);
    if v.len() < 0x44 {
        return Err(format_err!("{name}: index entry of {} bytes", v.len()));
    }
    v[0x10..0x30].copy_from_slice(&record[0x28..0x48]);
    v[0x30..0x38].copy_from_slice(&record[0x60..0x68]);
    v[0x38..0x40].copy_from_slice(&record[0x58..0x60]);
    v[0x40..0x44].copy_from_slice(&record[0x48..0x4c]);
    Ok(())
}

/// A named stream kept in a record: key (the value's length, 0x80000002,
/// descriptor 0x000500b0, the name), value laid out like inline $DATA
/// (sizes exact, not rounded).
fn stream_row(name: &str, data: &[u8]) -> Vec<u8> {
    let n = data.len();
    let mut v = vec![0u8; 0x3c + n];
    v[0x04..0x08].copy_from_slice(&((0x30 + n) as u32).to_le_bytes());
    v[0x08..0x0c].copy_from_slice(&0x0cu32.to_le_bytes());
    v[0x0c..0x10].copy_from_slice(&0x30u32.to_le_bytes());
    for at in [0x18, 0x20, 0x28, 0x30] {
        v[at..at + 8].copy_from_slice(&(n as u64).to_le_bytes());
    }
    v[0x38..0x3c].copy_from_slice(&2u32.to_le_bytes());
    v[0x3c..].copy_from_slice(data);
    let mut key = (v.len() as u64).to_le_bytes().to_vec();
    key.extend(0x8000_0002u32.to_le_bytes());
    key.extend(0x0005_00b0u32.to_le_bytes());
    key.extend(utf16_bytes(name));
    row(&key, &v, 0)
}

/// Whether a record row is a named stream kept in the record (any, or the
/// one named `name`, compared without case).
fn is_stream_row(r: &[u8], name: Option<&str>) -> bool {
    let (key, value) = (row_key(r), row_value(r));
    key.len() >= 0x10
        && le32(key, 8) == 0x8000_0002
        && le32(key, 12) & 0xffff == 0xb0
        && value.len() >= 0x3c
        && le16(value, 0x10) == 0
        && le16(value, 2) & 0x1000 == 0
        && name.is_none_or(|n| upcased(&key[0x10..]) == upcased(&utf16_bytes(n)))
}

/// Whether a record row is a named stream (inline or in clusters; not a
/// snapshot).
fn is_named_stream(r: &[u8]) -> bool {
    let (key, value) = (row_key(r), row_value(r));
    key.len() >= 0x10
        && le32(key, 8) == 0x8000_0002
        && le32(key, 12) & 0xffff == 0xb0
        && value.len() >= 0x12
        && le16(value, 0x10) == 0
}

/// Whether a record row belongs to a stream set (the levels of named
/// streams in clusters); its set id is at 0x30 of the key.
fn is_set_row(r: &[u8]) -> bool {
    let key = row_key(r);
    key.len() >= 0x38 && le32(key, 8) == 3
}

/// Inserts a row into a record's rows in Windows' order: by marker (the
/// low bits of 0x80000001, 0x80000002, 3), attribute type, then name.
fn insert_attribute(rows: &mut Vec<Vec<u8>>, new: Vec<u8>) {
    let class = |k: &[u8]| {
        (
            k.get(8..12).map(|_| le32(k, 8) & 0x7fff_ffff),
            k.get(12..16).map(|_| le32(k, 12) & 0xffff),
            upcased(k.get(0x10..).unwrap_or(&[])),
        )
    };
    let at = class(row_key(&new));
    let pos = rows.iter().position(|r| class(row_key(r)) > at).unwrap_or(rows.len());
    rows.insert(pos, new);
}

/// Whether a directory row is the index entry (key flags 2) of `name`.
fn is_entry_row(key: &[u8], name: &str) -> bool {
    key.len() > 4 && le16(key, 0) == ROW_NAME && le16(key, 2) == 2 && utf16(&key[4..]) == name
}

/// The file id row of a file whose record is a row of type 0x40: 2, the id
/// and the home directory.
fn split_id_row(id: u64, home: u64) -> (Vec<u8>, Vec<u8>) {
    let mut key = vec![0u8; 24];
    key[0..4].copy_from_slice(&[0x20, 0, 0, 0x80]);
    key[8..16].copy_from_slice(&id.to_le_bytes());
    let mut value = vec![0u8; 24];
    value[0..8].copy_from_slice(&2u64.to_le_bytes());
    value[8..16].copy_from_slice(&id.to_le_bytes());
    value[16..24].copy_from_slice(&home.to_le_bytes());
    (key, value)
}
