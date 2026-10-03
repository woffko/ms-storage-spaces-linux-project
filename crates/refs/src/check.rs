//! Checking a volume's consistency (`refs check`): what the write tests
//! check after every change, for any volume.
//!
//! * Pages: every page the checkpoint and the object table reach parses
//!   the way Windows checks pages (its checksum on reading; the row area
//!   row by row up to its end, every key index entry naming a live row,
//!   the free bytes the area less the live rows, the last row of an index
//!   node and only it with row flag 2), and its clusters are used in the
//!   allocator that hands them out.
//! * Files: every file's clusters (all levels of its streams, named
//!   streams, snapshots, and extent maps kept in pages) are used in the
//!   medium allocator, and clusters several files map are counted as
//!   shared in the block reference count table.
//! * The pages only the older checkpoint references and that are still
//!   allocated are counted (Windows frees them with its next checkpoint).
//! * Compacted (compressed) containers: their compressed clusters are used
//!   in the medium allocator (the files' runs there are not looked up).

use std::collections::BTreeMap;

use storage_spaces::io::ReadAt;

use crate::error::Result;
use crate::file::{Content, Target};
use crate::node::Node;
use crate::page::{PAGE_HEADER_SIZE, PageRef};
use crate::refcount;
use crate::util::{le16, le32, le64};
use crate::volume::{ROOT_DIRECTORY, Volume};

/// Problems listed in a report at most (the count goes on).
const LISTED: usize = 100;

#[derive(Debug, Default)]
pub struct Report {
    pub pages: u64,
    pub directories: u64,
    pub files: u64,
    pub data_clusters: u64,
    /// Clusters of pages only the older checkpoint references, still
    /// allocated (not a problem: Windows frees them with its next
    /// checkpoint, `refs` with its next commit; `refsutil leak` counts
    /// them as leaked meanwhile).
    pub deferred_clusters: u64,
    pub problems: Vec<String>,
    /// All problems, also those beyond the listed ones.
    pub problem_count: u64,
}

impl Report {
    fn problem(&mut self, p: String) {
        self.problem_count += 1;
        if self.problems.len() < LISTED {
            self.problems.push(p);
        }
    }
}

/// An allocator row's clusters: a bitmap, or a uniform range (wholly used
/// or not).
enum Bits {
    Map(Vec<u8>),
    Uniform(bool),
}

/// An allocator's rows: start, count, clusters.
pub(crate) struct Allocator(Vec<(u64, u64, Bits)>);

impl Allocator {
    /// Allocator `root`'s rows (the small allocator, 12, is kept at
    /// physical clusters).
    pub(crate) fn read<D: ReadAt>(vol: &Volume<D>, root: usize) -> Result<Self> {
        let mut rows = Vec::new();
        vol.walk(&vol.checkpoint.roots[root].clone(), root == 12, &mut |row| {
            let v = row.value;
            if v.len() >= 0x18 {
                let (start, count) = (le64(v, 0), le64(v, 8));
                let bits = match le16(v, 0x12) {
                    1 | 5 | 9 => Bits::Map(v[0x18..].to_vec()),
                    // A uniform range: no free clusters when wholly used
                    // (0xffff when wholly free).
                    _ => Bits::Uniform(le16(v, 0x10) == 0),
                };
                rows.push((start, count, bits));
            }
            Ok(())
        })?;
        rows.sort_by_key(|r| r.0);
        Ok(Allocator(rows))
    }

    /// Whether cluster `c` is used.
    pub(crate) fn used(&self, c: u64) -> bool {
        let i = self.0.partition_point(|r| r.0 <= c);
        let Some((start, count, bits)) = i.checked_sub(1).map(|i| &self.0[i]) else {
            return false;
        };
        if c >= start + count {
            return false;
        }
        match bits {
            Bits::Map(b) => {
                let j = (c - start) as usize;
                b.get(j / 8).is_some_and(|byte| byte >> (j % 8) & 1 != 0)
            }
            Bits::Uniform(used) => *used,
        }
    }
}

