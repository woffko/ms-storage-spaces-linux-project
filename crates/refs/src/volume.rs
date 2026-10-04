//! A ReFS volume: the boot sector, the superblock, the current checkpoint
//! and its roots, the container table that translates virtual cluster
//! numbers, and the object table.

use std::collections::BTreeMap;

use storage_spaces::io::ReadAt;

use crate::boot::{BOOT_SECTOR_SIZE, BootSector};
use crate::checksum::{crc32c, crc64, sha256};
use crate::error::{Error, Result, format_err};
use crate::node::{Node, Row};
use crate::page::{PAGE_HEADER_SIZE, PageHeader, PageRef, V1_BLOCK, V1_HEADER_SIZE};
use crate::util::{le16, le32, le64};

/// Cluster of the primary superblock.
pub const SUPERBLOCK_LCN: u64 = 0x1e;
/// Roots of the checkpoint.
pub const ROOT_OBJECTS: usize = 0;
pub const ROOT_CONTAINERS: usize = 7;
pub const ROOT_CONTAINERS_COPY: usize = 8;
/// Per-container rows (compacted containers' streams).
pub const ROOT_CONTAINER_INDEX: usize = 10;
/// Object ids.
pub const ROOT_DIRECTORY: u64 = 0x600;
/// Deepest tree walked (real trees have a few levels).
const MAX_DEPTH: usize = 16;

#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub lcn: u64,
    pub clock: u64,
    pub major: u16,
    pub minor: u16,
    pub flags: u32,
    /// Bytes of a page reference: 0x30 (CRC64), 0x48 (SHA-256), 0x68.
    pub reference_size: usize,
    pub roots: Vec<PageRef>,
    /// Where each root's reference is in the checkpoint.
    pub(crate) root_offsets: Vec<usize>,
}

pub struct Volume<D> {
    pub(crate) dev: D,
    /// Byte offset of the volume on the device.
    pub(crate) offset: u64,
    pub boot: BootSector,
    pub cluster: u64,
    /// Bytes of a metadata page: 16 KiB on 4 KiB clusters, one cluster on
    /// 64 KiB clusters, a block of 16 KiB on ReFS 1.x.
    pub page_size: u64,
    /// ReFS 1.x: pages are blocks of 16 KiB named by their number from the
    /// start of the volume (24-byte references, 48-byte headers), there
    /// is no container table, and data runs count blocks too.
    pub(crate) v1: bool,
    pub volume_guid: [u8; 16],
    pub checkpoint: Checkpoint,
    /// The clusters of both checkpoint copies (the superblock's list).
    pub checkpoint_lcns: Vec<u64>,
    /// Clusters per container, and each container's first physical cluster.
    pub clusters_per_container: u64,
    pub(crate) containers: BTreeMap<u64, u64>,
    /// The class of each container, by its first physical cluster (u32 at
    /// 0x14 of its row: 0 data, 1 metadata, 0x2000 not handed out yet,
    /// 0x4000 full of data, others the log's and reserved areas).
    pub(crate) container_classes: BTreeMap<u64, u32>,
    /// The log's state, read once (commits by `refs` leave the log alone).
    log: std::sync::OnceLock<LogState>,
    objects: BTreeMap<u64, PageRef>,
    /// The clock of the first checkpoint `refs` wrote through this value
    /// (0: none yet).
    pub(crate) own_since: std::sync::atomic::AtomicU64,
    /// Compacted (compressed) containers, by id (not in `containers`).
    pub(crate) compacted: crate::compress::Map,
    /// The last units decompressed.
    units: std::sync::Mutex<Vec<Unit>>,
}

/// A decompressed unit: (container, range, unit), its bytes.
type Unit = ((u64, usize, usize), std::sync::Arc<Vec<u8>>);

/// Units of compacted containers kept decompressed.
const UNITS_KEPT: usize = 8;

/// Verifies a superblock's or checkpoint's own checksum: the descriptor
/// at `at` (a page reference to the page itself), over the first cluster
/// with the whole descriptor zeroed.
fn self_checksum_ok(page: &[u8], at: usize, len: usize, cluster: usize, v1: bool) -> bool {
    let Some(desc) = page.get(at..at + len) else {
        return false;
    };
    let parsed = if v1 {
        PageRef::parse_v1(desc)
    } else {
        PageRef::parse(desc)
    };
    let Ok(r) = parsed else {
        return false;
    };
    let mut copy = page[..cluster.min(page.len())].to_vec();
    if at + len > copy.len() {
        return false;
    }
    copy[at..at + len].fill(0);
    match r.checksum_kind {
        _ if cfg!(fuzzing) => true,
        0 => true,
        1 => crc32c(&copy).to_le_bytes()[..] == r.checksum[..],
        2 => crc64(&copy).to_le_bytes()[..] == r.checksum[..],
        4 => sha256(&copy)[..] == r.checksum[..],
        _ => false,
    }
}

