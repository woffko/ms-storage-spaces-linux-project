//! Predictions of what Windows writes, checked against the states of the
//! scenarios in tools/scenarios.sh (fixtures captured from the snapshots
//! tools/vm/Invoke-Scenario.ps1 took between the steps).

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

use storage_spaces::Pool;
use storage_spaces::cache::CacheWriter;
use storage_spaces::database::Database;
use storage_spaces::drt::{DirtyRegions, DrtWriter};
use storage_spaces::format::{
    ExtentRecord, POOL_DB_OFFSET, Record, SLAB_SIZE, SPACE_SECURITY_DESCRIPTOR, SpaceEdit, edit_space_record,
};
use storage_spaces::io::SparseImage;
use storage_spaces::journal::JournalWriter;

fn state(scenario: &str, label: &str) -> Pool<SparseImage> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/scenarios")
        .join(scenario)
        .join(label);
    let mut disks = Vec::new();
    while let Ok(f) = File::open(dir.join(format!("disk{}.fixture", disks.len()))) {
        disks.push(SparseImage::read_from(f).unwrap());
    }
    Pool::open(disks).unwrap()
}

/// The dirty region log of the only user space, the size of its tracking
/// space (the second header copy sits 8 KiB before its end) and the virtual
/// slab where the extent run holding each byte offset starts.
fn log(pool: &Pool<SparseImage>) -> (DirtyRegions, u64, impl Fn(u64) -> u64 + '_) {
    let space = pool.user_spaces().next().unwrap();
    let reader = pool.open_space(space.id()).unwrap();
    let log = reader.dirty_regions().unwrap().clone();
    let size = log.copies()[1].offset + 0x2000;
    let layout = reader.layout().clone();
    let run_of = move |offset: u64| layout.run_start_offset(layout.locate(offset).row) / SLAB_SIZE;
    (log, size, run_of)
}

/// The steps of a scenario with their start in seconds of the day (UTC).
fn steps(scenario: &str) -> Vec<(String, f64)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/scenarios")
        .join(scenario)
        .join("scenario.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let steps: Vec<(String, f64)> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let t = &e["started"].as_str().unwrap()[11..];
            let secs = t[..2].parse::<f64>().unwrap() * 3600.0
                + t[3..5].parse::<f64>().unwrap() * 60.0
                + t[6..].split('+').next().unwrap().parse::<f64>().unwrap();
            (e["step"].as_str().unwrap().to_string(), secs)
        })
        .collect();
    assert!(
        steps.windows(2).all(|w| w[0].1 <= w[1].1),
        "{scenario} crosses midnight"
    );
    steps
}

/// Seconds without writes after which Windows leaves a run out of the next
/// header it writes: runs idle for 29 s were kept (m5drt), runs idle for
/// 35 s dropped (m5drt2).
const CLEAN_AFTER: f64 = 32.0;