/// The page structure check: problems found in one page.
fn page_problems(page: &[u8]) -> Option<String> {
    let u32_at = |o: usize| {
        page.get(o..o + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
    };
    let h = PAGE_HEADER_SIZE + u32_at(PAGE_HEADER_SIZE)?;
    let (start, end, free, index, count) = (
        u32_at(h)?,
        u32_at(h + 4)?,
        u32_at(h + 8)?,
        u32_at(h + 0x10)?,
        u32_at(h + 0x14)?,
    );
    let mut rows = BTreeMap::new();
    let mut o = start;
    while o < end {
        let size = u32_at(h + o)?;
        if size < 0x10 || !size.is_multiple_of(8) || o + size > end {
            return Some(format!("row at {o:#x} of {size:#x} bytes"));
        }
        rows.insert(o, (size, le16(page, h + o + 8)));
        o += size;
    }
    if o != end {
        return Some(format!("rows end at {o:#x}, the header says {end:#x}"));
    }
    let index_node = page.get(h + 0x0c).is_some_and(|&l| l > 0);
    // Node flag 8: the entries' high halves carry their keys' first u64
    // less the base at 0x18 (where it fits).
    let deltas = page.get(h + 0x0d).is_some_and(|&f| f & 8 != 0);
    let base = page
        .get(h + 0x18..h + 0x20)
        .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()));
    let mut live = 0;
    for i in 0..count {
        let entry = u32_at(h + index + 4 * i)?;
        let e = entry & 0xffff;
        let Some(&(size, flags)) = rows.get(&e) else {
            return Some(format!("key index entry {i} names no row ({e:#x})"));
        };
        let (ko, kl) = (le16(page, h + e + 4) as usize, le16(page, h + e + 6) as usize);
        let key0 = page
            .get(h + e + ko..h + e + ko + 8)
            .filter(|_| kl >= 8)
            .map(|k| u64::from_le_bytes(k.try_into().unwrap()));
        if deltas
            && let Some(k) = key0
            && k >= base
            && k - base < 0xffff
            && (entry >> 16) as u64 != k - base
        {
            return Some(format!(
                "key index entry {i} carries {:#x}, its key less the base {:#x}",
                entry >> 16,
                k - base
            ));
        }
        if flags & 4 != 0 {
            return Some(format!("key index entry {i} names a removed row"));
        }
        if index_node && (flags & 2 != 0) != (i + 1 == count) {
            return Some(format!("index row {i} of {count} with row flags {flags:#x}"));
        }
        live += size;
    }
    if index.checked_sub(start + live) != Some(free) {
        return Some(format!(
            "{free:#x} free bytes, the rows leave {:#x}",
            index - start - live
        ));
    }
    None
}

