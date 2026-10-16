//! What the guard makes of the copies of the pool database and of records
//! that contradict the pool: copies of one sequence that differ in what they
//! say (not only in their bytes) are suspect unless most copies hold the
//! version read; one copy has nothing to be compared with; an extent on a
//! disk the database does not list, one disk given twice, and two records
//! with one id are suspect. (1.2.1 called all of them healthy: a zeroed
//! sector of the first copy of `nocache` read other data, its warning said
//! Windows rewrites the copies.)

use std::fs::File;
use std::path::Path;

use storage_spaces::format::{Record, read_database};
use storage_spaces::gpt::Partition;
use storage_spaces::gpt::find_spaces_partition;
use storage_spaces::guard::{self, Options};
use storage_spaces::io::{ReadAt, SparseImage};
use storage_spaces::report::{Check, Report, Status, Verdict};
use storage_spaces::{Issue, Pool, SpaceReader};

fn load(name: &str) -> Vec<SparseImage> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    (0..)
        .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
        .map(|f| SparseImage::read_from(f).unwrap())
        .collect()
}

fn no_fs<D: ReadAt>(_: &SpaceReader<'_, D>, _: &Partition, _: &[String]) -> Vec<Check> {
    Vec::new()
}

fn report(disks: &[SparseImage]) -> Report {
    let pool = Pool::open(disks.to_vec()).unwrap();
    let names: Vec<String> = (0..disks.len()).map(|i| format!("disk{i}")).collect();
    guard::pool_reports(&pool, &names, &Options::default(), &mut no_fs).remove(0)
}

fn check<'r>(r: &'r Report, id: &str) -> &'r Check {
    r.checks.iter().find(|c| c.id == id).unwrap()
}

/// Where the copy of the pool database of `disk` is, if it has one.
fn copy_at(disk: &SparseImage) -> Option<u64> {
    let at = find_spaces_partition(disk).ok()??.offset + 0x1000;
    matches!(read_database(disk, at), Ok(Some(_))).then_some(at)
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().unwrap())
}

/// Where the body of record `id` starts in the copy at `at` of `disk` (its
/// first fragment), and the bytes of that fragment.
fn body_of(disk: &SparseImage, at: u64, id: u32) -> (u64, Vec<u8>) {
    let mut head = vec![0u8; 0x200];
    disk.read_exact_at(&mut head, at).unwrap();
    let (size, count) = (be32(&head[0x24..]) as u64, be32(&head[0x28..]) as u64);
    for k in 8..count {
        let mut e = vec![0u8; size as usize];
        disk.read_exact_at(&mut e, at + k * size).unwrap();
        if &e[..4] == b"SDBB" && be32(&e[8..]) == id && e[12..14] == [0, 0] {
            return (at + k * size + 16 + 8, e[16 + 8..].to_vec());
        }
    }
    panic!("no record {id}");
}

/// The places of the varints of a record body from `from` on, as
/// (offset, length): a length byte, then that many bytes, big-endian.
fn varints(body: &[u8], mut from: usize, n: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for _ in 0..n {
        let len = body[from] as usize;
        out.push((from + 1, len));
        from += 1 + len;
    }
    out
}

/// Sets the one-byte varint `which` (counted from 0, after `skip` leading
/// bytes) of record `id` to `value`, in every copy.
fn set_varint(disks: &mut [SparseImage], id: u32, skip: usize, which: usize, value: u8) {
    for disk in disks.iter_mut() {
        let Some(at) = copy_at(disk) else { continue };
        let (body_at, body) = body_of(disk, at, id);
        let (pos, len) = varints(&body, skip, which + 1)[which];
        assert_eq!(len, 1, "record {id}: varint {which} of {len} bytes");
        disk.insert(body_at + pos as u64, &[value]);
    }
}

/// The records of the database `disks` hold.
fn records(disks: &[SparseImage]) -> Vec<storage_spaces::format::RawRecord> {
    let disk = disks.iter().find(|d| copy_at(d).is_some()).unwrap();
    read_database(disk, copy_at(disk).unwrap()).unwrap().unwrap().1
}