impl<D: ReadAt> Volume<D> {
    /// Opens the volume that starts at byte `offset` of `dev`.
    pub fn open(dev: D, offset: u64) -> Result<Self> {
        let mut sector = vec![0u8; BOOT_SECTOR_SIZE];
        dev.read_exact_at(&mut sector, offset)?;
        let boot = BootSector::parse(&sector)?;
        let cluster = boot.cluster_size();
        let v1 = boot.major == 1;
        let page_size = if v1 { V1_BLOCK } else { cluster.max(16384) };
        let mut vol = Volume {
            dev,
            offset,
            boot,
            cluster,
            page_size,
            v1,
            volume_guid: [0; 16],
            checkpoint: Checkpoint {
                lcn: 0,
                clock: 0,
                major: 0,
                minor: 0,
                flags: 0,
                reference_size: 0,
                roots: Vec::new(),
                root_offsets: Vec::new(),
            },
            clusters_per_container: 0,
            containers: BTreeMap::new(),
            container_classes: BTreeMap::new(),
            objects: BTreeMap::new(),
            checkpoint_lcns: Vec::new(),
            log: std::sync::OnceLock::new(),
            own_since: std::sync::atomic::AtomicU64::new(0),
            compacted: BTreeMap::new(),
            units: std::sync::Mutex::new(Vec::new()),
        };
        vol.load()?;
        Ok(vol)
    }

    /// Reads the superblock, the current checkpoint and the tables cached
    /// from it (again after a commit).
    pub(crate) fn load(&mut self) -> Result<()> {
        if self.v1 {
            return self.load_v1();
        }
        let (cluster, page_size) = (self.cluster, self.page_size);
        let total_clusters = self.boot.volume_size() / cluster;
        // The superblock: the primary, then the two copies at the end.
        let mut supb = None;
        for lcn in [
            SUPERBLOCK_LCN,
            total_clusters.wrapping_sub(2),
            total_clusters.wrapping_sub(3),
        ] {
            if lcn >= total_clusters {
                continue;
            }
            let page = self.read_physical(lcn, 1)?;
            let (at, len) = (le32(&page, 0x78) as usize, le32(&page, 0x7c) as usize);
            if &page[0..4] == b"SUPB" && self_checksum_ok(&page, at, len, cluster as usize, false) {
                supb = Some(page);
                break;
            }
        }
        let supb = supb.ok_or_else(|| format_err!("no valid superblock"))?;
        self.volume_guid = supb[0x50..0x60].try_into().unwrap();
        let (list, count) = (le32(&supb, 0x70) as usize, le32(&supb, 0x74) as usize);
        self.checkpoint_lcns.clear();
        if count != 2 || list + 16 > supb.len() {
            return Err(format_err!("superblock lists {count} checkpoints"));
        }
        // The checkpoint with the higher clock among the valid ones.
        let mut best: Option<(u64, u64, Vec<u8>)> = None;
        for i in 0..count {
            let lcn = le64(&supb, list + 8 * i);
            self.checkpoint_lcns.push(lcn);
            if lcn >= total_clusters {
                continue;
            }
            let page = self.read_physical(lcn, page_size / cluster)?;
            let reference_size = le32(&page, 0x5c) as usize;
            if &page[0..4] != b"CHKP"
                || !self_checksum_ok(
                    &page,
                    le32(&page, 0x58) as usize,
                    reference_size,
                    cluster as usize,
                    false,
                )
            {
                continue;
            }
            let clock = le64(&page, 0x60);
            if best.as_ref().is_none_or(|b| clock > b.1) {
                best = Some((lcn, clock, page));
            }
        }
        let (lcn, clock, chkp) = best.ok_or_else(|| format_err!("no valid checkpoint"))?;
        self.checkpoint = Self::parse_checkpoint(lcn, clock, &chkp)?;
        self.load_containers()?;
        self.load_objects()
    }

    /// The other checkpoint the superblock lists, when it is valid and
    /// older than the current one.
    pub(crate) fn older_checkpoint(&self) -> Option<Checkpoint> {
        if self.v1 {
            return None;
        }
        let lcn = *self.checkpoint_lcns.iter().find(|&&l| l != self.checkpoint.lcn)?;
        let page = self.read_physical(lcn, self.page_size / self.cluster).ok()?;
        let ok = page.get(0..4) == Some(b"CHKP")
            && self_checksum_ok(
                &page,
                le32(&page, 0x58) as usize,
                le32(&page, 0x5c) as usize,
                self.cluster as usize,
                false,
            );
        let clock = le64(&page, 0x60);
        if !ok || clock >= self.checkpoint.clock {
            return None;
        }
        Self::parse_checkpoint(lcn, clock, &page).ok()
    }

