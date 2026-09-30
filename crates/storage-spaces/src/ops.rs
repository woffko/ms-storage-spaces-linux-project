//! Management operations as plans ([`crate::plan::Plan`]), with the choices
//! Linux makes itself: defaults as Windows 11 24H2 chooses them, which disks
//! and slabs, ids and numbers. The byte layout of every change follows the
//! models of [`crate::create`] and [`crate::manage`], which are checked
//! byte for byte against what Windows writes.

use std::collections::BTreeMap;

use crate::Pool;
use crate::create::{DATABASE_COPIES, Hidden, NewDisk, NewHidden, NewPool, NewSpace, next_space_number};
use crate::database::Database;
use crate::error::{Error, Result};
use crate::format::{ExtentRecord, POOL_DB_OFFSET, Record, SLAB_SIZE, assemble_records};
use crate::guid::Guid;
use crate::io::ReadAt;
use crate::plan::{Action, Plan, Target};
use crate::records::{DiskBody, PoolBody, SpaceBody};

/// A source of new GUIDs (random ones do: Windows reads any).
pub type Guids<'a> = &'a mut dyn FnMut() -> Guid;

/// Refuses pools a management operation must not touch, and returns the
/// pool database: every member present and in agreement (no warnings at
/// all), every record one [`crate::records`] reproduces byte for byte, and
/// every space healthy.
pub fn check_pool<D: ReadAt>(pool: &Pool<D>) -> Result<Database> {
    if !pool.warnings.is_empty() {
        return Err(Error::Pool(format!(
            "the pool is not in a clean state ({}); attach it to Windows first",
            pool.warnings.join("; ")
        )));
    }
    let db = pool.database_model()?;
    for r in assemble_records(db.bytes(), 0x40)? {
        let understood = match r.kind {
            1 => PoolBody::decode(r.version, &r.body).map(|_| ()),
            2 => DiskBody::decode(&r.body).map(|_| ()),
            3 | 6 => SpaceBody::decode(r.kind == 6, &r.body).map(|_| ()),
            4 => Record::decode(&r).map(|_| ()),
            k => Err(crate::error::format_err!("record type {k}")),
        };
        understood.map_err(|e| Error::Pool(format!("record {} is not understood: {e}", r.id)))?;
    }
    for space in pool.user_spaces() {
        let reader = pool.open_space(space.id())?;
        if reader.condition() != crate::Condition::Healthy {
            return Err(Error::Pool(format!(
                "space \"{}\" is {:?}; repair it first",
                space.name(),
                reader.condition()
            )));
        }
    }
    Ok(db)
}

/// The FILETIME of now.
pub fn filetime_now() -> u64 {
    let since_1970 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (since_1970.as_nanos() / 100) as u64 + 11_644_473_600 * 10_000_000
}

/// Used and total physical slabs of the pool's disks, for placing extents.
#[derive(Debug, Clone)]
pub struct Slabs {
    /// Disk id -> (total slabs, used ranges (start, count)).
    disks: BTreeMap<u64, (u64, Vec<(u64, u64)>)>,
}

impl Slabs {
    /// From the database: the disks eligible for new extents (usage
    /// Auto-Select) and every extent.
    pub fn of(db: &Database) -> Result<Self> {
        let mut disks = BTreeMap::new();
        let mut used: Vec<(u64, u64, u64)> = Vec::new();
        for r in assemble_records(db.bytes(), 0x40)? {
            match r.kind {
                2 => {
                    let d = DiskBody::decode(&r.body)?;
                    if d.usage == 1 {
                        disks.insert(d.id, (d.data_size / SLAB_SIZE, Vec::new()));
                    }
                }
                4 => {
                    if let Ok(Record::Extent(e)) = Record::decode(&r) {
                        used.push((e.disk_id, e.physical_slab, e.slab_count));
                    }
                }
                _ => {}
            }
        }
        for (disk, start, count) in used {
            if let Some((_, ranges)) = disks.get_mut(&disk) {
                ranges.push((start, count));
            }
        }
        for (_, ranges) in disks.values_mut() {
            ranges.sort_unstable();
        }
        Ok(Slabs { disks })
    }

    pub fn disks(&self) -> usize {
        self.disks.len()
    }

    fn free(&self, disk: u64) -> u64 {
        let (total, ranges) = &self.disks[&disk];
        total - ranges.iter().map(|r| r.1).sum::<u64>()
    }