impl<D: ReadAt> Volume<D> {
    /// Checks the volume (see the module); `skip` names objects not to
    /// look into (fixtures that leave some out).
    pub fn check(&self, skip: &[u64]) -> Result<Report> {
        let mut report = Report::default();
        match self.deferred_pages() {
            Ok(d) => report.deferred_clusters = d.iter().map(|(_, l)| l.len() as u64).sum(),
            Err(e) => report.problem(format!("the older checkpoint's pages: {e}")),
        }
        let medium = Allocator::read(self, 1)?;
        let container = Allocator::read(self, 2)?;
        let per_page = (self.page_size / self.cluster) as usize;
        // Pages, by table: (reference, physical, allocator, what).
        let mut todo: Vec<(PageRef, bool, Option<u8>, String)> = Vec::new();
        for (i, r) in self.checkpoint.roots.iter().enumerate() {
            let (physical, allocator) = match i {
                7 | 8 | 12 => (true, None),
                1 | 2 | 6 | 11 => (false, Some(2)),
                _ => (false, Some(1)),
            };
            todo.push((r.clone(), physical, allocator, format!("root {i}")));
        }
        for oid in self.object_ids().collect::<Vec<_>>() {
            // Objects 7 and 8 name the container tables' physical clusters.
            if oid != 7 && oid != 8 && !skip.contains(&oid) {
                todo.push((self.object(oid)?.clone(), false, Some(1), format!("object {oid:#x}")));
            }
        }
        while let Some((r, physical, allocator, what)) = todo.pop() {
            report.pages += 1;
            let page = match self.read_page(&r, physical) {
                Ok(p) => p,
                Err(e) => {
                    report.problem(format!("{what}: {e}"));
                    continue;
                }
            };
            for &l in &r.lcns[..per_page] {
                let lcn = if physical { Ok(l) } else { self.translate(l) };
                match (lcn, allocator) {
                    (Ok(lcn), Some(1)) if !medium.used(lcn) => {
                        report.problem(format!("{what}: page cluster {lcn:#x} free in the medium allocator"))
                    }
                    (Ok(lcn), Some(2)) if !container.used(lcn) => {
                        report.problem(format!("{what}: page cluster {lcn:#x} free in the container allocator"))
                    }
                    (Err(e), _) => report.problem(format!("{what}: {e}")),
                    _ => {}
                }
            }
            if let Some(p) = page_problems(&page) {
                report.problem(format!("{what}: page {:#x}: {p}", r.lcns[0]));
                continue;
            }
            let node = Node::at(&page, PAGE_HEADER_SIZE)?;
            if !node.is_leaf() {
                for row in node.rows() {
                    match row.and_then(|row| PageRef::parse(row.value)) {
                        Ok(child) => todo.push((child, physical, allocator, what.clone())),
                        Err(e) => report.problem(format!("{what}: {e}")),
                    }
                }
            }
        }
        // Files: their clusters, and who else maps them.
        let mut runs: Vec<(u64, u64, String)> = Vec::new();
        let mut records = std::collections::HashSet::new();
        let mut dirs = vec![(ROOT_DIRECTORY, String::new())];
        while let Some((oid, path)) = dirs.pop() {
            report.directories += 1;
            let entries = match self.read_dir(oid) {
                Ok(e) => e,
                Err(e) => {
                    report.problem(format!("{path}/: {e}"));
                    continue;
                }
            };
            for e in entries {
                let name = format!("{path}/{}", e.name);
                if let Target::Directory(child) = e.target {
                    if e.attributes & 0x400 == 0 && !skip.contains(&child) {
                        dirs.push((child, name));
                    }
                    continue;
                }
                // The names of a moved or linked file share its record.
                if let Target::Split { home, ordinal } = e.target
                    && !records.insert((home, ordinal))
                {
                    continue;
                }
                report.files += 1;
                let file = match self.open_file(&e) {
                    Ok(f) => f,
                    Err(err) => {
                        report.problem(format!("{name}: {err}"));
                        continue;
                    }
                };
                let streams = file
                    .data
                    .iter()
                    .chain(file.streams.iter().chain(&file.snapshots).map(|(_, s)| s));
                for s in streams {
                    if let Content::Extents(x) = &s.content {
                        for x in x.iter().filter(|x| x.written) {
                            // In a compacted container: its compressed
                            // clusters stand for them (checked below).
                            if self.is_compacted(x.vlcn) {
                                report.data_clusters += x.clusters;
                                continue;
                            }
                            match self.translate(x.vlcn) {
                                Ok(lcn) => runs.push((lcn, x.clusters, name.clone())),
                                Err(err) => report.problem(format!("{name}: {err}")),
                            }
                        }
                    }
                }
                if let Ok(record) = self.record(&e) {
                    for page in self.map_pages_of_record(&record) {
                        runs.push((page, 1, format!("{name} (extent map)")));
                    }
                }
            }
        }
        // The compressed clusters of compacted containers.
        for (id, c) in &self.compacted {
            for k in 0..c.clusters {
                match self.translate(c.data + k) {
                    Ok(lcn) if !medium.used(lcn) => {
                        report.problem(format!(
                            "compacted container {id:#x}: compressed cluster {lcn:#x} free in the medium allocator"
                        ));
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        report.problem(format!("compacted container {id:#x}: {e}"));
                        break;
                    }
                }
            }
        }
        // Snapshots and the live data share the clusters the newer levels
        // did not replace: a file's runs are taken once.
        runs.sort();
        runs.dedup();
        let (refcounts, row_problems) = self.refcount_rows()?;
        for p in row_problems {
            report.problem(p);
        }
        let shared = |c: u64| -> bool {
            let Ok(v) = self.virtual_of(c) else { return false };
            refcounts
                .iter()
                .find(|(first, count, _)| *first <= v && v < first + count)
                .is_some_and(|(_, _, value)| refcount::count_of(value, v).is_some_and(|c| c > 0))
        };
        let mut last: Option<(u64, String)> = None;
        for (lcn, n, name) in &runs {
            report.data_clusters += n;
            for c in *lcn..lcn + n {
                if !medium.used(c) {
                    report.problem(format!("{name}: data cluster {c:#x} free in the medium allocator"));
                    break;
                }
            }
            if let Some((end, other)) = &last
                && *lcn < *end
                && other != name
            {
                let c = *lcn;
                if !shared(c) {
                    report.problem(format!(
                        "{name}: cluster {c:#x} also mapped by {other}, without a reference count"
                    ));
                }
            }
            if last.as_ref().is_none_or(|(end, _)| lcn + n > *end) {
                last = Some((lcn + n, name.clone()));
            }
        }
        Ok(report)
    }