/// Replays the writes, disconnects and reconnects of a scenario on
/// [`DrtWriter`], the model of the mirror dirty region log, and compares
/// both header pages with every snapshot, byte for byte. `first` is the
/// snapshot to start from, or `None` for a new space. Returns the number of
/// snapshots compared.
fn replay_dirty_region_log(scenario: &str, first: Option<&str>) -> usize {
    let steps = steps(scenario);
    let label = |s: &str| {
        s.strip_prefix("snap:")
            .map(|l| l.split(':').next().unwrap().to_string())
    };
    let any = steps.iter().find_map(|(s, _)| label(s)).unwrap();
    let pool = state(scenario, &any);
    let (_, size, run_of) = log(&pool);
    let mut writer = match first {
        Some(l) => log(&state(scenario, l)).0.writer(),
        None => DrtWriter::new(),
    };
    let mut pages = [writer.page(), writer.page()];
    if let Some(l) = first {
        let (disk, _, _) = log(&state(scenario, l));
        pages = [disk.copies()[0].page.clone(), disk.copies()[1].page.clone()];
    }
    let mut last_write: HashMap<u64, f64> = HashMap::new();
    let mut checked = 0;
    let mut started = first.is_none();
    for (step, at) in &steps {
        let a: Vec<&str> = step.split(':').collect();
        if !started {
            started = label(step).as_deref() == first;
            continue;
        }
        match a[0] {
            "write" => {
                let offset = a[2].parse::<u64>().unwrap() * 1024;
                let len = a[3].parse::<u64>().unwrap() * 1024;
                for run in run_of(offset)..=run_of(offset + len - 1) {
                    if !writer.runs().contains(&run) {
                        writer.clean(|r| last_write.get(&r).is_some_and(|&t| at - t > CLEAN_AFTER));
                        let (at_end, page) = writer.write(run).unwrap();
                        pages[usize::from(at_end)] = page;
                    }
                    last_write.insert(run, *at);
                }
            }
            "disconnect" => {
                let page = writer.disconnect();
                pages = [page.clone(), page];
                last_write.clear();
            }
            // Attaching loads the log from the newest copy.
            "connect" => {
                writer = DirtyRegions::load(size, |off, buf| {
                    buf.copy_from_slice(&pages[usize::from(off != 0)]);
                    Ok(())
                })
                .unwrap()
                .unwrap()
                .writer();
            }
            "snap" => {
                let (windows, _, _) = log(&state(scenario, a[1]));
                for (i, c) in windows.copies().iter().enumerate() {
                    assert!(
                        c.page == pages[i],
                        "{scenario} {}: copy at {:#x}\n Windows {}\n model   {}",
                        a[1],
                        c.offset,
                        hex(&c.page[..0x40]),
                        hex(&pages[i][..0x40])
                    );
                }
                checked += 1;
            }
            _ => {}
        }
    }
    checked
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// m5drt: a two-way mirror of four 256 MiB extent runs written in the order
/// 0, 2, 0, 1, 3, then disconnected, reconnected and written in run 1.
/// m5drt2: writes into runs 0 to 6 after 5 to 180 s without writes.
/// Windows wrote exactly the header pages the model predicts, stale entries
/// included: the first write into a run adds it in the next generation (odd
/// generations at the end copy, even ones at the start), after the runs idle
/// for longer than about 30 s were removed; a write into a listed run
/// changes nothing; a disconnect removes every run and writes generation 0
/// into both copies; attaching loads the listed runs of the newest copy.
#[test]
fn mirror_dirty_region_log_follows_the_writes() {
    assert_eq!(replay_dirty_region_log("m5drt", Some("s0")), 7);
    assert_eq!(replay_dirty_region_log("m5drt2", None), 7);
}

/// The pool database copy of every member disk of a scenario state.
fn databases(scenario: &str, label: &str) -> Vec<Database> {
    let pool = state(scenario, label);
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/scenarios")
        .join(scenario)
        .join(label);
    pool.members
        .iter()
        .map(|m| {
            let f = File::open(dir.join(format!("disk{}.fixture", m.device))).unwrap();
            let disk = SparseImage::read_from(f).unwrap();
            Database::read(&disk, m.partition.offset + POOL_DB_OFFSET, 64).unwrap()
        })
        .collect()
}

/// Checks that every member of `label` carries `predicted`.
fn assert_database(predicted: &Database, scenario: &str, label: &str) {
    for (i, windows) in databases(scenario, label).iter().enumerate() {
        let diff: Vec<usize> = (0..predicted.bytes().len())
            .filter(|&k| predicted.bytes()[k] != windows.bytes()[k])
            .collect();
        assert!(diff.is_empty(), "{scenario} {label} device {i}: bytes {diff:x?} differ");
    }
}

/// m5db: a simple space renamed, a second space created, the first one
/// extended, the second one deleted, the pool set read-only and writable.
/// Every member's pool database matches the model byte for byte: each
/// update writes new record versions into the first free slots long enough
/// (the old versions still occupying theirs), then frees the old versions,
/// and commits with the next sequence. Changed space records carry the new
/// sequence and, from the first change on, the default security
/// descriptor; extent records carry the sequence they were written at. The
/// read-only flag of the pool is not stored. Unpredictable inputs taken from
/// Windows: the timestamps, and the record of the new space (GUID, id).
#[test]
fn pool_database_updates_follow_the_model() {
    let dbs: Vec<Vec<Database>> = (0..7).map(|i| databases("m5db", &format!("s{i}"))).collect();
    let ts = |i: usize| dbs[i][0].timestamp();
    let extent = |space_id, virtual_slab, column, slab_count, disk_id, physical_slab| ExtentRecord {
        space_id,
        virtual_slab,
        column,
        copy: 0,
        slab_count,
        disk_id,
        physical_slab,
        flags: 0,
        stale_marker: 0xffff_ffff,
    };

    // s0 -> s1: rename m5db to m5dbx.
    let mut db = dbs[0][0].clone();
    let old = db.record(21).unwrap();
    let body = edit_space_record(
        &old.body,
        &SpaceEdit {
            sequence: 3,
            name: Some("m5dbx"),
            security_descriptor: Some(&SPACE_SECURITY_DESCRIPTOR),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(db.update(&[(3, 16, &body)], &[21]), Some(vec![27]));
    db.commit(3, ts(1));
    assert_database(&db, "m5db", "s1");

    // s1 -> s2: a new 2-column space of 1 GiB allocation units.
    let new = dbs[2][0].record(21).unwrap();
    let writes = [
        (3, 16, new.body.clone()),
        (4, 6, extent(37, 0, 0, 4, 1, 3).encode(4)),
        (4, 6, extent(37, 0, 1, 4, 2, 3).encode(4)),
    ];
    let writes: Vec<(u8, u8, &[u8])> = writes.iter().map(|(k, v, b)| (*k, *v, b.as_slice())).collect();
    assert_eq!(db.update(&writes, &[]), Some(vec![21, 32, 33]));
    db.commit(4, ts(2));
    assert_database(&db, "m5db", "s2");

    // s2 -> s3: m5dbx extended from 1 to 1.5 GiB by one row.
    let old = db.record(27).unwrap();
    let body = edit_space_record(
        &old.body,
        &SpaceEdit {
            sequence: 5,
            size: Some(0x6000_0000),
            ..Default::default()
        },
    )
    .unwrap();
    let writes = [
        (3, 16, body),
        (4, 6, extent(5, 4, 0, 1, 1, 7).encode(5)),
        (4, 6, extent(5, 4, 1, 1, 2, 7).encode(5)),
    ];
    let writes: Vec<(u8, u8, &[u8])> = writes.iter().map(|(k, v, b)| (*k, *v, b.as_slice())).collect();
    assert_eq!(db.update(&writes, &[27]), Some(vec![34, 39, 40]));
    db.commit(5, ts(3));
    assert_database(&db, "m5db", "s3");

    // s3 -> s4: the new space deleted.
    assert_eq!(db.update(&[], &[21, 32, 33]), Some(vec![]));
    db.commit(6, ts(4));
    assert_database(&db, "m5db", "s4");

    // s4 -> s5 -> s6: read-only and writable again change nothing.
    assert_database(&db, "m5db", "s5");
    assert_database(&db, "m5db", "s6");
}

/// Replays slab allocations of a thin space: each allocation is one update
/// of the pool database with one extent record per copy, on the disks
/// Windows chose (the input) and at the first free slab of each; the
/// database of every state must match byte for byte.
/// A state and the allocations before it: (virtual slab, disk per copy).
type Allocations<'a> = (&'a str, &'a [(u64, &'a [u64])]);

