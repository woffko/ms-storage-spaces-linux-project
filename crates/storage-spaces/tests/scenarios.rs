//! Predictions of what Windows writes, checked against the states of the
//! scenarios in tools/scenarios.sh (fixtures captured from the snapshots
//! tools/vm/Invoke-Scenario.ps1 took between the steps).

use std::fs::File;
use std::path::Path;

use storage_spaces::Pool;
use storage_spaces::drt::DirtyRegions;
use storage_spaces::io::SparseImage;

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

/// The dirty region log of the only user space, and the size of its
/// tracking space (the second header copy sits 8 KiB before its end).
fn log(pool: &Pool<SparseImage>) -> (DirtyRegions, u64) {
    let space = pool.user_spaces().next().unwrap();
    let reader = pool.open_space(space.id()).unwrap();
    let log = reader.dirty_regions().unwrap().clone();
    let size = log.copies()[1].offset + 0x2000;
    (log, size)
}

/// The header pages of `after` are those of `before` with `written` (offset,
/// page) replacing a copy; with `covered_only` just the part the checksum
/// covers is compared.
fn assert_pages(
    before: &DirtyRegions,
    after: &DirtyRegions,
    written: &[(u64, Vec<u8>)],
    step: &str,
    covered_only: bool,
) {
    for (b, a) in before.copies().iter().zip(after.copies()) {
        let expected = written
            .iter()
            .rev()
            .find(|(o, _)| *o == b.offset)
            .map_or(&b.page, |(_, p)| p);
        let len = if covered_only {
            covered(expected)
        } else {
            expected.len()
        };
        assert_eq!(a.page[..len], expected[..len], "{step}: copy at {:#x}", b.offset);
    }
}

/// Length of the part of a header page the checksum covers.
fn covered(page: &[u8]) -> usize {
    0x18 + 8 * u32::from_le_bytes(page[0x10..0x14].try_into().unwrap()) as usize
}

/// m5drt: a two-way mirror of four 256 MiB extent runs. Windows wrote
/// exactly the header pages the model predicts: the first write into a run
/// adds it in the next generation, which replaces the older copy; a write
/// into a listed run changes nothing; runs that went clean (run 2, written
/// two minutes before) are left out of the next generation; a disconnect
/// resets both copies to generation 0, and after reconnecting the log starts
/// again from there. A normal header write leaves the rest of the page zero.
#[test]
fn mirror_dirty_region_log_follows_the_writes() {
    let s: Vec<_> = (0..8).map(|i| log(&state("m5drt", &format!("s{i}")))).collect();
    let size = s[0].1;
    // A new space: both copies empty.
    assert_eq!(
        DirtyRegions::after_disconnect(size).to_vec(),
        [
            (0, s[0].0.copies()[0].page.clone()),
            (size - 0x2000, s[0].0.copies()[1].page.clone())
        ]
    );
    // s0 -> s1: a write into run 0.
    let w1 = s[0].0.after_first_write(size, 0, &[]).unwrap();
    assert_eq!(w1.0, size - 0x2000);
    assert_pages(&s[0].0, &s[1].0, &[w1], "s1", false);
    // s1 -> s2: run 2.
    let w2 = s[1].0.after_first_write(size, 2, &[]).unwrap();
    assert_eq!(w2.0, 0);
    assert_pages(&s[1].0, &s[2].0, &[w2], "s2", false);
    // s2 -> s3: run 0 again, already listed.
    assert!(s[2].0.after_first_write(size, 0, &[]).is_none());
    assert_pages(&s[2].0, &s[3].0, &[], "s3", false);
    // s3 -> s4: runs 1 and 3; run 2 has gone clean meanwhile.
    let w3 = s[3].0.after_first_write(size, 1, &[2]).unwrap();
    let mid = DirtyRegions::load(size, |off, buf| {
        let page = if off == w3.0 { &w3.1 } else { &s[3].0.copies()[0].page };
        buf.copy_from_slice(page);
        Ok(())
    })
    .unwrap()
    .unwrap();
    let w4 = mid.after_first_write(size, 3, &[]).unwrap();
    assert_pages(&s[3].0, &s[4].0, &[w3, w4], "s4", false);
    // s4 -> s5: disconnect. Only the covered part is predicted: Windows
    // leaves stale entries after the empty list (1, 1, 3 here).
    assert_pages(&s[4].0, &s[5].0, &DirtyRegions::after_disconnect(size), "s5", true);
    assert_ne!(s[5].0.copies()[0].page[0x18..0x30], [0; 24]);
    // s5 -> s6: connect changes nothing.
    assert_pages(&s[5].0, &s[6].0, &[], "s6", false);
    // s6 -> s7: run 1 after reconnecting.
    let w7 = s[6].0.after_first_write(size, 1, &[]).unwrap();
    assert_pages(&s[6].0, &s[7].0, &[w7], "s7", false);
}