    /// The first run of `n` free slabs of `disk` (Windows' rule).
    fn first_run(&self, disk: u64, n: u64) -> Option<u64> {
        let (total, ranges) = &self.disks[&disk];
        let mut at = 0;
        for &(start, count) in ranges {
            if start >= at + n {
                break;
            }
            at = at.max(start + count);
        }
        (at + n <= *total).then_some(at)
    }

    /// `n` slabs on the disk with the most free slabs (the lowest id on a
    /// tie) that is not in `exclude` and has a run of them.
    pub fn allocate(&mut self, n: u64, exclude: &[u64]) -> Option<(u64, u64)> {
        let disk = self
            .disks
            .keys()
            .filter(|d| !exclude.contains(d))
            .filter(|&&d| self.first_run(d, n).is_some())
            .max_by_key(|&&d| (self.free(d), std::cmp::Reverse(d)))
            .copied()?;
        let start = self.first_run(disk, n)?;
        let ranges = &mut self.disks.get_mut(&disk).unwrap().1;
        ranges.push((start, n));
        ranges.sort_unstable();
        Some((disk, start))
    }
}

/// A blank disk for a new pool.
#[derive(Debug, Clone)]
pub struct BlankDisk {
    pub size: u64,
    pub logical_sector: u64,
    pub physical_sector: u64,
    pub manufacturer: String,
    pub model: String,
}

/// `spaces pool create`: a pool of `disks` named `name`, with the given
/// logical sector size (default: the largest of the disks'). The first five
/// disks carry the pool database. Each disk gets its header and database,
/// then its partition table, so that a disk shows up as a member only once
/// it is complete.
pub fn plan_create_pool(
    disks: &[BlankDisk],
    name: &str,
    logical_sector: Option<u32>,
    guids: Guids,
) -> Result<(Plan, NewPool)> {
    if disks.is_empty() {
        return Err(Error::Pool("a pool needs at least one disk".into()));
    }
    if name.is_empty() || name.encode_utf16().count() > 256 {
        return Err(Error::Pool("the pool needs a name of 1 to 256 characters".into()));
    }
    let widest = disks.iter().map(|d| d.logical_sector).max().unwrap() as u32;
    let logical = logical_sector.unwrap_or(widest);
    if logical < widest || !(logical == 512 || logical == 4096) {
        return Err(Error::Pool(format!(
            "logical sector size {logical}: 512 or 4096 bytes, at least the disks' ({widest})"
        )));
    }
    let now = filetime_now();
    let new = NewPool {
        name: name.to_string(),
        guid: guids(),
        logical_sector: logical,
        physical_sector: disks
            .iter()
            .map(|d| d.physical_sector)
            .max()
            .unwrap()
            .max(logical as u64) as u32,
        metadata_guid: guids(),
        created: now,
        disks: disks
            .iter()
            .enumerate()
            .map(|(i, d)| NewDisk {
                size: d.size,
                sector: d.logical_sector,
                guid: guids(),
                gpt_disk_guid: guids(),
                msr_guid: guids(),
                partition_guid: guids(),
                joined: now,
                manufacturer: d.manufacturer.clone(),
                model: d.model.clone(),
                database_copy: i < DATABASE_COPIES,
            })
            .collect(),
    };
    let writes = new.writes()?;
    let mut plan = Plan {
        summary: vec![format!(
            "create pool \"{}\" ({}) of {} disks, {} byte logical sectors",
            new.name,
            new.guid,
            disks.len(),
            logical
        )],
        steps: Vec::new(),
    };
    // The header and database pages first, the partition table last.
    for (i, w) in writes.iter().enumerate() {
        let (table, member): (Vec<_>, Vec<_>) = w.iter().cloned().partition(|(o, _)| *o == 0 || *o > (16 << 20));
        let to = |v: Vec<(u64, Vec<u8>)>| {
            v.into_iter()
                .map(|(offset, bytes)| Action::Write {
                    target: Target::New(i),
                    offset,
                    bytes,
                })
                .collect()
        };
        plan.step(format!("disk {i}: header and pool database"), to(member));
        plan.step(format!("disk {i}: partition table"), to(table));
    }
    Ok((plan, new))
}

/// What to create (`spaces space create`).
#[derive(Debug, Clone)]
pub struct SpaceSpec {
    pub name: String,
    /// 1 simple, 2 mirror, 3 parity (single).
    pub resiliency: u8,
    /// Requested size; rounded up to whole rows.
    pub size: u64,
    pub thin: bool,
    /// Mirror: 2 (default) or 3.
    pub copies: Option<u64>,
    pub columns: Option<u64>,
    /// Bytes: a power of two from 16 KiB to 16 MiB (default 256 KiB).
    pub interleave: Option<u64>,
    /// Parity: the write-back cache (default 1 GiB, at least 512 MiB).
    pub write_cache: Option<u64>,
}