    /// The pages of the tree below `r` (their references), but those
    /// whose first cluster `skip` holds and the pages below them; with
    /// `lenient`, also those that fail to read (overwritten since).
    pub(crate) fn tree_pages(
        &self,
        r: &PageRef,
        physical: bool,
        skip: &std::collections::HashSet<u64>,
        lenient: bool,
        out: &mut Vec<PageRef>,
    ) -> Result<()> {
        let mut todo = vec![(r.clone(), 0)];
        while let Some((r, depth)) = todo.pop() {
            if depth > MAX_DEPTH {
                return Err(format_err!("B+-tree deeper than {MAX_DEPTH} levels"));
            }
            if skip.contains(&r.lcns[0]) {
                continue;
            }
            let page = match self.read_page(&r, physical) {
                Ok(p) => p,
                Err(_) if lenient => continue,
                Err(e) => return Err(e),
            };
            let node = self.node_in(&page, self.node_offset())?;
            if !node.is_leaf() {
                for row in node.rows() {
                    todo.push((self.page_ref(row?.value)?, depth + 1));
                }
            }
            out.push(r);
        }
        Ok(())
    }

    /// The pages only the older checkpoint references that are still
    /// allocated, as Windows leaves them: it keeps pages a checkpoint
    /// replaced (those of the object tables, at least) allocated until it
    /// writes the next checkpoint (the older one still needs them), and
    /// frees them then. By allocator (1 medium, 2 container, 12 small),
    /// their physical clusters; only pages whose content still matches
    /// the older checkpoint's checksums. Tables both checkpoints reference
    /// alike are not walked.
    pub fn deferred_pages(&self) -> Result<Vec<(usize, Vec<u64>)>> {
        use std::collections::HashSet;
        let Some(old) = self.older_checkpoint().filter(|_| !self.v1) else {
            return Ok(Vec::new());
        };
        let per_page = (self.page_size / self.cluster) as usize;
        let allocator = |root: Option<usize>| match root {
            Some(7 | 8 | 12) => 12,
            Some(1 | 2 | 6 | 11) => 2,
            _ => 1,
        };
        // (table, older root, current root)
        let mut tables: Vec<(Option<usize>, PageRef, Option<PageRef>)> = Vec::new();
        for (i, r) in old.roots.iter().enumerate() {
            let current = self.checkpoint.roots.get(i).cloned();
            if current.as_ref().is_some_and(|c| c.lcns == r.lcns) {
                continue;
            }
            tables.push((Some(i), r.clone(), current));
        }
        let reference_size = old.reference_size;
        // An older object table overwritten since: nothing to free.
        let objects = self.walk(&old.roots[ROOT_OBJECTS], false, &mut |row| {
            if row.key.len() < 16 {
                return Err(format_err!("object table key of {} bytes", row.key.len()));
            }
            let reference = row
                .value
                .get(0x20..0x20 + reference_size)
                .ok_or_else(|| format_err!("object table row of {} bytes", row.value.len()))?;
            let r = PageRef::parse(reference)?;
            let current = self.objects.get(&le64(row.key, 8)).cloned();
            if current.as_ref().is_none_or(|c| c.lcns != r.lcns) {
                tables.push((None, r, current));
            }
            Ok(())
        });
        if objects.is_err() {
            return Ok(Vec::new());
        }
        let mut allocators = BTreeMap::new();
        for a in [1, 2, 12] {
            allocators.insert(a, crate::check::Allocator::read(self, a)?);
        }
        let mut out = Vec::new();
        for (root, old_root, current) in tables {
            let physical = matches!(root, Some(7 | 8 | 12));
            let mut now = Vec::new();
            if let Some(c) = &current {
                self.tree_pages(c, physical, &HashSet::new(), false, &mut now)?;
            }
            let now: HashSet<u64> = now.iter().map(|r| r.lcns[0]).collect();
            let mut then = Vec::new();
            self.tree_pages(&old_root, physical, &now, true, &mut then)?;
            for r in then {
                let lcns = r.lcns[..per_page]
                    .iter()
                    .map(|&l| if physical { Ok(l) } else { self.translate(l) })
                    .collect::<Result<Vec<_>>>()?;
                if lcns.iter().all(|&c| allocators[&allocator(root)].used(c)) {
                    out.push((allocator(root), lcns));
                }
            }
        }
        Ok(out)
    }