fn replay_allocations(scenario: &str, space_id: u64, steps: &[Allocations]) {
    let first = databases(scenario, "s0");
    let mut db = first[0].clone();
    let mut sequence = db.sequence();
    for (label, allocations) in steps {
        for (virtual_slab, disks) in *allocations {
            sequence += 1;
            let bodies: Vec<Vec<u8>> = disks
                .iter()
                .enumerate()
                .map(|(copy, &disk_id)| {
                    ExtentRecord {
                        space_id,
                        virtual_slab: *virtual_slab,
                        column: 0,
                        copy: copy as u64,
                        slab_count: 1,
                        disk_id,
                        physical_slab: db.first_free_slab(disk_id),
                        flags: 0,
                        stale_marker: 0xffff_ffff,
                    }
                    .encode(sequence)
                })
                .collect();
            let writes: Vec<(u8, u8, &[u8])> = bodies.iter().map(|b| (4, 6, b.as_slice())).collect();
            db.update(&writes, &[]).unwrap();
        }
        if !allocations.is_empty() {
            let windows = databases(scenario, label);
            db.commit(sequence, windows[0].timestamp());
        }
        assert_database(&db, scenario, label);
    }
}

/// m5thin: a thin simple space on three disks written at 2 GiB, 0, 3 GiB,
/// 256 MiB and then 1 GiB from 1 GiB on (256 MiB slabs). m5thinm: the same
/// as a two-way mirror. Windows allocates a slab (every copy) per database
/// update when a write first reaches it, in the order of the write, at the
/// first free slab of the disks it picks; the write at 0 found its slab
/// allocated with the space. Which disks it picks does not follow from the
/// metadata (not the emptiest: m5thin put the slab at 3 GiB on disk 3 while
/// disk 2 had two slabs fewer), so the model takes them from Windows.
#[test]
fn thin_slabs_are_allocated_per_update_at_the_first_free_slab() {
    replay_allocations(
        "m5thin",
        6,
        &[
            ("s1", &[(8, &[3])]),
            ("s2", &[]),
            ("s3", &[(12, &[3])]),
            ("s4", &[(1, &[2])]),
            ("s5", &[(4, &[2]), (5, &[1]), (6, &[2]), (7, &[2])]),
        ],
    );
    replay_allocations(
        "m5thinm",
        6,
        &[
            ("s1", &[(8, &[1, 3])]),
            ("s2", &[]),
            ("s3", &[(12, &[1, 3])]),
            ("s4", &[(4, &[1, 3]), (5, &[1, 2]), (6, &[2, 1]), (7, &[2, 3])]),
        ],
    );
}

