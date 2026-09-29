//! What Windows did with pools whose mirror copies were made to differ
//! (tools/vm/Test-RoundTrip.ps1 on copies of batch 9 pools; the evidence in
//! tests/evidence was recorded from its output and from the disks
//! fetched back afterwards). It backs how the reader treats mirror rows of
//! extent runs the dirty region log lists.

use std::path::Path;

use serde_json::Value;

fn evidence(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/evidence").join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Copy 1 carried a different tag in the first block of every MiB. Windows
/// found both pools healthy and read copy 0 in every pass over drtdism (run
/// listed) but copy 1 in every probe pass over drtdisc (log empty), after
/// its own pattern check had read copy 0: it serves either copy, whatever
/// the log says. Neither pool's copies were changed.
#[test]
fn windows_reads_either_mirror_copy_and_keeps_both() {
    for (name, tag, served) in [
        ("e3drtdism.json", "drtdism", "drtdism"),
        ("e3drtdisc.json", "drtdisc", "COPY1"),
    ] {
        let e = evidence(name);
        assert_eq!(e["connected"]["space"], "Healthy", "{name}");
        assert_eq!(e["pattern_check_ok"], true, "{name}");
        for pass in e["probe"].as_array().unwrap() {
            assert_eq!(pass["counts"][served], 256, "{name}");
        }
        assert_eq!(e["copies_before"], e["copies_after"], "{name}");
        assert_eq!(e["copies_after"]["copy0"][tag], 256, "{name}");
        assert_eq!(e["copies_after"]["copy1"]["COPY1"], 256, "{name}");
    }
}

/// A differing block in a listed run survived 120 s attached and
/// Repair-VirtualDisk: Windows does not resynchronise the copies.
#[test]
fn repair_does_not_reconcile_mirror_copies() {
    let e = evidence("drte2.json");
    assert_eq!(e["states"]["repaired"]["space"], "Healthy");
    assert_eq!(e["states"]["repaired"]["jobs"][0], "drtdism-Repair:Completed");
    assert_eq!(e["block_8mib_before"], e["block_8mib_after"]);
    assert_ne!(e["block_8mib_after"]["copy0"], e["block_8mib_after"]["copy1"]);
}

/// A member device with some bytes replaced.
struct Patched {
    inner: storage_spaces::io::SparseImage,
    offset: u64,
    bytes: Vec<u8>,
}

impl storage_spaces::io::ReadAt for Patched {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        self.inner.read_exact_at(buf, offset)?;
        let (start, end) = (offset, offset + buf.len() as u64);
        let (p_start, p_end) = (self.offset, self.offset + self.bytes.len() as u64);
        for at in start.max(p_start)..end.min(p_end) {
            buf[(at - start) as usize] = self.bytes[(at - p_start) as usize];
        }
        Ok(())
    }

    fn size(&self) -> std::io::Result<u64> {
        self.inner.size()
    }
}

/// Diverging copies of the pool database (the m5db state s1, device 1's copy
/// changed, attached on Windows): with equal sequences (t1: the space named
/// m5dbT on device 1) Windows used device 0's copy and rewrote neither; a
/// newer copy that does not decode (t2: sequence 4, a record with an
/// inconsistent fragment) made Windows treat its disk as lost and write the
/// good copy again with sequence 5, above every copy it had seen. spaces
/// makes the same choice for t1.
#[test]
fn windows_resolves_diverging_database_copies() {
    let t1 = evidence("tornt1.json");
    assert_eq!(t1["connected"]["spaces"][0][0], "m5dbx");
    assert_eq!(t1["unchanged_after"], serde_json::json!([true, true]));
    let t2 = evidence("tornt2.json");
    assert_eq!(t2["attached"]["disks"][1][1], "Lost Communication");
    assert_eq!(t2["sequences_before"], serde_json::json!([3, 4]));
    assert_eq!(t2["sequences_after"], serde_json::json!([5, 4]));
    assert_eq!(t2["records_after_equal_device0_before"], true);

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios/m5db/s1");
    let disks: Vec<Patched> = (0..2)
        .map(|i| {
            let inner = storage_spaces::io::SparseImage::read_from(
                std::fs::File::open(dir.join(format!("disk{i}.fixture"))).unwrap(),
            )
            .unwrap();
            let offset = 0x100_1000;
            let mut bytes = vec![0u8; 0x1000];
            storage_spaces::io::ReadAt::read_exact_at(&inner, &mut bytes, offset).unwrap();
            if i == 1 {
                let name: Vec<u8> = "m5dbx".encode_utf16().flat_map(u16::to_be_bytes).collect();
                let at = bytes.windows(name.len()).position(|w| w == name).unwrap();
                let renamed: Vec<u8> = "m5dbT".encode_utf16().flat_map(u16::to_be_bytes).collect();
                bytes[at..at + renamed.len()].copy_from_slice(&renamed);
            }
            Patched { inner, offset, bytes }
        })
        .collect();
    let pool = storage_spaces::Pool::open(disks).unwrap();
    assert_eq!(pool.user_spaces().next().unwrap().name(), "m5dbx");
    assert!(pool.warnings.iter().any(|w| w.contains("torn")), "{:?}", pool.warnings);
}