    /// ReFS 1.x: the superblock at block 0x1e (the volume GUID at 0x30, at
    /// 0x50 the offset and at 0x54 the number of its checkpoints' block
    /// numbers, at 0x58 and 0x5c its own reference), the checkpoint with
    /// the higher sequence number (u64 at 8 of the block header), and its
    /// roots; no container table: one container stands for the volume,
    /// whose clusters are where they say.
    fn load_v1(&mut self) -> Result<()> {
        let supb = self.read_block(SUPERBLOCK_LCN)?;
        let (at, len) = (le32(&supb, 0x58) as usize, le32(&supb, 0x5c) as usize);
        if le64(&supb, 0) != SUPERBLOCK_LCN || !self_checksum_ok(&supb, at, len, V1_BLOCK as usize, true) {
            return Err(format_err!("no valid superblock"));
        }
        self.volume_guid = supb[0x30..0x40].try_into().unwrap();
        let (list, count) = (le32(&supb, 0x50) as usize, le32(&supb, 0x54) as usize);
        if count != 2 || list + 16 > supb.len() {
            return Err(format_err!("superblock lists {count} checkpoints"));
        }
        self.checkpoint_lcns.clear();
        let mut best: Option<(u64, u64, Vec<u8>)> = None;
        for i in 0..count {
            let block = le64(&supb, list + 8 * i);
            self.checkpoint_lcns.push(block);
            let Ok(page) = self.read_block(block) else {
                continue;
            };
            let (at, len) = (le32(&page, 0x38) as usize, le32(&page, 0x3c) as usize);
            if le64(&page, 0) != block || !self_checksum_ok(&page, at, len, V1_BLOCK as usize, true) {
                continue;
            }
            let clock = le64(&page, 8);
            if best.as_ref().is_none_or(|b| clock > b.1) {
                best = Some((block, clock, page));
            }
        }
        let (block, clock, chkp) = best.ok_or_else(|| format_err!("no valid checkpoint"))?;
        self.checkpoint = Self::parse_checkpoint_v1(block, clock, &chkp)?;
        let clusters = (self.boot.volume_size() / self.cluster).max(1);
        self.clusters_per_container = clusters.next_power_of_two();
        self.containers = BTreeMap::from([(0, 0)]);
        self.container_classes = BTreeMap::from([(0, 0)]);
        self.compacted.clear();
        self.units.lock().unwrap().clear();
        self.load_objects()
    }

