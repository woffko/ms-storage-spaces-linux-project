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