/// The ids and numbers new records take: after every id in use, and the
/// smallest free numbers.
fn next_ids(db: &Database) -> Result<u64> {
    let mut max = 0;
    for r in assemble_records(db.bytes(), 0x40)? {
        let id = match r.kind {
            2 => DiskBody::decode(&r.body)?.id,
            3 | 6 => SpaceBody::decode(r.kind == 6, &r.body)?.id,
            _ => 0,
        };
        max = max.max(id);
    }
    Ok(max + 1)
}

/// The databases of the metadata space are kept on at most five of its
/// copies: all in pools of up to five disks, else those of the disks that
/// carry the pool database.
fn metadata_disks<D: ReadAt>(pool: &Pool<D>) -> Option<Vec<u64>> {
    if pool.members.len() <= DATABASE_COPIES {
        return None;
    }
    Some(
        pool.disks
            .values()
            .filter(|d| d.member.is_some_and(|m| pool.members[m].header.database_copy))
            .map(|d| d.id)
            .collect(),
    )
}

/// Turns the writes of a pool database update into steps: one per member
/// carrying a copy, each flushed before the next (a crash leaves every copy
/// whole, old or new).
fn database_steps(plan: &mut Plan, writes: Vec<(usize, u64, Vec<u8>)>) {
    for (device, offset, bytes) in writes {
        plan.step(
            format!("pool database on device {device}"),
            vec![Action::Write {
                target: Target::Member(device),
                offset,
                bytes,
            }],
        );
    }
}

