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

/// Largest space: 2^32 slabs (1 EiB), what extent records may address.
pub const MAX_SPACE_SIZE: u64 = SLAB_SIZE << 32;

/// `size` rounded up to whole rows of `data_columns` allocation units of
/// `unit` bytes, refused beyond [`MAX_SPACE_SIZE`] (and for a row size that
/// is no whole number of slabs, from a record not understood).
fn whole_rows(size: u64, unit: u64, data_columns: u64) -> Result<u64> {
    let row = unit
        .checked_mul(data_columns)
        .filter(|r| *r > 0 && r.is_multiple_of(SLAB_SIZE))
        .ok_or_else(|| Error::Pool(format!("rows of {data_columns} units of {unit} bytes")))?;
    size.checked_next_multiple_of(row)
        .filter(|s| *s <= MAX_SPACE_SIZE)
        .ok_or_else(|| Error::Pool(format!("{size} bytes: spaces hold at most {MAX_SPACE_SIZE} bytes")))
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
    let size = whole_rows(spec.size, unit, data_columns)?;

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
        let size = whole_rows(spec.write_cache.unwrap_or(1 << 30), SLAB_SIZE, columns)?;
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

/// A plan that writes `db` (one update of the pool's database) to every
/// member carrying a copy, one after the other.
fn database_plan<D: ReadAt>(pool: &Pool<D>, summary: String, db: &Database) -> Plan {
    let mut plan = Plan {
        summary: vec![summary],
        steps: Vec::new(),
    };
    let writes = pool
        .members
        .iter()
        .filter(|m| m.header.database_copy)
        .map(|m| (m.device, m.partition.offset + POOL_DB_OFFSET, db.bytes().to_vec()))
        .collect();
    database_steps(&mut plan, writes);
    plan
}

fn space_id<D: ReadAt>(pool: &Pool<D>, name: &str) -> Result<u64> {
    pool.user_spaces()
        .find(|s| s.name() == name || s.id().to_string() == name || s.info.guid.to_string() == name)
        .map(|s| s.id())
        .ok_or_else(|| Error::Pool(format!("no space \"{name}\"")))
}

/// `spaces pool rename`.
pub fn plan_rename_pool<D: ReadAt>(pool: &Pool<D>, name: &str) -> Result<Plan> {
    let db = check_pool(pool)?;
    if name.is_empty() {
        return Err(Error::Pool("the name must not be empty".into()));
    }
    let next = crate::manage::rename_pool(&db, name, filetime_now())?;
    Ok(database_plan(
        pool,
        format!("rename pool \"{}\" to \"{name}\"", pool.name),
        &next,
    ))
}

/// `spaces space rename`.
pub fn plan_rename_space<D: ReadAt>(pool: &Pool<D>, space: &str, name: &str) -> Result<Plan> {
    let db = check_pool(pool)?;
    let id = space_id(pool, space)?;
    if name.is_empty() || pool.find_space(name).is_some() {
        return Err(Error::Pool(format!(
            "a space named \"{name}\" exists or the name is empty"
        )));
    }
    let next = crate::manage::rename_space(&db, id, name, filetime_now())?;
    Ok(database_plan(
        pool,
        format!("rename space \"{space}\" to \"{name}\""),
        &next,
    ))
}

/// `spaces space delete`: its data is lost.
pub fn plan_delete_space<D: ReadAt>(pool: &Pool<D>, space: &str) -> Result<Plan> {
    let db = check_pool(pool)?;
    let id = space_id(pool, space)?;
    let next = crate::manage::delete_space(&db, id, filetime_now())?;
    Ok(database_plan(
        pool,
        format!("delete space \"{space}\" and everything on it"),
        &next,
    ))
}

/// `spaces disk set`: media type (0 unspecified, 1 HDD, 2 SSD) and usage
/// (1 Auto-Select, 2 Manual-Select, 3 Hot Spare).
pub fn plan_set_disk<D: ReadAt>(pool: &Pool<D>, disk_id: u64, media: Option<u8>, usage: Option<u8>) -> Result<Plan> {
    let db = check_pool(pool)?;
    if !pool.disks.contains_key(&disk_id) {
        return Err(Error::Pool(format!("no disk with id {disk_id}")));
    }
    if media.is_some_and(|m| m > 2) || usage.is_some_and(|u| !(1..=3).contains(&u)) {
        return Err(Error::Pool(
            "media 0-2, usage 1-3 (retiring moves data: spaces disk retire)".into(),
        ));
    }
    let next = crate::manage::set_disk(&db, disk_id, media, usage, filetime_now())?;
    Ok(database_plan(
        pool,
        format!("set disk {disk_id}: media {media:?}, usage {usage:?}"),
        &next,
    ))
}

/// `spaces space resize` to a larger size: a fixed space gets the extents
/// of its new rows, a thin space only the new size (rows are allocated as
/// they are written).
pub fn plan_resize_space<D: ReadAt>(pool: &Pool<D>, space: &str, size: u64) -> Result<Plan> {
    let db = check_pool(pool)?;
    let id = space_id(pool, space)?;
    let s = &pool.spaces[&id];
    let policy = s
        .info
        .policy
        .ok_or_else(|| Error::Pool("the space has no placement policy".into()))?;
    if pool.children(id).any(|c| c.info.is_child && !c.extents.is_empty()) {
        return Err(Error::Pool("resizing tiered spaces is not supported".into()));
    }
    let data_columns = policy
        .columns
        .checked_sub(if policy.resiliency == crate::format::Resiliency::Parity {
            policy.redundancy
        } else {
            0
        })
        .ok_or_else(|| Error::Pool("more parity than columns".into()))?;
    let unit = s.info.allocation_unit;
    let old = s.info.size.unwrap_or(0);
    let size = whole_rows(size, unit, data_columns)?;
    let group = unit * data_columns;
    if size <= old {
        return Err(Error::Pool(format!("the space is {old} bytes; it can only grow")));
    }
    let mut extents = Vec::new();
    if s.info.provisioning == crate::format::Provisioning::Fixed {
        let mut slabs = Slabs::of(&db)?;
        let per_extent = unit / SLAB_SIZE;
        for g in old / group..size / group {
            let mut used = Vec::new();
            for column in 0..policy.columns {
                for copy in 0..policy.copies {
                    let (disk, slab) = slabs
                        .allocate(per_extent, &used)
                        .ok_or_else(|| Error::Pool("the pool has no room for the space's new rows".into()))?;
                    used.push(disk);
                    extents.push(ExtentRecord {
                        flags: 0,
                        stale_marker: 0xffff_ffff,
                        space_id: id,
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
    }
    let next = crate::manage::resize_space(&db, id, size, &extents, filetime_now())?;
    Ok(database_plan(
        pool,
        format!("resize space \"{space}\" from {old} to {size} bytes"),
        &next,
    ))
}

/// `spaces pool remove`: a pool without spaces; each member keeps its
/// header and database behind a partition table without the pool
/// partition (as `Remove-StoragePool` leaves it).
pub fn plan_remove_pool<D: ReadAt>(pool: &Pool<D>) -> Result<Plan> {
    check_pool(pool)?;
    if let Some(s) = pool.user_spaces().next() {
        return Err(Error::Pool(format!(
            "the pool still has spaces (\"{}\"); delete them first",
            s.name()
        )));
    }
    let mut plan = Plan {
        summary: vec![format!("remove pool \"{}\" ({})", pool.name, pool.guid)],
        steps: Vec::new(),
    };
    for m in &pool.members {
        let writes = crate::gpt::remove_partitions(&pool.devices[m.device], crate::gpt::STORAGE_SPACES_PARTITION_TYPE)?
            .ok_or_else(|| Error::Pool(format!("device {} has no GPT", m.device)))?;
        plan.step(
            format!("device {}: partition table without the pool partition", m.device),
            writes
                .into_iter()
                .map(|(offset, bytes)| Action::Write {
                    target: Target::Member(m.device),
                    offset,
                    bytes,
                })
                .collect(),
        );
    }
    Ok(plan)
}

/// `spaces disk add` of a blank disk (to a pool of at most four disks).
pub fn plan_add_disk<D: ReadAt>(pool: &Pool<D>, disk: &BlankDisk, guids: Guids) -> Result<Plan> {
    let db = check_pool(pool)?;
    if disk.logical_sector > pool.logical_sector_size as u64 {
        return Err(Error::Pool(format!(
            "a disk of {}-byte sectors does not fit a pool of {}-byte sectors",
            disk.logical_sector, pool.logical_sector_size
        )));
    }
    let now = filetime_now();
    let new = NewDisk {
        size: disk.size,
        sector: disk.logical_sector,
        guid: guids(),
        gpt_disk_guid: guids(),
        msr_guid: guids(),
        partition_guid: guids(),
        joined: now,
        manufacturer: disk.manufacturer.clone(),
        model: disk.model.clone(),
        database_copy: true,
    };
    let id = next_ids(&db)?;
    Ok(crate::manage::add_disk(pool, &db, &new, id, now, |_| now)?.plan)
}

/// `spaces disk remove` of a retired disk that holds nothing any more (see
/// [`plan_retire_disk`]): two database updates, then its partition table
/// without the pool partition.
pub fn plan_remove_disk<D: ReadAt>(pool: &Pool<D>, disk_id: u64) -> Result<Plan> {
    let disk = pool
        .disks
        .get(&disk_id)
        .ok_or_else(|| Error::Pool(format!("no disk with id {disk_id}")))?;
    let member = disk.member.map(|m| &pool.members[m]);
    // A missing disk (repaired away from): only the others are written.
    let db = if member.is_some() {
        check_pool(pool)?
    } else {
        check_pool_missing_disks(pool)?
    };
    if let Some(m) = member
        && (disk.usage != crate::format::DiskUsage::Retired || m.header.database_copy)
    {
        return Err(Error::Pool(format!(
            "disk {disk_id} must be retired first (spaces disk retire)"
        )));
    }
    let now = filetime_now();
    let (first, second) = crate::manage::remove_disk_updates(&db, disk_id, now)?;
    let mut plan = Plan {
        summary: vec![format!(
            "remove disk {disk_id} ({}) from pool \"{}\"",
            disk.guid, pool.name
        )],
        steps: Vec::new(),
    };
    // A missing disk leaves the lists in the metadata space now (a retired
    // one left them when it was retired).
    if member.is_none() {
        plan.step(
            "databases in the metadata space without the disk",
            space_databases_without(pool, &db, disk_id, now)?,
        );
    }
    let copies: Vec<&crate::Member> = pool.members.iter().filter(|m| m.header.database_copy).collect();
    for (what, db) in [("first", &first), ("second", &second)] {
        for m in &copies {
            plan.step(
                format!("{what} update on device {}", m.device),
                vec![Action::Write {
                    target: Target::Member(m.device),
                    offset: m.partition.offset + POOL_DB_OFFSET,
                    bytes: db.bytes().to_vec(),
                }],
            );
        }
    }
    let Some(member) = member else {
        return Ok(plan);
    };
    let table = crate::gpt::remove_partitions(&pool.devices[member.device], crate::gpt::STORAGE_SPACES_PARTITION_TYPE)?
        .ok_or_else(|| Error::Pool(format!("disk {disk_id} has no GPT")))?;
    plan.step(
        format!("device {}: partition table without the pool partition", member.device),
        table
            .into_iter()
            .map(|(offset, bytes)| Action::Write {
                target: Target::Member(member.device),
                offset,
                bytes,
            })
            .collect(),
    );
    Ok(plan)
}

/// `spaces disk retire`: marks the disk retired and moves everything on it
/// to the other disks, as Windows' retirement and repair leave it: the
/// disk's record retired; each extent on it copied to another disk that
/// holds nothing else of its row (a new copy flagged as being regenerated,
/// the slabs copied, then recorded as the old copy and the old extent
/// freed: a crash in between leaves the old copy current); the databases
/// of the metadata space no longer listing the disk (its last entry moved
/// into its place); finally the disk's record without its database copy,
/// and the disk a last copy of the database and a header without the copy.
/// Pools of at most five disks (every disk carries the database).
pub fn plan_retire_disk<D: ReadAt>(pool: &Pool<D>, disk_id: u64) -> Result<Plan> {
    use crate::format::{DATA_AREA_OFFSET, DiskHeader, Record, SpaceRole};
    let mut db = check_pool(pool)?;
    if pool.members.len() > DATABASE_COPIES {
        return Err(Error::Pool(
            "retiring disks of pools of more than five disks is not supported yet".into(),
        ));
    }
    let disk = pool
        .disks
        .get(&disk_id)
        .ok_or_else(|| Error::Pool(format!("no disk with id {disk_id}")))?;
    let member = disk
        .member
        .map(|m| pool.members[m].clone())
        .ok_or_else(|| Error::Pool(format!("disk {disk_id} is not at hand")))?;
    let now = filetime_now();
    let mut plan = Plan {
        summary: vec![format!(
            "retire disk {disk_id} ({}) and move its data to the others",
            disk.guid
        )],
        steps: Vec::new(),
    };
    let others: Vec<crate::Member> = pool
        .members
        .iter()
        .filter(|m| m.header.database_copy && m.device != member.device)
        .cloned()
        .collect();
    let write_db = |plan: &mut Plan, what: &str, db: &Database, with_retired: bool| {
        let mut to: Vec<&crate::Member> = others.iter().collect();
        if with_retired {
            to.push(&member);
        }
        for m in to {
            plan.step(
                format!("{what} on device {}", m.device),
                vec![Action::Write {
                    target: Target::Member(m.device),
                    offset: m.partition.offset + POOL_DB_OFFSET,
                    bytes: db.bytes().to_vec(),
                }],
            );
        }
    };
    // Retired (every copy, the disk's own last).
    if disk.usage != crate::format::DiskUsage::Retired {
        db = crate::manage::set_disk(&db, disk_id, None, Some(5), now)?;
        write_db(&mut plan, "disk retired", &db, true);
    }
    // The moves.
    let metadata_id = pool
        .spaces
        .values()
        .find(|s| s.info.role == SpaceRole::Metadata)
        .map(|s| s.id())
        .ok_or_else(|| Error::Pool("no metadata space".into()))?;
    let decoded = |db: &Database| -> Result<Vec<(u32, ExtentRecord)>> {
        Ok(assemble_records(db.bytes(), 0x40)?
            .iter()
            .filter_map(|r| match Record::decode(r) {
                Ok(Record::Extent(e)) => Some((r.id, e)),
                _ => None,
            })
            .collect())
    };
    let moving: Vec<(u32, ExtentRecord)> = decoded(&db)?
        .into_iter()
        .filter(|(_, e)| e.disk_id == disk_id && e.space_id != metadata_id)
        .collect();
    let mut slabs = Slabs::of(&db)?;
    for (old_id, e) in moving {
        let extents = decoded(&db)?;
        let row: Vec<&ExtentRecord> = extents
            .iter()
            .map(|(_, x)| x)
            .filter(|x| x.space_id == e.space_id && x.virtual_slab == e.virtual_slab)
            .collect();
        let exclude: Vec<u64> = row.iter().map(|x| x.disk_id).chain([disk_id]).collect();
        let (target, slab) = slabs.allocate(e.slab_count, &exclude).ok_or_else(|| {
            Error::Pool(format!(
                "no other disk has {} free slabs in a row for space {} slab {} (its row is on disks {:?})",
                e.slab_count, e.space_id, e.virtual_slab, exclude
            ))
        })?;
        let next_copy = row
            .iter()
            .filter(|x| x.column == e.column)
            .map(|x| x.copy)
            .max()
            .unwrap_or(0)
            + 1;
        let from = pool
            .slab_location(e.disk_id, e.physical_slab)?
            .ok_or_else(|| Error::Pool(format!("disk {} is not at hand", e.disk_id)))?;
        let rebuild = |to: Target, to_offset: u64| Action::Copy {
            from: Target::Member(from.0),
            from_offset: from.1,
            to,
            to_offset,
            len: e.slab_count * SLAB_SIZE,
        };
        let mut write = |plan: &mut Plan, what: &str, db: &Database| write_db(plan, what, db, false);
        relocate(
            pool,
            &mut plan,
            &mut db,
            now,
            old_id,
            &e,
            next_copy,
            (target, slab),
            &rebuild,
            &mut write,
        )?;
    }
    // The databases of the metadata space: without the disk.
    let space_dbs = space_databases_without(pool, &db, disk_id, now)?;
    plan.step("databases in the metadata space without the disk", space_dbs);
    // Its copy of the pool database, last.
    let sequence = db.sequence() + 1;
    let (old, mut record) = assemble_records(db.bytes(), 0x40)?
        .into_iter()
        .filter(|r| r.kind == 2)
        .find_map(|r| {
            DiskBody::decode(&r.body)
                .ok()
                .filter(|d| d.id == disk_id)
                .map(|d| (r, d))
        })
        .ok_or_else(|| Error::Pool(format!("no disk record {disk_id}")))?;
    record.sequence = sequence;
    record.database_copy = false;
    let body = record.encode();
    let (mut next, _) = db.updated(&[(old.kind, old.version, &body)], &[old.id])?;
    next.commit(sequence, now);
    write_db(&mut plan, "disk without its database copy", &next, false);
    let header = DiskHeader {
        generation: member.header.generation + 1,
        database_copy: false,
        ..member.header.clone()
    };
    let mut page = header.encode().to_vec();
    page.resize(POOL_DB_OFFSET as usize, 0);
    page.extend_from_slice(next.bytes());
    plan.step(
        format!("device {}: last database and header without the copy", member.device),
        vec![Action::Write {
            target: Target::Member(member.device),
            offset: member.partition.offset,
            bytes: page,
        }],
    );
    let _ = DATA_AREA_OFFSET;
    Ok(plan)
}

/// Like [`check_pool`], but a pool may miss disks (the only warnings
/// allowed), and its spaces may be degraded (not failed).
fn check_pool_missing_disks<D: ReadAt>(pool: &Pool<D>) -> Result<Database> {
    let other: Vec<&String> = pool.warnings.iter().filter(|w| !w.ends_with(" is missing")).collect();
    if !other.is_empty() {
        return Err(Error::Pool(format!(
            "the pool is not in a state repair handles ({})",
            other.iter().map(|w| w.as_str()).collect::<Vec<_>>().join("; ")
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
        if pool.open_space(space.id())?.condition() == crate::Condition::Failed {
            return Err(Error::Pool(format!(
                "space \"{}\" has lost data; it cannot be repaired",
                space.name()
            )));
        }
    }
    Ok(db)
}

/// `spaces pool repair`: rebuilds every copy that is on a missing disk or
/// out of date on another disk (as `Repair-VirtualDisk` regenerates them):
/// copies half regenerated (a crash, or Windows' own) are dropped first;
/// then each copy is rebuilt as a regenerating copy on a disk that holds
/// nothing else of its row, from a current copy of the same column (mirror)
/// or as the XOR of the row's other columns (single parity), and recorded
/// in place of the old one. Missing disks can be removed afterwards
/// (`spaces disk remove`).
pub fn plan_repair<D: ReadAt>(pool: &Pool<D>) -> Result<Plan> {
    use crate::format::{Resiliency, SpaceRole};
    let mut db = check_pool_missing_disks(pool)?;
    let now = filetime_now();
    let mut plan = Plan {
        summary: vec![format!("repair pool \"{}\"", pool.name)],
        steps: Vec::new(),
    };
    let copies: Vec<crate::Member> = pool
        .members
        .iter()
        .filter(|m| m.header.database_copy)
        .cloned()
        .collect();
    let write_db = |plan: &mut Plan, what: &str, db: &Database| {
        for m in &copies {
            plan.step(
                format!("{what} on device {}", m.device),
                vec![Action::Write {
                    target: Target::Member(m.device),
                    offset: m.partition.offset + POOL_DB_OFFSET,
                    bytes: db.bytes().to_vec(),
                }],
            );
        }
    };
    let extents = |db: &Database| -> Result<Vec<(u32, ExtentRecord)>> {
        Ok(assemble_records(db.bytes(), 0x40)?
            .iter()
            .filter_map(|r| match Record::decode(r) {
                Ok(Record::Extent(e)) => Some((r.id, e)),
                _ => None,
            })
            .collect())
    };
    let present = |disk: u64| pool.disks.get(&disk).is_some_and(|d| d.member.is_some());
    let metadata = pool
        .spaces
        .values()
        .find(|s| s.info.role == SpaceRole::Metadata)
        .map(|s| s.id());
    // Half regenerated copies go.
    let partial: Vec<u32> = extents(&db)?
        .iter()
        .filter(|(_, e)| e.flags & ExtentRecord::FLAG_REGENERATING != 0)
        .map(|(id, _)| *id)
        .collect();
    if !partial.is_empty() {
        let (mut next, _) = db.updated(&[], &partial)?;
        next.commit(db.sequence() + 1, now);
        db = next;
        write_db(
            &mut plan,
            &format!("{} half regenerated copies dropped", partial.len()),
            &db,
        );
    }
    let broken: Vec<(u32, ExtentRecord)> = extents(&db)?
        .into_iter()
        .filter(|(_, e)| Some(e.space_id) != metadata && (!present(e.disk_id) || !e.is_current()))
        .collect();
    let mut slabs = Slabs::of(&db)?;
    for (old_id, e) in broken {
        let all = extents(&db)?;
        let row: Vec<&ExtentRecord> = all
            .iter()
            .map(|(_, x)| x)
            .filter(|x| x.space_id == e.space_id && x.virtual_slab == e.virtual_slab)
            .collect();
        let policy = pool
            .spaces
            .get(&e.space_id)
            .and_then(|s| s.info.policy)
            .ok_or_else(|| Error::Pool(format!("space {} has no placement policy", e.space_id)))?;
        let good = |x: &&&ExtentRecord| x.is_current() && present(x.disk_id);
        let location = |x: &ExtentRecord| -> Result<(Target, u64)> {
            let (device, at) = pool
                .slab_location(x.disk_id, x.physical_slab)?
                .ok_or_else(|| Error::Pool(format!("disk {} is not at hand", x.disk_id)))?;
            Ok((Target::Member(device), at))
        };
        // Where the data comes from.
        let source: Vec<(Target, u64)> = match policy.resiliency {
            Resiliency::Mirror => match row.iter().find(|x| good(x) && x.column == e.column && x.copy != e.copy) {
                Some(x) => vec![location(x)?],
                None => {
                    return Err(Error::Pool(format!(
                        "no current copy of space {} slab {}",
                        e.space_id, e.virtual_slab
                    )));
                }
            },
            Resiliency::Parity if policy.redundancy == 1 => {
                let others: Vec<&&ExtentRecord> = row.iter().filter(|x| good(x) && x.column != e.column).collect();
                if others.len() as u64 != policy.columns - 1 || others.iter().any(|x| x.slab_count != e.slab_count) {
                    return Err(Error::Pool(format!(
                        "space {} slab {} lacks columns to rebuild from",
                        e.space_id, e.virtual_slab
                    )));
                }
                others.iter().map(|x| location(x)).collect::<Result<_>>()?
            }
            _ => {
                return Err(Error::Pool(format!(
                    "space {} ({:?}) cannot rebuild a lost copy",
                    e.space_id, policy.resiliency
                )));
            }
        };
        let exclude: Vec<u64> = row.iter().map(|x| x.disk_id).collect();
        let (target, slab) = slabs.allocate(e.slab_count, &exclude).ok_or_else(|| {
            Error::Pool(format!(
                "no disk has {} free slabs in a row for space {} slab {} (its row is on disks {:?}); add a disk",
                e.slab_count, e.space_id, e.virtual_slab, exclude
            ))
        })?;
        let next_copy = row
            .iter()
            .filter(|x| x.column == e.column)
            .map(|x| x.copy)
            .max()
            .unwrap_or(0)
            + 1;
        let rebuild = |to: Target, to_offset: u64| {
            let len = e.slab_count * SLAB_SIZE;
            match source.len() {
                1 => Action::Copy {
                    from: source[0].0,
                    from_offset: source[0].1,
                    to,
                    to_offset,
                    len,
                },
                _ => Action::Xor {
                    from: source.clone(),
                    to,
                    to_offset,
                    len,
                },
            }
        };
        let mut write = |plan: &mut Plan, what: &str, db: &Database| write_db(plan, what, db);
        relocate(
            pool,
            &mut plan,
            &mut db,
            now,
            old_id,
            &e,
            next_copy,
            (target, slab),
            &rebuild,
            &mut write,
        )?;
    }
    if plan.steps.is_empty() {
        plan.summary.push("nothing to repair".into());
    }
    Ok(plan)
}

/// Rewrites the databases in the metadata space without disk `disk_id` in
/// their disk lists (its entry replaced by the last, as Windows does), on
/// every copy of the metadata space but the disk's own.
fn space_databases_without<D: ReadAt>(pool: &Pool<D>, db: &Database, disk_id: u64, now: u64) -> Result<Vec<Action>> {
    use crate::create::SPACE_DATABASE_STRIDE;
    use crate::format::SpaceRole;
    let disk = pool
        .disks
        .get(&disk_id)
        .ok_or_else(|| Error::Pool(format!("no disk with id {disk_id}")))?;
    let meta = pool
        .spaces
        .values()
        .find(|s| s.info.role == SpaceRole::Metadata)
        .ok_or_else(|| Error::Pool("no metadata space".into()))?;
    let holders: Vec<&ExtentRecord> = meta.extents.iter().filter(|x| x.disk_id != disk_id).collect();
    let mut space_dbs = Vec::new();
    for number in assemble_records(db.bytes(), 0x40)?
        .iter()
        .filter(|r| r.kind == 3)
        .filter_map(|r| SpaceBody::decode(false, &r.body).ok())
        .filter(|s| s.role != 1)
        .map(|s| s.number)
    {
        let offset = number * SPACE_DATABASE_STRIDE;
        let Some((device, at)) = holders
            .iter()
            .find_map(|x| pool.slab_location(x.disk_id, x.physical_slab).ok().flatten())
        else {
            continue;
        };
        let old = Database::read_formatted(&pool.devices[device], at + offset)?;
        let Some(list) = assemble_records(old.bytes(), 0x40)?.into_iter().find(|r| r.kind == 7) else {
            continue;
        };
        let mut c = crate::format::Cursor::new(&list.body);
        c.varint()?;
        c.varint()?;
        let n = c.varint()? as usize;
        let mut disks: Vec<Guid> = (0..n).map(|_| c.guid()).collect::<Result<_>>()?;
        let Some(pos) = disks.iter().position(|g| *g == disk.guid) else {
            continue;
        };
        disks.swap_remove(pos);
        let mut body = crate::format::encode_varint(0);
        body.extend(crate::format::encode_varint(old.sequence() + 1));
        body.extend(crate::format::encode_varint(disks.len() as u64));
        for g in &disks {
            body.extend_from_slice(&g.0);
        }
        let (mut updated, _) = old.updated(&[(7, list.version, &body)], &[list.id])?;
        updated.commit(old.sequence() + 1, now);
        for x in &holders {
            if let Some((device, at)) = pool.slab_location(x.disk_id, x.physical_slab)? {
                space_dbs.push(Action::Write {
                    target: Target::Member(device),
                    offset: at + offset,
                    bytes: updated.bytes().to_vec(),
                });
            }
        }
    }
    Ok(space_dbs)
}

/// Moves extent `e` (record `old_id`) to physical slab `slab` of disk
/// `target`: the data first written into the free slabs by `rebuild` (given
/// their location) and made durable, then one database update records the
/// extent there and frees the old place. A crash before the update leaves
/// only unreferenced slabs written; the copies of the update agree on
/// either place holding the data. (Windows, whose own moves go through a
/// copy flagged as being regenerated, marked disks lost when it found such
/// a copy of a dirty region log written by Linux after a crash.) `write`
/// writes a database update to the members; `next_copy` is unused.
#[allow(clippy::too_many_arguments)]
fn relocate<D: ReadAt>(
    pool: &Pool<D>,
    plan: &mut Plan,
    db: &mut Database,
    now: u64,
    old_id: u32,
    e: &ExtentRecord,
    _next_copy: u64,
    (target, slab): (u64, u64),
    rebuild: &dyn Fn(Target, u64) -> Action,
    write: &mut dyn FnMut(&mut Plan, &str, &Database),
) -> Result<()> {
    let (device, at) = pool
        .slab_location(target, slab)?
        .ok_or_else(|| Error::Pool(format!("disk {target} is not at hand")))?;
    plan.step(
        format!("data of space {} slab {} to disk {target}", e.space_id, e.virtual_slab),
        vec![rebuild(Target::Member(device), at)],
    );
    let sequence = db.sequence() + 1;
    let body = ExtentRecord {
        flags: e.flags & !ExtentRecord::FLAG_REGENERATING,
        stale_marker: ExtentRecord::CURRENT,
        disk_id: target,
        physical_slab: slab,
        ..*e
    }
    .encode(sequence);
    let (mut next, _) = db.updated(&[(4, 6, &body)], &[old_id])?;
    next.commit(sequence, now);
    *db = next;
    write(plan, &format!("space {} slab {} moved", e.space_id, e.virtual_slab), db);
    Ok(())
}

/// `spaces pool optimize` (as `Optimize-StoragePool`): moves extents from
/// the fullest eligible disk to the emptiest one whose row does not use it
/// yet, one at a time with the moves of a retirement, until the disks' used
/// slabs differ by less than an extent.
pub fn plan_rebalance<D: ReadAt>(pool: &Pool<D>) -> Result<Plan> {
    use crate::format::SpaceRole;
    let mut db = check_pool(pool)?;
    let now = filetime_now();
    let mut plan = Plan {
        summary: vec![format!("optimize pool \"{}\"", pool.name)],
        steps: Vec::new(),
    };
    let copies: Vec<crate::Member> = pool
        .members
        .iter()
        .filter(|m| m.header.database_copy)
        .cloned()
        .collect();
    let mut write = |plan: &mut Plan, what: &str, db: &Database| {
        for m in &copies {
            plan.step(
                format!("{what} on device {}", m.device),
                vec![Action::Write {
                    target: Target::Member(m.device),
                    offset: m.partition.offset + POOL_DB_OFFSET,
                    bytes: db.bytes().to_vec(),
                }],
            );
        }
    };
    let metadata = pool
        .spaces
        .values()
        .find(|s| s.info.role == SpaceRole::Metadata)
        .map(|s| s.id());
    for _ in 0..1024 {
        let slabs = Slabs::of(&db)?;
        let used: BTreeMap<u64, u64> = slabs
            .disks
            .iter()
            .map(|(&d, (_, r))| (d, r.iter().map(|x| x.1).sum()))
            .collect();
        let Some((&fullest, &most)) = used.iter().max_by_key(|&(d, u)| (u, std::cmp::Reverse(*d))) else {
            break;
        };
        let extents: Vec<(u32, ExtentRecord)> = assemble_records(db.bytes(), 0x40)?
            .iter()
            .filter_map(|r| match Record::decode(r) {
                Ok(Record::Extent(e)) => Some((r.id, e)),
                _ => None,
            })
            .collect();
        // The first extent of the fullest disk that fits an emptier disk
        // its row does not use, without making that disk the fuller one.
        let mut chosen = None;
        for (id, e) in extents
            .iter()
            .filter(|(_, e)| e.disk_id == fullest && Some(e.space_id) != metadata && e.is_current())
        {
            let row: Vec<&ExtentRecord> = extents
                .iter()
                .map(|(_, x)| x)
                .filter(|x| x.space_id == e.space_id && x.virtual_slab == e.virtual_slab)
                .collect();
            let exclude: Vec<u64> = row.iter().map(|x| x.disk_id).collect();
            // The emptiest disk the row does not use, if moving the extent
            // there leaves it below what the fullest has now.
            let target = used
                .iter()
                .filter(|(d, _)| !exclude.contains(d))
                .min_by_key(|&(d, u)| (*u, *d))
                .filter(|&(_, u)| u + e.slab_count < most)
                .map(|(d, _)| *d);
            if let Some(t) = target {
                let next_copy = row
                    .iter()
                    .filter(|x| x.column == e.column)
                    .map(|x| x.copy)
                    .max()
                    .unwrap_or(0)
                    + 1;
                chosen = Some((*id, *e, t, next_copy));
                break;
            }
        }
        let Some((old_id, e, target, next_copy)) = chosen else {
            break;
        };
        let mut place = slabs.clone();
        let exclude: Vec<u64> = used.keys().filter(|&&d| d != target).copied().collect();
        let Some((disk, slab)) = place.allocate(e.slab_count, &exclude) else {
            break;
        };
        let from = pool
            .slab_location(e.disk_id, e.physical_slab)?
            .ok_or_else(|| Error::Pool(format!("disk {} is not at hand", e.disk_id)))?;
        let rebuild = |to: Target, to_offset: u64| Action::Copy {
            from: Target::Member(from.0),
            from_offset: from.1,
            to,
            to_offset,
            len: e.slab_count * SLAB_SIZE,
        };
        relocate(
            pool,
            &mut plan,
            &mut db,
            now,
            old_id,
            &e,
            next_copy,
            (disk, slab),
            &rebuild,
            &mut write,
        )?;
    }
    if plan.steps.is_empty() {
        plan.summary.push("the disks are balanced".into());
    }
    Ok(plan)
}

/// What a scrub found, and the plan that repairs it.
#[derive(Debug, Clone, Default)]
pub struct Scrub {
    pub lines: Vec<String>,
    /// Differences where nothing was being written: 1 MiB chunks of mirror
    /// copies that differ, parity units that do not match their data.
    pub mismatches: u64,
    /// Differences in mirror rows the dirty region table, or parity
    /// stripes the journal, lists as being written: what a crash leaves,
    /// which Windows settles when it takes the pool.
    pub unsettled: u64,
    /// Makes every difference agree: mirror chunks copied from the first
    /// copy, parity units recomputed from the data.
    pub plan: Plan,
}

/// `spaces pool scrub`: reads every copy of every mirror row and every
/// stripe of the single parity spaces and reports what disagrees. Neither
/// side of a difference is known to be the right one; the repair plan
/// keeps the first mirror copy and the parity spaces' data.
pub fn scrub<D: ReadAt>(pool: &Pool<D>) -> Result<Scrub> {
    use crate::format::Resiliency;
    check_pool(pool)?;
    let mut out = Scrub {
        plan: Plan {
            summary: vec![format!("make what scrubbing pool \"{}\" found agree", pool.name)],
            steps: Vec::new(),
        },
        ..Default::default()
    };
    const CHUNK: u64 = 1 << 20;
    let location = |disk: u64, slab: u64, offset: u64| -> Result<(Target, u64)> {
        let (device, at) = pool
            .slab_location(disk, slab)?
            .ok_or_else(|| Error::Pool(format!("disk {disk} is not at hand")))?;
        Ok((Target::Member(device), at + offset))
    };
    for space in pool.user_spaces() {
        let reader = pool.open_space(space.id())?;
        let l = reader.layout().clone();
        let skipped = match l.resiliency {
            _ if reader.is_tiered() => Some("tiered"),
            Resiliency::Simple => Some("simple, nothing to compare"),
            Resiliency::Parity if l.parity_units != 1 => Some("dual parity"),
            _ => None,
        };
        if let Some(why) = skipped {
            out.lines.push(format!("{}: {why}, not scrubbed", space.name()));
            continue;
        }
        let rows = reader.size().div_ceil(SLAB_SIZE * l.data_columns);
        let (mut mismatches, mut unsettled) = (0u64, 0u64);
        let mut repairs = Vec::new();
        let (mut first, mut other) = (vec![0u8; CHUNK as usize], vec![0u8; CHUNK as usize]);
        if l.resiliency == Resiliency::Mirror {
            let drt = reader.dirty_regions();
            for row in 0..rows {
                let listed = drt.is_some_and(|d| d.is_dirty(l.run_start_offset(row) / SLAB_SIZE));
                for column in 0..l.columns {
                    let copies: Vec<(u64, u64)> = l
                        .copies_of(column)
                        .iter()
                        .filter_map(|&c| l.physical(column, c, row))
                        .collect();
                    let Some((&(disk0, slab0), rest)) = copies.split_first() else {
                        continue;
                    };
                    for offset in (0..SLAB_SIZE).step_by(CHUNK as usize) {
                        pool.read_slab(disk0, slab0, offset, &mut first)?;
                        for &(disk, slab) in rest {
                            pool.read_slab(disk, slab, offset, &mut other)?;
                            if first == other {
                                continue;
                            }
                            *if listed { &mut unsettled } else { &mut mismatches } += 1;
                            let (from, from_offset) = location(disk0, slab0, offset)?;
                            let (to, to_offset) = location(disk, slab, offset)?;
                            repairs.push(Action::Copy {
                                from,
                                from_offset,
                                to,
                                to_offset,
                                len: CHUNK,
                            });
                        }
                    }
                }
            }
        } else {
            let journal = reader.journal();
            for row in (0..rows).filter(|&r| l.physical(0, 0, r).is_some()) {
                let columns: Vec<(u64, u64)> = (0..l.columns)
                    .map(|c| l.physical(c, 0, row))
                    .collect::<Option<_>>()
                    .ok_or_else(|| Error::Pool(format!("space \"{}\" row {row} lacks a column", space.name())))?;
                for offset in (0..SLAB_SIZE).step_by(CHUNK as usize) {
                    first.fill(0);
                    for &(disk, slab) in &columns {
                        pool.read_slab(disk, slab, offset, &mut other)?;
                        first.iter_mut().zip(&other).for_each(|(a, u)| *a ^= u);
                    }
                    for (k, unit) in first.chunks(l.interleave as usize).enumerate() {
                        if unit.iter().all(|&b| b == 0) {
                            continue;
                        }
                        let at = offset + k as u64 * l.interleave;
                        let loc = crate::layout::Location {
                            column: 0,
                            row,
                            offset_in_slab: at,
                            contiguous: l.interleave,
                        };
                        let stripe = l.stripe_of(&loc);
                        let listed = journal.is_some_and(|j| j.is_dirty(l.run_start_offset(row), stripe));
                        *if listed { &mut unsettled } else { &mut mismatches } += 1;
                        let parity = l.parity_column(stripe) as usize;
                        let from = columns
                            .iter()
                            .enumerate()
                            .filter(|&(c, _)| c != parity)
                            .map(|(_, &(disk, slab))| location(disk, slab, at))
                            .collect::<Result<Vec<_>>>()?;
                        let (to, to_offset) = location(columns[parity].0, columns[parity].1, at)?;
                        repairs.push(Action::Xor {
                            from,
                            to,
                            to_offset,
                            len: l.interleave,
                        });
                    }
                }
            }
        }
        let what = if l.resiliency == Resiliency::Mirror {
            "1 MiB chunks whose copies differ"
        } else {
            "stripes whose parity does not match"
        };
        out.lines.push(format!(
            "{}: {mismatches} {what}, {unsettled} more where writes were under way",
            space.name()
        ));
        out.mismatches += mismatches;
        out.unsettled += unsettled;
        out.plan.step(format!("make space \"{}\" agree", space.name()), repairs);
    }
    Ok(out)
}