/// m5stale: a two-way mirror on two disks, written, one disk detached and
/// the space written in two runs, the disk attached again, the space
/// repaired. While the disk is away Windows writes nothing to the pool
/// database (copies that miss writes are only known in memory and from the
/// dirty region log). When the disk returns, it alone gets an update: the
/// extent of the copy Windows lists as needing reallocation (copy 0 of the
/// first row, written while its disk was away) rewritten unchanged with the
/// new sequence; the other member keeps the older database until the repair
/// writes the same one there.
#[test]
fn a_returning_disk_is_updated_first_and_the_others_at_repair() {
    let s1 = databases("m5stale", "s1");
    for label in ["s2", "s3"] {
        assert_database(&s1[0], "m5stale", label);
    }
    let mut db = s1[0].clone();
    let Ok(Record::Extent(extent)) = Record::decode(&db.record(29).unwrap()) else {
        panic!("record 29 is no extent")
    };
    assert_eq!((extent.virtual_slab, extent.copy, extent.disk_id), (0, 0, 2));
    assert_eq!(db.update(&[(4, 6, &extent.encode(3))], &[29]), Some(vec![37]));
    let s4 = databases("m5stale", "s4");
    db.commit(3, s4[1].timestamp());
    assert_eq!(s4[0].bytes(), s1[0].bytes(), "the member that stayed");
    assert_eq!(s4[1].bytes(), db.bytes(), "the returning member");
    assert_database(&db, "m5stale", "s5");
}

/// The valid slots of the cache of the only user space: (index, content from
/// the type field on, without trailing zeros).
fn cache_slots(pool: &Pool<SparseImage>) -> Vec<(usize, Vec<u8>)> {
    let space = pool.user_spaces().next().unwrap();
    let reader = pool.open_space(space.id()).unwrap();
    reader
        .cache()
        .unwrap()
        .slots()
        .iter()
        .map(|s| (s.index, s.content.clone()))
        .collect()
}

fn content(page: &[u8]) -> Vec<u8> {
    let end = page.iter().rposition(|&b| b != 0).map_or(0x20, |p| p + 1).max(0x20);
    page[0x20..end].to_vec()
}

