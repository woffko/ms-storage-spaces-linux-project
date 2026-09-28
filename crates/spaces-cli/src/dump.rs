//! `spaces dump` and `spaces diff`: the pool metadata as text, one
//! self-contained fact per line, so that two states of a pool can be
//! compared line by line.

use std::collections::HashMap;
use std::fmt::Write as _;

use storage_spaces::format::{DbHeader, RawRecord, Record};
use storage_spaces::io::ReadAt;
use storage_spaces::{OpenOptions, Pool};

/// The metadata of `pool` as lines.
pub fn dump<D: ReadAt>(pool: &Pool<D>) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = |s: String| out.push(s);
    line(format!(
        "pool {} {:?} version {} sectors {}/{}",
        pool.guid, pool.name, pool.version, pool.logical_sector_size, pool.physical_sector_size
    ));
    for w in &pool.warnings {
        line(format!("warning {w}"));
    }

    // Every copy of the pool database, grouped into identical versions.
    let mut versions: Vec<(DbHeader, Vec<RawRecord>, Vec<usize>)> = Vec::new();
    for m in &pool.members {
        line(format!(
            "member {} disk {} partition {:#x}+{:#x} header version {} formatted {:#x}",
            m.device,
            m.header.disk_guid,
            m.partition.offset,
            m.partition.length,
            m.header.version,
            m.header.format_time
        ));
        match pool.database_copy(m.device) {
            Ok(None) => line(format!("database device {} none", m.device)),
            Err(e) => line(format!("database device {} unreadable: {e}", m.device)),
            Ok(Some((header, records))) => {
                line(format!(
                    "database device {} sequence {} timestamp {:#x} entries {}x{:#x} digest {:016x}",
                    m.device,
                    header.sequence,
                    header.timestamp,
                    header.entry_count,
                    header.entry_size,
                    digest(&records)
                ));
                match versions
                    .iter_mut()
                    .find(|(h, r, _)| h.sequence == header.sequence && *r == records)
                {
                    Some((_, _, devs)) => devs.push(m.device),
                    None => versions.push((header, records, vec![m.device])),
                }
            }
        }
    }

    // The records of the version the pool uses, then how the others differ.
    let chosen = versions
        .iter()
        .filter(|v| v.0.sequence == pool.database.sequence)
        .max_by_key(|v| v.2.len());
    if let Some(chosen) = chosen {
        let records = &chosen.1;
        for r in records {
            line(format!(
                "record {} type {} v{} {}",
                r.id,
                r.kind,
                r.version,
                hex(&r.body)
            ));
            match Record::decode(r) {
                Ok(Record::Other { .. }) => {}
                Ok(decoded) => line(format!("record {} {decoded:?}", r.id)),
                Err(e) => line(format!("record {} undecodable: {e}", r.id)),
            }
        }
        for (header, other, devs) in versions.iter().filter(|v| !std::ptr::eq(*v, chosen)) {
            let tag = format!("database sequence {} on devices {devs:?}", header.sequence);
            let mine: HashMap<u32, &RawRecord> = records.iter().map(|r| (r.id, r)).collect();
            let theirs: HashMap<u32, &RawRecord> = other.iter().map(|r| (r.id, r)).collect();
            for r in other {
                match mine.get(&r.id) {
                    Some(m) if *m == r => {}
                    _ => line(format!(
                        "{tag} record {} type {} v{} {}",
                        r.id,
                        r.kind,
                        r.version,
                        hex(&r.body)
                    )),
                }
            }
            for r in records.iter().filter(|r| !theirs.contains_key(&r.id)) {
                line(format!("{tag} lacks record {}", r.id));
            }
        }
    }

    // The logs of each user space.
    for space in pool.user_spaces() {
        let tag = format!("space {} {:?}", space.id(), space.name());
        let reader = match pool.open_space_with(space.id(), OpenOptions::default()) {
            Ok(r) => r,
            Err(e) => {
                line(format!("{tag} cannot be opened: {e}"));
                continue;
            }
        };
        if let Some(d) = reader.dirty_regions() {
            for c in d.copies() {
                match &c.header {
                    Some(h) => line(format!(
                        "{tag} dirty regions at {:#x} generation {} runs {:?}",
                        c.offset, h.generation, h.runs
                    )),
                    None => line(format!("{tag} dirty regions at {:#x} invalid", c.offset)),
                }
            }
        }
        if let Some(j) = reader.journal() {
            for (offset, versions) in j.entries() {
                line(format!("{tag} journal run {offset:#x} {versions:?}"));
            }
        }
        if let Some(c) = reader.cache() {
            line(format!("{tag} cache {:?}", c.header));
            if c.conflicting_chunks() > 0 {
                line(format!("{tag} cache conflicting chunks {}", c.conflicting_chunks()));
            }
            for (offset, block, valid) in c.mappings() {
                line(format!("{tag} cache chunk {offset:#x} block {block} {valid:?}"));
            }
        }
    }
    out
}

/// Lines only in `old` (prefixed "-") and only in `new` ("+"), each in its
/// own order. The dump lines state self-contained facts, so this multiset
/// difference reads like a diff without needing context lines.
pub fn diff(old: &[String], new: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for (sign, a, b) in [('-', old, new), ('+', new, old)] {
        let mut other: HashMap<&str, usize> = HashMap::new();
        for l in b {
            *other.entry(l.as_str()).or_default() += 1;
        }
        for l in a {
            match other.get_mut(l.as_str()) {
                Some(n) if *n > 0 => *n -= 1,
                _ => out.push(format!("{sign} {l}")),
            }
        }
    }
    out
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

/// FNV-1a over the records, to tell database copies apart at a glance.
fn digest(records: &[RawRecord]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for r in records {
        let head = [r.id.to_le_bytes().as_slice(), &[r.kind, r.version]].concat();
        for &b in head.iter().chain(&r.body) {
            h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::path::Path;
    use storage_spaces::io::SparseImage;

    fn fixture(name: &str, disks: usize) -> Pool<SparseImage> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../storage-spaces/tests/fixtures")
            .join(name);
        let disks = (0..disks)
            .map(|i| SparseImage::read_from(File::open(dir.join(format!("disk{i}.fixture"))).unwrap()).unwrap())
            .collect();
        Pool::open(disks).unwrap()
    }

    #[test]
    fn dump_shows_database_copies_records_and_logs() {
        let lines = dump(&fixture("mirror2", 2));
        let has = |prefix: &str| lines.iter().any(|l| l.starts_with(prefix));
        assert!(has(
            "pool b0b2e63a-06d6-4f36-9e01-613cbec142b1 \"ss-mirror2\" version 29"
        ));
        assert!(has("database device 0 sequence 2 "));
        assert!(has("database device 1 sequence 2 "));
        assert!(has("record 8 Pool(PoolRecord {"));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Space(SpaceRecord { id: 5,") && l.contains("name: \"mirror2\""))
        );
        assert!(lines.contains(&"space 5 \"mirror2\" dirty regions at 0xfffe000 generation 1 runs [0]".to_string()));
        // Both members carry the same copy, so no other version is listed.
        assert!(!has("database sequence"));
        assert!(diff(&lines, &lines).is_empty());
    }

    #[test]
    fn diff_lists_lines_of_one_side_only() {
        let s = |v: &[&str]| v.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        let old = s(&["a", "b", "b", "c"]);
        let new = s(&["b", "c", "d"]);
        assert_eq!(diff(&old, &new), ["- a", "- b", "+ d"]);
        assert!(diff(&old, &old).is_empty());
    }
}