/// Writes from Linux (SpaceWriter through spaces write-pattern) into a copy
/// of simple2c_26100: four ranges, across interleave boundaries and in the
/// second row. Windows found the space healthy before and after
/// Repair-VirtualDisk, and read exactly those ranges as written and the rest
/// of the 2 GiB unchanged (tools/rw-roundtrip.sh).
#[test]
fn windows_reads_what_linux_wrote_to_a_simple_space() {
    let e = evidence("rw-simple.json");
    assert_eq!(e["attached"][0][1], "Healthy");
    assert_eq!(e["repaired"][0][1], "Healthy");
    assert_eq!(e["pattern_ok"], true);
    assert_eq!(e["bytes_checked"], 2u64 << 30);
    assert_eq!(e["written_from_linux"].as_str().unwrap().split(';').count(), 4);
}

/// NTFS written from Linux into a copy of simple2c_26100 through ublk
/// (tools/rw-ntfs-check.sh: GPT and mkntfs, a stress run of 3000 seeded
/// file operations checked against a model, then tools/work-roundtrip.sh):
/// with ntfs-3g, and with ntfs3 leaving out truncation, Windows found the
/// space healthy, chkdsk clean and every file intact. With truncation,
/// ntfs3 (Linux 6.8) left four small files truncated to zero that chkdsk
/// reports as corrupt; the same run on a plain disk image gave the same
/// four records, so it is the file system driver, not the space.
#[test]
fn windows_accepts_ntfs_written_from_linux_to_a_simple_space() {
    for name in ["rw-ntfs3.json", "rw-ntfs3g.json"] {
        let e = evidence(name);
        assert_eq!(e["attached"][0][1], "Healthy", "{name}");
        assert_eq!(e["repaired"][0][1], "Healthy", "{name}");
        assert_eq!(e["chkdsk_exit"], 0, "{name}");
        assert!(e["files"].as_u64().unwrap() > 500, "{name}");
        assert_eq!(e["mismatching"].as_array().unwrap().len(), 0, "{name}");
    }
    let e = evidence("rw-ntfs3-truncate.json");
    assert_eq!(e["corrupt_records"], serde_json::json!(["39", "B6", "1A4", "258"]));
    assert_eq!(e["mismatching"].as_array().unwrap().len(), 4);
}

/// A two-way mirror written from Linux (mirror2_26100 copies): NTFS with
/// ntfs-3g and 3000 file operations (Windows: healthy, chkdsk clean, all
/// files intact), and a write cut off before its 4th member write, leaving
/// one 256 KiB unit on copy 0 only inside a listed run: Windows attached it
/// as healthy before and after repair (and spaces refuses that row).
#[test]
fn windows_accepts_mirrors_written_from_linux_and_cut_off() {
    let e = evidence("rw-mirror-ntfs3g.json");
    assert_eq!(
        (e["attached"][0][1].as_str(), e["repaired"][0][1].as_str()),
        (Some("Healthy"), Some("Healthy"))
    );
    assert_eq!(e["chkdsk_exit"], 0);
    assert_eq!(e["files"], 573);
    assert_eq!(e["mismatching"].as_array().unwrap().len(), 0);
    let e = evidence("rw-mirror-crash.json");
    assert_eq!(
        (e["attached"][0][1].as_str(), e["repaired"][0][1].as_str()),
        (Some("Healthy"), Some("Healthy"))
    );
}

/// A single parity space written from Linux in place under the journal
/// (parity3_26100 copies): NTFS with ntfs-3g and 3000 file operations was
/// accepted (healthy, chkdsk clean, all files intact). A write cut off
/// between its data and its parity left a stripe the journal records as not
/// consistent: Windows attached the space as healthy but repaired nothing,
/// not even with Repair-VirtualDisk, so after a later disk failure it would
/// rebuild from the stale parity. In-place rewrites leave that write hole
/// open; Windows avoids it by rewriting stripes through the cache.
#[test]
fn in_place_parity_writes_leave_windows_a_stale_stripe_after_a_crash() {
    let e = evidence("rw-parity-ntfs3g.json");
    assert_eq!(
        (e["attached"][0][1].as_str(), e["repaired"][0][1].as_str()),
        (Some("Healthy"), Some("Healthy"))
    );
    assert_eq!(e["chkdsk_exit"], 0);
    assert_eq!(e["mismatching"].as_array().unwrap().len(), 0);
    let e = evidence("rw-parity-crash.json");
    assert_eq!(e["repaired"][0][1], "Healthy");
    assert!(e["after_windows"].as_str().unwrap().contains("still inconsistent"));
}