/// m5wbc and m5pj: a 3-column parity space with the default 1 GiB
/// write-back cache (Windows creates it even when asked for none), written
/// 4 KiB at 0, 64 KiB at 1 MiB, 4 KiB at 0 again after 30 s, and (m5pj)
/// 512 KiB at 0. Replayed on [`CacheWriter`], every slot Windows wrote
/// matches byte for byte: the first write into a chunk takes the next block
/// (from 64) and the next slot with the next sequence, a write into cached
/// sectors writes nothing, filling a chunk writes it as full (with the run
/// word Windows leaves behind the entry). Nothing was moved out of the
/// cache within two minutes.
#[test]
fn cache_log_follows_the_writes() {
    for scenario in ["m5wbc", "m5pj"] {
        let first = state(scenario, "s0");
        let space = first.user_spaces().next().unwrap();
        let header = first.open_space(space.id()).unwrap().cache().unwrap().header.clone();
        let mut writer = CacheWriter::new(header, 64);
        let mut expected = vec![(0, content(&writer.init_slot()))];
        let mut checked = 0;
        for (step, _) in steps(scenario) {
            let a: Vec<&str> = step.split(':').collect();
            match a[0] {
                "write" => {
                    let offset = a[2].parse::<u64>().unwrap() * 1024;
                    let len = a[3].parse::<u64>().unwrap() * 1024;
                    for (index, page) in writer.write(offset, len) {
                        expected.push((index, content(&page)));
                    }
                }
                "snap" => {
                    assert_eq!(cache_slots(&state(scenario, a[1])), expected, "{scenario} {}", a[1]);
                    checked += 1;
                }
                _ => {}
            }
        }
        assert!(checked >= 5, "{scenario}");
    }
}

/// m5pj2: a 3-column parity space (stripes of 512 KiB of data) written
/// 4 MiB at 0, 512 KiB at 8 MiB and 4 MiB at 16 MiB, then 4 KiB at 0, in
/// requests of at most 1 MiB. Each request of whole stripes bypasses the
/// cache and gets a parity journal slot ([`JournalWriter`]); the 4 KiB write
/// goes to the cache ([`CacheWriter`]). Every journal and cache slot of every
/// state matches byte for byte; a minute without writes changed nothing.
#[test]
fn parity_journal_logs_whole_stripe_writes() {
    let scenario = "m5pj2";
    let first = state(scenario, "s0");
    let space = first.user_spaces().next().unwrap();
    let reader = first.open_space(space.id()).unwrap();
    let layout = reader.layout().clone();
    let stripe = layout.data_columns * layout.interleave;
    let stripes = reader.size() / stripe;
    assert_eq!(stripes, 4096);
    let mut journal = JournalWriter::new(space.info.guid, 4096, 1024);
    let mut cache = CacheWriter::new(reader.cache().unwrap().header.clone(), 64);
    let mut journal_slots: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut cached = vec![(0, content(&cache.init_slot()))];
    let mut checked = 0;
    for (step, _) in steps(scenario) {
        let a: Vec<&str> = step.split(':').collect();
        match a[0] {
            "write" => {
                let offset = a[2].parse::<u64>().unwrap() * 1024;
                let len = a[3].parse::<u64>().unwrap() * 1024;
                let mut at = offset;
                while at < offset + len {
                    let n = (offset + len - at).min(1 << 20);
                    if at % stripe == 0 && n % stripe == 0 {
                        let (i, page) = journal.write(0, stripes, at / stripe, n / stripe);
                        journal_slots.push((i, content(&page)));
                    } else {
                        for (i, page) in cache.write(at, n) {
                            cached.push((i, content(&page)));
                        }
                    }
                    at += n;
                }
            }
            "snap" => {
                let pool = state(scenario, a[1]);
                let space = pool.user_spaces().next().unwrap();
                let reader = pool.open_space(space.id()).unwrap();
                let windows: Vec<(usize, Vec<u8>)> = reader.journal().map_or(Vec::new(), |j| {
                    j.slots().iter().map(|s| (s.index, s.content.clone())).collect()
                });
                assert_eq!(windows, journal_slots, "{scenario} {} journal", a[1]);
                assert_eq!(cache_slots(&pool), cached, "{scenario} {} cache", a[1]);
                checked += 1;
            }
            _ => {}
        }
    }
    assert_eq!(checked, 6);
}
