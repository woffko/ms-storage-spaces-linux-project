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