    /// The pages of extent maps a record keeps outside it (its levels'
    /// values whose node is an index over pages).
    fn map_pages_of_record(&self, record: &[u8]) -> Vec<u64> {
        let mut out = Vec::new();
        let Ok(node) = Node::at(record, 0) else {
            return out;
        };
        for row in node.rows().flatten() {
            let k = row.key;
            let level = k.len() >= 0x18
                && le32(k, 8) == 0x8000_0002
                && le32(k, 12) & 0xffff == 0x80
                && le64(k, 0x10) >= crate::file::LIVE_STREAM;
            let set = k.len() >= 0x40 && le32(k, 8) == 3 && le64(k, 0x38) >= crate::file::LIVE_STREAM;
            if (level || set)
                && let Ok(pages) = self.extent_map_pages(row.value)
            {
                out.extend(pages);
            }
        }
        out
    }

    /// The block reference count table's rows (see `refcount`): their
    /// ranges and values, and the problems found in them (a row of counts
    /// too short for its range, an unknown kind).
    #[allow(clippy::type_complexity)]
    fn refcount_rows(&self) -> Result<(Vec<(u64, u64, Vec<u8>)>, Vec<String>)> {
        let (mut rows, mut problems) = (Vec::new(), Vec::new());
        self.walk(&self.checkpoint.roots[6].clone(), false, &mut |row| {
            let Some((first, n)) = refcount::range(row.value) else {
                problems.push(format!("reference count row of {} bytes", row.value.len()));
                return Ok(());
            };
            match le32(row.value, 0x14) {
                refcount::COUNTS if row.value.len() < 0x1c + 2 * n as usize => problems.push(format!(
                    "reference count row {first:#x}: {n:#x} clusters, {} bytes",
                    row.value.len()
                )),
                refcount::COUNTS | refcount::UNIFORM => {}
                kind => problems.push(format!("reference count row {first:#x} of kind {kind}")),
            }
            rows.push((first, n, row.value.to_vec()));
            Ok(())
        })?;
        Ok((rows, problems))
    }
}