    /// A ReFS 1.x checkpoint: the version at 0x34 and 0x36, at 0x58 the
    /// number of roots and from 0x5c their offsets (24-byte references).
    fn parse_checkpoint_v1(block: u64, clock: u64, page: &[u8]) -> Result<Checkpoint> {
        let count = le32(page, 0x58) as usize;
        if !(6..=32).contains(&count) {
            return Err(format_err!("checkpoint with {count} roots"));
        }
        let root_offsets: Vec<usize> = (0..count).map(|i| le32(page, 0x5c + 4 * i) as usize).collect();
        let roots = root_offsets
            .iter()
            .enumerate()
            .map(|(i, &at)| {
                page.get(at..at + 0x18)
                    .ok_or_else(|| format_err!("checkpoint root {i} outside the page"))
                    .and_then(PageRef::parse_v1)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Checkpoint {
            lcn: block,
            clock,
            major: crate::util::le16(page, 0x34),
            minor: crate::util::le16(page, 0x36),
            flags: 0,
            reference_size: 0x18,
            roots,
            root_offsets,
        })
    }

    /// Reads ReFS 1.x block `n`.
    fn read_block(&self, n: u64) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; V1_BLOCK as usize];
        let at = n
            .checked_mul(V1_BLOCK)
            .and_then(|b| b.checked_add(self.offset))
            .filter(|_| n < self.boot.volume_size() / V1_BLOCK)
            .ok_or_else(|| format_err!("block {n:#x} outside the volume"))?;
        self.dev.read_exact_at(&mut buf, at)?;
        Ok(buf)
    }

    /// Checkpoint root `i` (ReFS 1.x has 6 roots, 3.x 13 or more).
    pub(crate) fn root(&self, i: usize) -> Result<PageRef> {
        self.checkpoint
            .roots
            .get(i)
            .cloned()
            .ok_or_else(|| format_err!("no checkpoint root {i} (of {})", self.checkpoint.roots.len()))
    }

    /// Where a page's node starts (after its header).
    pub fn node_offset(&self) -> usize {
        if self.v1 { V1_HEADER_SIZE } else { PAGE_HEADER_SIZE }
    }

    /// The node whose descriptor is at `buf[descriptor]`, as this volume
    /// lays nodes out.
    pub fn node_in<'b>(&self, buf: &'b [u8], descriptor: usize) -> Result<Node<'b>> {
        if self.v1 {
            Node::at_v1(buf, descriptor)
        } else {
            Node::at(buf, descriptor)
        }
    }

    /// A page reference in a row of this volume's kind.
    pub fn page_ref(&self, b: &[u8]) -> Result<PageRef> {
        if self.v1 {
            PageRef::parse_v1(b)
        } else {
            PageRef::parse(b)
        }
    }

    fn parse_checkpoint(lcn: u64, clock: u64, page: &[u8]) -> Result<Checkpoint> {
        let reference_size = le32(page, 0x5c) as usize;
        if !matches!(reference_size, 0x30 | 0x48 | 0x68) {
            return Err(Error::Unsupported(format!("page references of {reference_size} bytes")));
        }
        let flags = le32(page, 0x78);
        let count = le32(page, 0x90) as usize;
        if !(13..=32).contains(&count) {
            return Err(format_err!("checkpoint with {count} roots"));
        }
        // Flag 0x200: 0x94 holds the offset of the array of root offsets.
        let array = if flags & 0x200 != 0 {
            le32(page, 0x94) as usize
        } else {
            0x94
        };
        let root_offsets: Vec<usize> = (0..count).map(|i| le32(page, array + 4 * i) as usize).collect();
        let roots = root_offsets
            .iter()
            .enumerate()
            .map(|(i, &at)| {
                page.get(at..at + reference_size)
                    .ok_or_else(|| format_err!("checkpoint root {i} outside the page"))
                    .and_then(PageRef::parse)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Checkpoint {
            lcn,
            clock,
            major: crate::util::le16(page, 0x54),
            minor: crate::util::le16(page, 0x56),
            flags,
            reference_size,
            roots,
            root_offsets,
        })
    }

    /// The log (MLog): the checkpoint's log sequence number and the newest
    /// record's. Windows replays records from the checkpoint's on when it
    /// attaches the volume, so while the newest is not older, the volume
    /// on disk is the checkpoint plus changes only the log holds (`refs`
    /// reads the checkpoint, and must not write: the replay would land on
    /// top of its changes).
    pub fn log_state(&self) -> Result<LogState> {
        if let Some(&state) = self.log.get() {
            return Ok(state);
        }
        let state = self.read_log_state()?;
        Ok(*self.log.get_or_init(|| state))
    }

    fn read_log_state(&self) -> Result<LogState> {
        const PAGE: u64 = 4096;
        if self.v1 {
            return Err(Error::Unsupported("the log of ReFS 1.x".into()));
        }
        let checkpoint = Lsn::from(le64(&self.read_physical(self.checkpoint.lcn, 1)?, 0x70));
        // The control page, where Windows put it so far: among the
        // volume's first clusters (plain volumes), or right after the log
        // region at its usual place, 4 KiB pages 0x24000..0x44000 (a
        // volume inside a space). Neither: the log is not understood.
        let signature = le32(&self.read_physical(SUPERBLOCK_LCN, 1)?, 0x0c);
        let is_control = |c: &[u8]| &c[0..4] == b"MLog" && le32(c, 4) == signature && le64(c, 0x28) == 0;
        let mut control = None;
        for lcn in 0..0x400u64.min(self.boot.volume_size() / self.cluster) {
            let c = self.read_physical(lcn, 1)?;
            if is_control(&c) {
                control = Some(c);
                break;
            }
        }
        if control.is_none() {
            let mut c = vec![0u8; PAGE as usize];
            if self.dev.read_exact_at(&mut c, self.offset + 0x44000 * PAGE).is_ok() && is_control(&c) {
                control = Some(c);
            }
        }
        let control = control.ok_or_else(|| format_err!("no log control page"))?;
        let (epoch, start, end) = (le64(&control, 0x20), le64(&control, 0xb8), le64(&control, 0xc0));
        if start >= end || (end - start) > 1 << 22 {
            return Err(format_err!("log of pages {start:#x}..{end:#x}"));
        }
        // Every record page of the current epoch.
        let mut newest = None;
        let mut head = [0u8; 0x30];
        for page in start..end {
            self.dev.read_exact_at(&mut head, self.offset + page * PAGE)?;
            if &head[0..4] == b"MLog" && le32(&head, 4) == signature && le64(&head, 0x20) == epoch {
                let lsn = Lsn::from(le64(&head, 0x28));
                newest = newest.max(Some(lsn));
            }
        }
        Ok(LogState { checkpoint, newest })
    }

    /// Reads `count` clusters from physical cluster `lcn`.
    pub fn read_physical(&self, lcn: u64, count: u64) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; (count * self.cluster) as usize];
        let at = lcn
            .checked_mul(self.cluster)
            .and_then(|b| b.checked_add(self.offset))
            .ok_or_else(|| format_err!("cluster {lcn:#x} beyond any device"))?;
        self.dev.read_exact_at(&mut buf, at)?;
        Ok(buf)
    }

    /// The virtual cluster that names physical cluster `lcn`.
    pub fn virtual_of(&self, lcn: u64) -> Result<u64> {
        let cpc = self.clusters_per_container;
        let shift = 64 - cpc.leading_zeros();
        self.containers
            .iter()
            .find(|&(_, &start)| start <= lcn && lcn < start + cpc)
            .map(|(&cid, &start)| (cid << shift) | (lcn - start))
            .ok_or_else(|| format_err!("cluster {lcn:#x} in no container"))
    }

    /// The physical cluster of virtual cluster `vlcn`.
    pub fn translate(&self, vlcn: u64) -> Result<u64> {
        let cpc = self.clusters_per_container;
        let shift = 64 - cpc.leading_zeros();
        let start = self
            .containers
            .get(&(vlcn >> shift))
            .ok_or_else(|| format_err!("virtual cluster {vlcn:#x} in no container"))?;
        Ok(start + (vlcn & (cpc - 1)))
    }

    /// Reads the page a reference points at (virtual clusters unless
    /// `physical`), checking its checksum and that it is a B+-tree page.
    pub fn read_page(&self, r: &PageRef, physical: bool) -> Result<Vec<u8>> {
        if self.v1 {
            let page = self.read_block(r.lcns[0])?;
            if !r.verifies(&page) {
                return Err(format_err!("page at {:#x} fails its checksum", r.lcns[0]));
            }
            if le64(&page, 0) != r.lcns[0] {
                return Err(format_err!("block {:#x} is no page of its own", r.lcns[0]));
            }
            return Ok(page);
        }
        let per_page = (self.page_size / self.cluster) as usize;
        let mut page = Vec::with_capacity(self.page_size as usize);
        for &lcn in &r.lcns[..per_page] {
            let lcn = if physical { lcn } else { self.translate(lcn)? };
            page.extend(self.read_physical(lcn, 1)?);
        }
        if !r.verifies(&page) {
            return Err(format_err!("page at {:#x} fails its checksum", r.lcns[0]));
        }
        let header = PageHeader::parse(&page)?;
        if &header.signature != b"MSB+" {
            return Err(format_err!("page at {:#x} is no B+-tree page", r.lcns[0]));
        }
        Ok(page)
    }

    /// Calls `f` with every leaf row of the tree whose root `root` points
    /// at, in key order.
    pub fn walk(&self, root: &PageRef, physical: bool, f: &mut dyn FnMut(Row<'_>) -> Result<()>) -> Result<()> {
        self.walk_while(root, physical, &mut |row| f(row).map(|()| true))
    }

    /// Like `walk`, until `f` returns false.
    pub fn walk_while(&self, root: &PageRef, physical: bool, f: &mut dyn FnMut(Row<'_>) -> Result<bool>) -> Result<()> {
        self.level(root, physical, 0, f).map(drop)
    }

    /// Like `walk`, from a node already at hand (one embedded in a value).
    pub fn walk_node(
        &self,
        node: &Node<'_>,
        physical: bool,
        depth: usize,
        f: &mut dyn FnMut(Row<'_>) -> Result<()>,
    ) -> Result<()> {
        self.node(node, physical, depth, &mut |row| f(row).map(|()| true))
            .map(drop)
    }

    /// Walks the tree below `r`; false when `f` stopped the walk.
    fn level(
        &self,
        r: &PageRef,
        physical: bool,
        depth: usize,
        f: &mut dyn FnMut(Row<'_>) -> Result<bool>,
    ) -> Result<bool> {
        if depth > MAX_DEPTH {
            return Err(format_err!("B+-tree deeper than {MAX_DEPTH} levels"));
        }
        let page = self.read_page(r, physical)?;
        self.node(&self.node_in(&page, self.node_offset())?, physical, depth, f)
    }

    fn node(
        &self,
        node: &Node<'_>,
        physical: bool,
        depth: usize,
        f: &mut dyn FnMut(Row<'_>) -> Result<bool>,
    ) -> Result<bool> {
        for row in node.rows() {
            let row = row?;
            let more = if node.is_leaf() {
                f(row)?
            } else {
                self.level(&self.page_ref(row.value)?, physical, depth + 1, f)?
            };
            if !more {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn load_containers(&mut self) -> Result<()> {
        let mut containers = BTreeMap::new();
        let mut classes = BTreeMap::new();
        let mut compacted = BTreeMap::new();
        let mut cpc = 0;
        let mut last = Err(format_err!("no container table"));
        for root in [ROOT_CONTAINERS, ROOT_CONTAINERS_COPY] {
            let r = self.root(root)?;
            containers.clear();
            classes.clear();
            compacted.clear();
            last = self.walk(&r, true, &mut |row| {
                let v = row.value;
                if row.key.len() < 8 || v.len() < 0x30 {
                    return Err(format_err!("container table row of {} bytes", v.len()));
                }
                cpc = le32(v, 0x18) as u64;
                // A compacted container names its compressed bytes by a
                // virtual cluster instead (see `compress`).
                if le32(v, 0x14) == crate::compress::COMPACTED {
                    compacted.insert(le64(row.key, 0), crate::compress::Compacted::from_row(v)?);
                    return Ok(());
                }
                containers.insert(le64(row.key, 0), le64(v, v.len() - 16));
                classes.insert(le64(v, v.len() - 16), le32(v, 0x14));
                Ok(())
            });
            if last.is_ok() {
                break;
            }
        }
        last?;
        // The boot sector names the containers' size from 3.4 on (0 on 3.1).
        let named = self.boot.container_size;
        if !cpc.is_power_of_two() || (named != 0 && cpc.checked_mul(self.cluster) != Some(named)) {
            return Err(format_err!("{cpc} clusters per container"));
        }
        self.clusters_per_container = cpc;
        self.containers = containers;
        self.container_classes = classes;
        // Their streams' ranges and kept clusters: root 10.
        if !compacted.is_empty() {
            let r = self.root(ROOT_CONTAINER_INDEX)?;
            self.walk(&r, false, &mut |row| {
                if row.key.len() >= 16
                    && let Some(c) = compacted.get_mut(&le64(row.key, 0))
                {
                    c.add_row(le32(row.key, 12), row.value, cpc)?;
                }
                Ok(())
            })?;
        }
        self.compacted = compacted;
        self.units.lock().unwrap().clear();
        Ok(())
    }

    /// The compacted (compressed) containers: id, the virtual cluster of
    /// their compressed bytes and its clusters.
    pub fn compressed_runs(&self) -> Vec<(u64, u64, u64)> {
        self.compacted.iter().map(|(&id, c)| (id, c.data, c.clusters)).collect()
    }

    /// Whether virtual cluster `vlcn` is in a compacted container (its
    /// data is read from the container's compressed units, see
    /// `compressed_runs`).
    pub fn is_compacted(&self, vlcn: u64) -> bool {
        let shift = 64 - self.clusters_per_container.leading_zeros();
        self.compacted.contains_key(&(vlcn >> shift))
    }

    /// Reads from a compacted container (see `compress`): each cluster
    /// from its unit of the stream, zeros for a cluster it did not keep.
    fn read_compacted(&self, id: u64, first: u64, skip: u64, buf: &mut [u8]) -> Result<()> {
        let c = &self.compacted[&id];
        let cluster = self.cluster;
        let mut pos = first * cluster + skip;
        let mut done = 0;
        while done < buf.len() {
            let within = pos % cluster;
            let take = ((cluster - within) as usize).min(buf.len() - done);
            let out = &mut buf[done..done + take];
            match c.place(pos / cluster, cluster) {
                None => out.fill(0),
                Some(at) => {
                    let at = at + within;
                    let (r, u, unit_start) = c.unit_of(at)?;
                    let data = self.unit(id, r, u)?;
                    let o = (at - unit_start) as usize;
                    out.copy_from_slice(
                        data.get(o..o + take)
                            .ok_or_else(|| format_err!("a cluster across compressed units"))?,
                    );
                }
            }
            done += take;
            pos += take as u64;
        }
        Ok(())
    }

    /// Unit `u` of range `r` of compacted container `id`, decompressed
    /// (the last few are kept).
    fn unit(&self, id: u64, r: usize, u: usize) -> Result<std::sync::Arc<Vec<u8>>> {
        let key = (id, r, u);
        if let Some((_, d)) = self.units.lock().unwrap().iter().find(|(k, _)| *k == key) {
            return Ok(d.clone());
        }
        let c = &self.compacted[&id];
        let (start, end, size) = c.unit_bounds(r, u)?;
        let cluster = self.cluster;
        if end > c.clusters.saturating_mul(cluster) {
            return Err(format_err!(
                "a compressed unit past the container's compressed clusters"
            ));
        }
        let mut packed = vec![0u8; (end - start) as usize];
        let mut at = start;
        let mut done = 0;
        // Its bytes may cross the containers the compressed data spans.
        while done < packed.len() {
            let vlcn = c
                .data
                .checked_add(at / cluster)
                .ok_or_else(|| format_err!("compressed bytes beyond any cluster"))?;
            let left =
                (self.clusters_per_container - (vlcn & (self.clusters_per_container - 1))) * cluster - at % cluster;
            let n = (left as usize).min(packed.len() - done);
            if self.is_compacted(vlcn) {
                return Err(format_err!("compressed bytes in a compacted container"));
            }
            self.read_virtual(vlcn, at % cluster, &mut packed[done..done + n])?;
            done += n;
            at += n as u64;
        }
        let data = std::sync::Arc::new(c.decode(r, u, &packed, size)?);
        let mut units = self.units.lock().unwrap();
        if units.len() >= UNITS_KEPT {
            units.remove(0);
        }
        units.push((key, data.clone()));
        Ok(data)
    }

    fn load_objects(&mut self) -> Result<()> {
        let r = self.root(ROOT_OBJECTS)?;
        let reference_size = self.checkpoint.reference_size;
        // The reference: at 0x20 of the value (at 0 on ReFS 1.x).
        let at = if self.v1 { 0 } else { 0x20 };
        let mut objects = BTreeMap::new();
        self.walk(&r, false, &mut |row| {
            if row.key.len() < 16 {
                return Err(format_err!("object table key of {} bytes", row.key.len()));
            }
            let reference = row
                .value
                .get(at..at + reference_size)
                .ok_or_else(|| format_err!("object table row of {} bytes", row.value.len()))?;
            objects.insert(le64(row.key, 8), self.page_ref(reference)?);
            Ok(())
        })?;
        self.objects = objects;
        Ok(())
    }

    /// The root of an object's tree.
    pub fn object(&self, oid: u64) -> Result<&PageRef> {
        self.objects
            .get(&oid)
            .ok_or_else(|| Error::NotFound(format!("object {oid:#x}")))
    }

    pub fn object_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.objects.keys().copied()
    }

    /// Every leaf row of an object's tree, in key order.
    /// The clusters free for data (the medium allocator's bitmap rows'
    /// free counts and its free uniform ranges).
    pub fn free_clusters(&self) -> Result<u64> {
        let mut free = 0u64;
        self.walk(&self.root(1)?, false, &mut |row| {
            let v = row.value;
            if v.len() >= 0x18 {
                match (le16(v, 0x12), le16(v, 0x10)) {
                    (1, n) => free += u64::from(n),
                    (2, 0xffff) => free += le64(v, 8),
                    _ => {}
                }
            }
            Ok(())
        })?;
        Ok(free)
    }

    pub fn object_rows(&self, oid: u64) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let root = self.object(oid)?.clone();
        let mut rows = Vec::new();
        self.walk(&root, false, &mut |row| {
            rows.push((row.key.to_vec(), row.value.to_vec()));
            Ok(())
        })?;
        Ok(rows)
    }

    /// Reads bytes from `skip` bytes into virtual cluster `vlcn` on. A run
    /// of virtual clusters never leaves its container (in virtual numbers
    /// containers are twice their size apart), so neither may a read.
    pub fn read_virtual(&self, vlcn: u64, skip: u64, buf: &mut [u8]) -> Result<()> {
        let cpc = self.clusters_per_container;
        let left = (cpc - (vlcn & (cpc - 1))) * self.cluster;
        if skip.checked_add(buf.len() as u64).is_none_or(|end| end > left) {
            return Err(format_err!(
                "a read from virtual cluster {vlcn:#x} leaves its container"
            ));
        }
        let shift = 64 - cpc.leading_zeros();
        if self.compacted.contains_key(&(vlcn >> shift)) {
            return self.read_compacted(vlcn >> shift, vlcn & (cpc - 1), skip, buf);
        }
        let at = self
            .translate(vlcn)?
            .checked_mul(self.cluster)
            .and_then(|b| b.checked_add(self.offset + skip))
            .ok_or_else(|| format_err!("cluster {vlcn:#x} beyond any device"))?;
        self.dev.read_exact_at(buf, at)?;
        Ok(())
    }
}

/// A log sequence number: (wrap, sequence) as ReFS stores it (u32 low
/// part first), ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Lsn {
    pub high: u32,
    pub low: u32,
}

impl From<u64> for Lsn {
    fn from(v: u64) -> Self {
        Lsn {
            high: (v >> 32) as u32,
            low: v as u32,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LogState {
    /// Where the checkpoint's view of the volume ends in the log.
    pub checkpoint: Lsn,
    /// The newest record in the log.
    pub newest: Option<Lsn>,
}

impl LogState {
    /// Records Windows would replay over the checkpoint.
    pub fn needs_replay(&self) -> bool {
        self.newest.is_some_and(|n| n >= self.checkpoint)
    }
}
