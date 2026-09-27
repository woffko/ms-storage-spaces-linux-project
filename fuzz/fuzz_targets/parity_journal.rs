//! The parity journal (SPVDT) of parity spaces. The input is the journal
//! space from its start; the harness sets the signature, owner and header
//! CRC so that inputs reach the entry parser.
#![no_main]

use libfuzzer_sys::fuzz_target;
use storage_spaces::Guid;
use storage_spaces::journal::ParityJournal;

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
        for run in [0, 1 << 28, u64::MAX] {
            for stripe in [0, 1, 4095, u64::MAX] {
                let _ = journal.is_dirty(run, stripe);
            }
        }
    }
});
