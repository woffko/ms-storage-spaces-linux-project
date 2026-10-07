//! The parity journal (SPVDT) of parity spaces. The input is the journal
//! space from its start; the harness sets the signature, owner and header
//! CRC so that inputs reach the entry parser. The journal model then logs
//! writes of stripes taken from the input, which must read back as exactly
//! the stripes written being consistent.
#![no_main]

use libfuzzer_sys::fuzz_target;
use storage_spaces::Guid;
use storage_spaces::cache::LogWrite;
use storage_spaces::journal::{JournalWriter, ParityJournal};

const OWNER: [u8; 16] = [9; 16];

fn set_crc(b: &mut [u8], field: usize) {
    b[field..field + 4].fill(0);
    let crc = crc32fast::hash(b);
    b[field..field + 4].copy_from_slice(&crc.to_le_bytes());
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 0x60 {
        return;
    }
    let mut space = data.to_vec();
    space[0..8].copy_from_slice(b"SPVDT\0\0\0");
    space[8..24].copy_from_slice(&OWNER);
    // Keep the slot area small and inside the input most of the time; slots
    // whose first byte is odd get a valid signature, size and CRC.
    let slot_size = 0x40 + (u32::from_le_bytes(space[0x38..0x3c].try_into().unwrap()) % 0x1000);
    let slot_count = u32::from_le_bytes(space[0x3c..0x40].try_into().unwrap()) % 64;
    space[0x30..0x38].copy_from_slice(&0x60u64.to_le_bytes());
    space[0x38..0x3c].copy_from_slice(&slot_size.to_le_bytes());
    space[0x3c..0x40].copy_from_slice(&slot_count.to_le_bytes());
    set_crc(&mut space[..0x60], 0x24);
    let slots_end = (0x60 + (slot_size * slot_count) as usize).min(space.len());
    for slot in space[0x60..slots_end].chunks_exact_mut(slot_size as usize) {
        if slot[0] & 1 == 1 {
            slot[0..8].copy_from_slice(b"SPSLOT\0\0");
            slot[0x1c..0x20].copy_from_slice(&slot_size.to_le_bytes());
            slot[0x20..0x24].fill(0);
            set_crc(slot, 0x24);
        }
    }
    // The owner as the journal stores it (mixed-endian).
    let mut owner = OWNER;
    owner[0..4].reverse();
    owner[4..6].reverse();
    owner[6..8].reverse();
    let read = |offset: u64, buf: &mut [u8]| {
        buf.fill(0);
        if let Some(src) = usize::try_from(offset).ok().and_then(|o| space.get(o..)) {
            let n = src.len().min(buf.len());
            buf[..n].copy_from_slice(&src[..n]);
        }
        Ok(())
    };
    if let Ok(Some(journal)) = ParityJournal::load(Guid::from_slice(&owner).unwrap(), read) {
        let _ = journal.dirty_runs();
        let _ = journal.check_inside(1 << 30);
        for run in [0, 1 << 28, u64::MAX] {
            for stripe in [0, 1, 4095, u64::MAX] {
                let _ = journal.is_dirty(run, stripe);
            }
        }
    }
    // A quarter of the inputs also drive the model (it is slower).
    if data[0] & 3 == 0 {
        model_round_trip(data, Guid::from_slice(&owner).unwrap());
    }
});

/// Up to 100 writes into a run of 40000 stripes (so that runs of stripes
/// longer than a run word can hold occur), logged by the model into 32
/// slots with two checkpoint areas, so that the log wraps behind
/// checkpoints; loading it must give the stripes written as consistent.
fn model_round_trip(data: &[u8], owner: Guid) {
    const STRIPES: u64 = 40000;
    const SLOT: usize = 0x2000;
    const CP: usize = 0x2000;
    let cp_offset = 0x60 + 32 * SLOT;
    let mut journal = JournalWriter::new(owner, SLOT as u32, 32).with_checkpoint_areas(2);
    let mut space = vec![0u8; cp_offset + 2 * CP];
    space[0..8].copy_from_slice(b"SPVDT\0\0\0");
    space[8..24].copy_from_slice(&owner.to_mixed_endian());
    space[0x30..0x38].copy_from_slice(&0x60u64.to_le_bytes());
    space[0x38..0x3c].copy_from_slice(&(SLOT as u32).to_le_bytes());
    space[0x3c..0x40].copy_from_slice(&32u32.to_le_bytes());
    space[0x40..0x48].copy_from_slice(&(cp_offset as u64).to_le_bytes());
    space[0x48..0x4c].copy_from_slice(&(CP as u32).to_le_bytes());
    space[0x4c..0x50].copy_from_slice(&2u32.to_le_bytes());
    set_crc(&mut space[..0x60], 0x24);
    let mut written = vec![false; STRIPES as usize];
    for w in data.chunks_exact(6).take(100) {
        let first = u64::from(u32::from_le_bytes(w[..4].try_into().unwrap())) % STRIPES;
        let count = (u64::from(u16::from_le_bytes([w[4], w[5]])) + 1).min(STRIPES - first);
        for record in journal.write(0, STRIPES, first, count) {
            match record {
                LogWrite::Slot(index, page) => {
                    space[0x60 + index * SLOT..0x60 + (index + 1) * SLOT].copy_from_slice(&page)
                }
                LogWrite::Checkpoint(area, page) => {
                    let at = cp_offset + area * CP;
                    let n = page.len().min(CP);
                    space[at..at + n].copy_from_slice(&page[..n]);
                }
            }
        }
        written[first as usize..(first + count) as usize].fill(true);
    }
    if !written.contains(&true) {
        return;
    }
    let journal = ParityJournal::load(owner, |off: u64, buf: &mut [u8]| {
        buf.copy_from_slice(&space[off as usize..off as usize + buf.len()]);
        Ok(())
    })
    .unwrap()
    .unwrap();
    for (s, &w) in written.iter().enumerate().step_by(13) {
        assert_eq!(journal.is_dirty(0, s as u64), !w, "stripe {s}");
    }
}