/// `spaces space create`.
pub fn plan_create_space<D: ReadAt>(pool: &Pool<D>, spec: &SpaceSpec, guids: Guids) -> Result<(Plan, NewSpace)> {
    let db = check_pool(pool)?;
    if pool.find_space(&spec.name).is_some() || spec.name.is_empty() {
        return Err(Error::Pool(format!(
            "a space named \"{}\" exists or the name is empty",
            spec.name
        )));
    }
    let mut slabs = Slabs::of(&db)?;
    let n = slabs.disks() as u64;
    let interleave = spec.interleave.unwrap_or(256 << 10);
    if !interleave.is_power_of_two() || !(16 << 10..=16 << 20).contains(&interleave) {
        return Err(Error::Pool("interleave: a power of two from 16 KiB to 16 MiB".into()));
    }
    let (copies, columns, redundancy) = match spec.resiliency {
        1 => (1, spec.columns.unwrap_or(n.min(8)), 0),
        2 => {
            let copies = spec.copies.unwrap_or(2);
            if !(2..=3).contains(&copies) || (copies == 3 && n < 5) {
                return Err(Error::Pool("mirror: 2 copies, or 3 on at least five disks".into()));
            }
            (copies, spec.columns.unwrap_or((n / copies).clamp(1, 8)), copies - 1)
        }
        3 => (1, spec.columns.unwrap_or(3), 1),
        r => return Err(Error::Pool(format!("resiliency {r}"))),
    };
    if columns == 0 || columns * copies > n || (spec.resiliency == 3 && columns < 3) {
        return Err(Error::Pool(format!(
            "{columns} columns of {copies} copies need that many disks of the {n} eligible (parity: at least 3 columns)"
        )));
    }
    let data_columns = columns - redundancy * (spec.resiliency == 3) as u64;
    let unit = if spec.thin { SLAB_SIZE } else { 4 * SLAB_SIZE };
    let group = unit * data_columns;
    if spec.size == 0 {
        return Err(Error::Pool("the size must not be 0".into()));
    }
    let size = spec.size.div_ceil(group) * group;

    // Ids and numbers: the space, then (as Windows) the cache, then the
    // journal or dirty region log.
    let mut next_id = next_ids(&db)?;
    let mut id = || {
        next_id += 1;
        next_id - 1
    };
    let mut taken: Vec<u64> = Vec::new();
    let mut number = |db: &Database| -> Result<u64> {
        let mut n = next_space_number(db)?;
        while taken.contains(&n) {
            n += 1;
        }
        taken.push(n);
        Ok(n)
    };
    let space_id = id();
    let space_number = number(&db)?;
    let now = filetime_now();
    let mut hidden = Vec::new();
    let log = match spec.resiliency {
        2 => Some((Hidden::DirtyRegions, copies)),
        3 => Some((Hidden::Journal, 2)),
        _ => None,
    };
    let cache = if spec.resiliency == 3 {
        let columns = (n / 2).max(1);
        let size = spec.write_cache.unwrap_or(1 << 30);
        let size = size.div_ceil(SLAB_SIZE * columns) * SLAB_SIZE * columns;
        if size < 512 << 20 {
            return Err(Error::Pool("the write-back cache must be at least 512 MiB".into()));
        }
        Some((size, columns))
    } else if spec.write_cache.is_some_and(|c| c > 0) {
        return Err(Error::Pool(
            "only parity spaces take a write-back cache (as on Windows 11 24H2)".into(),
        ));
    } else {
        None
    };
    let cache_ids = cache.map(|_| (id(), id(), number(&db)));
    let log_ids = log.map(|_| (id(), id(), number(&db)));
    let exhausted = || Error::Pool("the pool has no room for the space".into());
    // Hidden spaces first, as Windows allocates them.
    let mut place_hidden = |kind: Hidden,
                            (container_id, child_id, number): (u64, u64, Result<u64>),
                            size: u64,
                            columns: u64,
                            copies: u64|
     -> Result<NewHidden> {
        let mut placed = Vec::new();
        for row in 0..size / (SLAB_SIZE * columns) {
            let mut used = Vec::new();
            for column in 0..columns {
                for copy in 0..copies {
                    let (disk, slab) = slabs.allocate(1, &used).ok_or_else(exhausted)?;
                    used.push(disk);
                    placed.push((row, column, copy, disk, slab));
                }
            }
        }
        Ok(NewHidden {
            kind,
            container_id,
            container_guid: guids(),
            number: number?,
            child_id,
            child_guid: guids(),
            size,
            redundancy: copies - 1,
            copies,
            columns,
            interleave_log2: interleave.trailing_zeros() as u8,
            slabs: placed,
            created: now,
        })
    };
    if let (Some((kind, log_copies)), Some(ids)) = (log, log_ids) {
        hidden.push(place_hidden(kind, ids, SLAB_SIZE, 1, log_copies)?);
    }
    if let (Some((size, columns)), Some(ids)) = (cache, cache_ids) {
        hidden.push(place_hidden(Hidden::Cache, ids, size, columns, 2)?);
    }
    // The data: every row group of a fixed space, the first of a thin one.
    let groups = if spec.thin { 1 } else { size / group };
    let per_extent = unit / SLAB_SIZE;
    let mut extents = Vec::new();
    for g in 0..groups {
        let mut used = Vec::new();
        for column in 0..columns {
            for copy in 0..copies {
                let (disk, slab) = slabs.allocate(per_extent, &used).ok_or_else(exhausted)?;
                used.push(disk);
                extents.push(ExtentRecord {
                    flags: 0,
                    stale_marker: 0xffff_ffff,
                    space_id,
                    virtual_slab: g * per_extent * data_columns,
                    column,
                    copy,
                    slab_count: per_extent,
                    disk_id: disk,
                    physical_slab: slab,
                });
            }
        }
    }
    let new = NewSpace {
        id: space_id,
        guid: guids(),
        name: spec.name.clone(),
        number: space_number,
        size,
        provisioning: if spec.thin { 1 } else { 2 },
        allocation_unit: unit,
        resiliency: spec.resiliency,
        redundancy,
        copies,
        columns,
        interleave_log2: interleave.trailing_zeros() as u8,
        write_cache: cache.map_or(0, |c| c.0),
        extents,
        hidden,
        created: now,
    };
    let writes = new.plan(pool, &db, now, metadata_disks(pool).as_deref())?;
    let (database, setup): (Vec<_>, Vec<_>) = writes.into_iter().partition(|(device, offset, _)| {
        pool.members
            .iter()
            .any(|m| m.device == *device && *offset == m.partition.offset + POOL_DB_OFFSET)
    });
    let mut plan = Plan {
        summary: vec![format!(
            "create space \"{}\" ({}): {}, {} columns, {} copies, interleave {} KiB, {} bytes{}",
            new.name,
            new.guid,
            ["simple", "mirror", "parity"][spec.resiliency as usize - 1],
            columns,
            copies,
            interleave >> 10,
            size,
            if spec.thin { ", thin" } else { "" }
        )],
        steps: Vec::new(),
    };
    plan.step(
        "databases in the metadata space, hidden spaces, the first sector",
        setup
            .into_iter()
            .map(|(device, offset, bytes)| Action::Write {
                target: Target::Member(device),
                offset,
                bytes,
            })
            .collect(),
    );
    database_steps(&mut plan, database);
    Ok((plan, new))
}