/// Single parity NTFS written from Linux through the write-back cache, with
/// a log that wrapped behind a checkpoint: Windows loaded the cache as
/// Linux left it (830 chunks, from slot 0 after the checkpoint up to slot
/// 0x16b at sequence 0x56c) and the journal, attached the space as healthy
/// with nothing to repair, and chkdsk and every file checked out.
#[test]
fn windows_reads_parity_written_through_the_cache() {
    let e = evidence("rw-parity-cache-ntfs3g.json");
    assert_eq!(
        (e["attached"][0][1].as_str(), e["repaired"][0][1].as_str()),
        (Some("Healthy"), Some("Healthy"))
    );
    assert_eq!(e["attached_jobs"].as_array().unwrap().len(), 0);
    assert_eq!((e["chkdsk_exit"].as_i64(), e["files"].as_i64()), (Some(0), Some(573)));
    assert_eq!(e["mismatching"].as_array().unwrap().len(), 0);
    let loaded = e["windows_loaded"][0].as_str().unwrap();
    assert!(loaded.contains("UsedLineCount: 0x33E") && loaded.contains("EndSlot: 0x16B"));
}

/// A destage cut off between the data and the parity of a stripe: the
/// cache still held the whole stripe, so Windows read the new data, found
/// the space healthy, and finished the destage itself (every stripe matches
/// its parity afterwards): the write hole stays closed for Windows too.
#[test]
fn windows_finishes_a_destage_cut_off_by_a_crash() {
    let e = evidence("rw-parity-cache-crash.json");
    assert_eq!(
        (e["attached"][0][1].as_str(), e["repaired"][0][1].as_str()),
        (Some("Healthy"), Some("Healthy"))
    );
    assert_eq!(e["pattern_check"]["ok"], true);
    assert!(e["windows_loaded"][0].as_str().unwrap().contains("UsedLineCount: 0x8"));
    assert!(
        e["after_windows"]
            .as_str()
            .unwrap()
            .contains("every stripe matches its parity")
    );
}

/// Cache logs written from Linux and read by Windows (the rule the reader
/// and CacheWriter follow, see cache::Checkpoint): without a checkpoint
/// Windows reads a wrapped log from slot 0 only and loses every chunk mapped
/// before it; a checkpoint naming the wrong slot to continue at loses the
/// slots in between; with the checkpoint Windows itself writes, every chunk
/// is read from the cache. NTFS written through the cache before the fix
/// lost 7 files this way.
#[test]
fn windows_reads_a_wrapped_cache_log_from_its_checkpoint() {
    let e = evidence("rw-cache-log.json");
    let runs = e["runs"].as_array().unwrap();
    let cached = |i: usize, set: &str| runs[i]["windows_reads_cached"][set].as_u64().unwrap();
    for i in [0, 1] {
        assert_eq!((cached(i, "B"), cached(i, "F"), cached(i, "E")), (0, 0, 30));
    }
    assert_eq!((cached(2, "F"), cached(2, "E")), (921, 29));
    assert_eq!(runs[2]["windows_reads_space"]["E"], 1);
    for (set, n) in [("A", 50), ("B", 50), ("F", 921), ("E", 30)] {
        assert_eq!(cached(3, set), n, "{set}");
    }
    assert_eq!(e["ntfs_before_the_fix"]["mismatching"], 7);
}

/// A thin parity space filled from Linux beyond its initial allocation
/// (768 MiB to 4.5 GiB, rows allocated as the cache destaged them, NTFS
/// with ntfs-3g): Windows loaded the cache as Linux left it, found the
/// space healthy with nothing to repair, and chkdsk and every file checked
/// out.
#[test]
fn windows_reads_a_thin_parity_space_filled_from_linux() {
    let e = evidence("rw-thin-parity-ntfs3g.json");
    assert_eq!(
        (e["attached"][0][1].as_str(), e["repaired"][0][1].as_str()),
        (Some("Healthy"), Some("Healthy"))
    );
    assert_eq!(e["attached_jobs"].as_array().unwrap().len(), 0);
    assert_eq!((e["chkdsk_exit"].as_i64(), e["files"].as_i64()), (Some(0), Some(573)));
    assert_eq!(e["mismatching"].as_array().unwrap().len(), 0);
    assert!(
        e["windows_loaded"][0]
            .as_str()
            .unwrap()
            .contains("UsedLineCount: 0x47D")
    );
}