/// A sector of the first copy zeroed, in turn, until the copies of one
/// sequence disagree in what they say: with two copies (one each) that is
/// suspect; with five (four hold the version read) a warning.
#[test]
fn copies_that_disagree_in_what_they_say() {
    for (name, expected) in [("nocache", Status::Suspect), ("mirror3", Status::Warning)] {
        let disks = load(name);
        let at = copy_at(&disks[0]).unwrap();
        let mut head = vec![0u8; 0x200];
        disks[0].read_exact_at(&mut head, at).unwrap();
        let (size, count) = (be32(&head[0x24..]) as u64, be32(&head[0x28..]) as u64);
        let mut found = false;
        for sector in (8 * size..count * size).step_by(512) {
            let mut d = disks.clone();
            d[0].insert(at + sector, &[0u8; 512]);
            let Ok(pool) = Pool::open(d.clone()) else { continue };
            if !pool
                .issues
                .iter()
                .any(|i| matches!(i, Issue::TornCopy { differs: true, .. }))
            {
                continue;
            }
            let r = report(&d);
            let c = check(&r, "pool.database");
            assert_eq!(c.status, expected, "{name}, sector {sector:#x}\n{}", r.to_text());
            found = true;
            break;
        }
        assert!(found, "{name}: no sector changes what the copy says");
    }
    // The sequence a disk or space record was written at, changed in one
    // copy: only the bytes differ, not what the copy says.
    let mut disks = load("mirror3");
    let at = copy_at(&disks[0]).unwrap();
    let (record, pos) = records(&disks)
        .into_iter()
        .filter(|r| matches!(r.kind, 2 | 3))
        .find_map(|r| {
            let (pos, len) = varints(&r.body, 0, 2)[1];
            (len == 1).then_some((r.id, pos))
        })
        .expect("a record with a one-byte sequence");
    let (body_at, body) = body_of(&disks[0], at, record);
    disks[0].insert(body_at + pos as u64, &[body[pos] ^ 1]);
    let r = report(&disks);
    let c = check(&r, "pool.database");
    assert_eq!(c.status, Status::Warning, "{}", r.to_text());
    assert!(
        c.evidence[0].found.as_deref().unwrap().contains("only in its bytes"),
        "{c:?}"
    );
    assert!(!c.advice.as_deref().unwrap().contains("rewrites the others"), "{c:?}");
}

#[test]
fn one_copy_has_nothing_to_compare() {
    let r = report(&load("simple1c"));
    let c = check(&r, "pool.database");
    assert_eq!(c.status, Status::Info, "{}", r.to_text());
    assert!(c.summary.contains("nothing to compare"), "{}", c.summary);
}

/// An extent naming a disk the database does not list (one bit of its disk
/// id flipped, as in `m4kn`): suspect when the pool opens, not a read
/// error later.
#[test]
fn an_extent_on_an_unknown_disk_is_suspect() {
    let mut disks = load("m4kn");
    let extent = records(&disks)
        .into_iter()
        .find(|r| matches!(Record::decode(r), Ok(Record::Extent(e)) if e.space_id > 2))
        .unwrap();
    let Ok(Record::Extent(e)) = Record::decode(&extent) else {
        unreachable!()
    };
    // Three varints, the flags, then slab count, space, virtual slab,
    // column, copy, stale marker, disk.
    let body = &extent.body;
    let skip = varints(body, 0, 3).last().map(|&(p, l)| p + l).unwrap() + 1;
    set_varint(&mut disks, extent.id, skip, 6, (e.disk_id as u8) | 0x80);
    let r = report(&disks);
    let c = check(&r, "pool.clean");
    assert_eq!(c.status, Status::Suspect, "{}", r.to_text());
    assert!(
        c.evidence
            .iter()
            .any(|e| e.found.as_deref().unwrap().contains("which it does not list")),
        "{c:?}"
    );
}

/// One disk given twice: suspect, naming both devices.
#[test]
fn one_disk_twice_is_suspect() {
    let disks = load("mirror2");
    let twice = vec![disks[0].clone(), disks[0].clone(), disks[1].clone()];
    let r = report(&twice);
    let c = check(&r, "pool.clean");
    assert_eq!(c.status, Status::Suspect, "{}", r.to_text());
    let found = c.evidence[0].found.as_deref().unwrap();
    assert!(found.contains("disk0 and disk1"), "{found}");
    assert!(r.verdict() >= Verdict::Suspect);
}

/// Two space records with one id: suspect (the last read).
#[test]
fn two_records_with_one_id_are_suspect() {
    let mut disks = load("multi");
    let spaces: Vec<_> = records(&disks)
        .into_iter()
        .filter(|r| r.kind == 3)
        .filter_map(|r| match Record::decode(&r) {
            Ok(Record::Space(s)) if s.id < 0x80 => Some((r.id, s.id)),
            _ => None,
        })
        .collect();
    assert!(spaces.len() >= 2, "{spaces:?}");
    let (record, _) = spaces[1];
    set_varint(&mut disks, record, 0, 0, spaces[0].1 as u8);
    let pool = Pool::open(disks.clone()).unwrap();
    assert!(
        pool.issues
            .iter()
            .any(|i| matches!(i, Issue::DuplicateRecord { kind: "space", .. })),
        "{:?}",
        pool.issues
    );
    let r = report(&disks);
    assert_eq!(check(&r, "pool.clean").status, Status::Suspect, "{}", r.to_text());
}
