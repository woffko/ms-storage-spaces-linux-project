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

use crate::error::{Error, Result, format_err};
use crate::file::{Target, Times};
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
    /// The parent page and the offset of this page's reference in it.
    parent: Option<(usize, usize)>,
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
}

impl<'v, D: WriteAt> Transaction<'v, D> {
    pub fn new(vol: &'v Volume<D>) -> Self {
        Transaction {
            vol,
            pages: Vec::new(),
            loaded: HashMap::new(),
            roots: HashMap::new(),
            freed: Default::default(),
        }
    }

    fn per_page(&self) -> usize {
        (self.vol.page_size / self.vol.cluster) as usize
    }

    /// Copies the page a reference names (once per transaction).
    fn load(&mut self, tree: Tree, r: &PageRef, depth: usize, parent: Option<(usize, usize)>) -> Result<usize> {
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
                    children.push((value, row.value.to_vec()));
                }
            }
            let depth = self.pages[i].depth + 1;
            for (at, r) in children.into_iter().rev() {
                let r = PageRef::parse(&r)?;
                stack.push(self.load(tree, &r, depth, Some((i, at)))?);
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
            i = self.pages[j].parent.map(|p| p.0);
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
        let bitmaps = self.bitmaps(allocator)?;
        for &c in clusters {
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

    /// The root page of a directory that is one page (leaf) only.
    fn single_page(&mut self, dir: u64) -> Result<usize> {
        let root = self.root(Tree::Object(dir))?;
        let d = &self.pages[root].data;
        if d[PAGE_HEADER_SIZE + le32(d, PAGE_HEADER_SIZE) as usize + 0x0c] != 0 {
            return Err(Error::Unsupported("directories of more than one page".into()));
        }
        Ok(root)
    }

    /// The keys of a page's rows.
    fn keys(&self, page: usize) -> Result<Vec<Vec<u8>>> {
        Node::at(&self.pages[page].data, PAGE_HEADER_SIZE)?
            .rows()
            .map(|r| r.map(|r| r.key.to_vec()))
            .collect()
    }

    /// Inserts a row into a leaf page (in key order, by `before`: whether
    /// the new row goes before an existing key), in the free space between
    /// the rows and the key index at the page's end, and counts it in the
    /// table's descriptor.
    fn insert(&mut self, page: usize, row: &[u8], before: &dyn Fn(&[u8]) -> bool) -> Result<()> {
        let d = &self.pages[page].data;
        let h = PAGE_HEADER_SIZE + le32(d, PAGE_HEADER_SIZE) as usize;
        let (end, free, index, count) = (
            le32(d, h + 4) as usize,
            le32(d, h + 8) as usize,
            le32(d, h + 0x10) as usize,
            le32(d, h + 0x14) as usize,
        );
        if d[h + 0x0c] != 0 {
            return Err(Error::Unsupported("inserting into an index node".into()));
        }
        let size = row.len().next_multiple_of(8);
        if free < size + 4 {
            return Err(Error::Unsupported(
                "the page is full (splitting it is not done yet)".into(),
            ));
        }
        if end + size + 4 > index {
            self.compact(page)?;
            return self.insert(page, row, before);
        }
        // Where in the key index the row goes.
        let node = Node::at(d, PAGE_HEADER_SIZE)?;
        let mut pos = count;
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
        put(d, h + 0x14, (count + 1) as u32);
        // The table's row count (in the descriptor of its root page).
        let root = self.roots[&self.pages[page].tree];
        let d = &mut self.pages[root].data;
        let rows = le64(d, PAGE_HEADER_SIZE + 0x20) + 1;
        d[PAGE_HEADER_SIZE + 0x20..PAGE_HEADER_SIZE + 0x28].copy_from_slice(&rows.to_le_bytes());
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
        let pos = Node::at(&self.pages[page].data, PAGE_HEADER_SIZE)?
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

    /// Marks a page and those above it changed.
    fn mark(&mut self, page: usize) {
        let mut i = Some(page);
        while let Some(j) = i {
            self.pages[j].dirty = true;
            i = self.pages[j].parent.map(|p| p.0);
        }
    }

    /// Writes the changed pages and the new checkpoint.
    pub fn commit(mut self) -> Result<()> {
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
            if let Some((parent, at)) = self.pages[i].parent {
                let data = self.pages[i].data.clone();
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
    /// file and, for data in extents, in written clusters; files of
    /// integrity streams are refused (their data is copied on write).
    pub fn overwrite(&mut self, path: &str, offset: u64, bytes: &[u8], now: u64) -> Result<()> {
        let entry = self.lookup(path)?;
        let file = self.open_file(&entry)?;
        let data = file
            .data
            .ok_or_else(|| Error::Unsupported(format!("{path}: no data stream")))?;
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

    /// Creates a file with `data` (up to 1 KiB, kept in its record) and
    /// all four times `now`, as Windows does: a name row with the record
    /// and a file id row in the directory, whose times become `now`. For
    /// now the directory must fit in one page with room left at its end,
    /// and the name must be ASCII.
    pub fn create_file(&mut self, path: &str, data: &[u8], now: u64) -> Result<()> {
        let trimmed = path.trim_end_matches('/');
        let (parent, name) = trimmed.rsplit_once('/').unwrap_or(("", trimmed));
        check_name(name)?;
        if data.len() > 1024 {
            return Err(Error::Unsupported("files of more than 1 KiB (data in extents)".into()));
        }
        match self.lookup(trimmed) {
            Err(Error::NotFound(_)) => {}
            Ok(_) => return Err(Error::Unsupported(format!("{path} exists"))),
            Err(e) => return Err(e),
        }
        let dir = self.directory_of(parent)?;
        let common = self.shared_security(dir)?;
        {
            let mut tx = Transaction::new(&*self);
            let root = tx.root(Tree::Object(dir))?;
            // The next file id, and the value every record of the
            // directory carries at 0x50.
            let keys = tx.keys(root)?;
            let next_id = keys
                .iter()
                .filter(|k| k.len() >= 16 && le16(k, 0) == ROW_FILE_ID)
                .map(|k| le64(k, 8))
                .max()
                .unwrap_or(1)
                + 1;

            let record = resident_record(data, now, next_id, common);
            let utf16: Vec<u8> = name.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
            // The file id row.
            let (key, value) = file_id_row(next_id, &utf16);
            tx.insert(root, &row(&key, &value, 0), &|k| directory_key_order(&key, k).is_lt())?;
            // The name row with the record.
            let mut key = vec![ROW_NAME as u8, 0, 1, 0];
            key.extend(&utf16);
            tx.insert(root, &row(&key, &record, 1), &|k| directory_key_order(&key, k).is_lt())?;
            self.touch_directory(&mut tx, parent, dir, now)?;
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

    /// Deletes a file whose record is in its directory entry and whose
    /// data is inline (or none): its name row and file id row.
    pub fn delete_file(&mut self, path: &str, now: u64) -> Result<()> {
        let (parent, name, dir, record) = self.embedded_file(path)?;
        let id = le64(&record, 0x80);
        {
            let mut tx = Transaction::new(&*self);
            let root = tx.single_page(dir)?;
            tx.remove(root, &|k| is_name_row(k, &name))?;
            tx.remove(root, &|k| {
                k.len() >= 16 && le16(k, 0) == ROW_FILE_ID && le64(k, 8) == id
            })?;
            self.touch_directory(&mut tx, &parent, dir, now)?;
            tx.commit()?;
        }
        self.load()
    }

    /// Renames a file whose record is in its directory entry and whose
    /// data is inline (or none), within its directory.
    pub fn rename(&mut self, path: &str, new_name: &str, now: u64) -> Result<()> {
        let (parent, name, dir, mut record) = self.embedded_file(path)?;
        check_name(new_name)?;
        let target = format!("{}/{new_name}", parent.trim_end_matches('/'));
        match self.lookup(&target) {
            Err(Error::NotFound(_)) => {}
            Ok(_) => return Err(Error::Unsupported(format!("{target} exists"))),
            Err(e) => return Err(e),
        }
        let id = le64(&record, 0x80);
        record[0x38..0x40].copy_from_slice(&now.to_le_bytes());
        let utf16: Vec<u8> = new_name.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        {
            let mut tx = Transaction::new(&*self);
            let root = tx.single_page(dir)?;
            tx.remove(root, &|k| is_name_row(k, &name))?;
            tx.remove(root, &|k| {
                k.len() >= 16 && le16(k, 0) == ROW_FILE_ID && le64(k, 8) == id
            })?;
            let (key, value) = file_id_row(id, &utf16);
            tx.insert(root, &row(&key, &value, 0), &|k| directory_key_order(&key, k).is_lt())?;
            let mut key = vec![ROW_NAME as u8, 0, 1, 0];
            key.extend(&utf16);
            tx.insert(root, &row(&key, &record, 1), &|k| directory_key_order(&key, k).is_lt())?;
            self.touch_directory(&mut tx, &parent, dir, now)?;
            tx.commit()?;
        }
        self.load()
    }

    /// For a file whose record is in its directory entry and whose data is
    /// inline or none: its directory's path, its name, the directory's
    /// object id and the record.
    fn embedded_file(&self, path: &str) -> Result<(String, String, u64, Vec<u8>)> {
        let trimmed = path.trim_end_matches('/');
        let (parent, name) = trimmed.rsplit_once('/').unwrap_or(("", trimmed));
        let entry = self.lookup(trimmed)?;
        let Target::Embedded(record) = &entry.target else {
            return Err(Error::Unsupported(format!(
                "{path}: only files whose record is in their directory entry"
            )));
        };
        let file = self.open_file(&entry)?;
        let extents = file
            .data
            .iter()
            .chain(file.streams.iter().chain(&file.snapshots).map(|(_, s)| s))
            .any(|s| matches!(s.content, crate::file::Content::Extents(_)));
        if extents || file.reparse.is_some() {
            return Err(Error::Unsupported(format!(
                "{path}: only files whose data is in their record (no links)"
            )));
        }
        Ok((
            parent.to_owned(),
            name.to_owned(),
            self.directory_of(parent)?,
            record.clone(),
        ))
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

    fn directory_of(&self, parent: &str) -> Result<u64> {
        if parent.trim_matches('/').is_empty() {
            return Ok(ROOT_DIRECTORY);
        }
        match self.lookup(parent)?.target {
            Target::Directory(oid) => Ok(oid),
            _ => Err(Error::NotFound(format!("directory {parent}"))),
        }
    }

    /// Changes the record of a file whose record is embedded in its
    /// directory entry, in one transaction.
    fn change_record(&mut self, path: &str, change: impl FnOnce(&mut [u8])) -> Result<()> {
        let trimmed = path.trim_end_matches('/');
        let (parent, name) = trimmed.rsplit_once('/').unwrap_or(("", trimmed));
        let dir = if parent.trim_matches('/').is_empty() {
            ROOT_DIRECTORY
        } else {
            match self.lookup(parent)?.target {
                Target::Directory(oid) => oid,
                _ => return Err(Error::NotFound(format!("directory {parent}"))),
            }
        };
        let entry = self.lookup(trimmed)?;
        if !matches!(entry.target, Target::Embedded(_)) {
            return Err(Error::Unsupported(format!(
                "{path}: only files whose record is in their directory entry"
            )));
        }
        {
            let mut tx = Transaction::new(&*self);
            let at = tx.find(Tree::Object(dir), &|k| {
                k.len() > 4 && le16(k, 0) == 0x30 && le16(k, 2) == 1 && utf16(&k[4..]) == name
            })?;
            if at.len < 0x68 {
                return Err(format_err!("{path}: record of {} bytes", at.len));
            }
            change(tx.value_mut(at));
            tx.commit()?;
        }
        self.load()
    }
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
    put64(&mut r, 0x98, 1);
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
