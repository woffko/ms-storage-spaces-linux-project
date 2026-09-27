//! The write-back cache header and slot log. The input is the cache space
//! from its start; the harness sets signatures, the owner GUID and CRCs
//! (when the first byte of a slot is odd) so that inputs reach the slot
//! parser.
#![no_main]

use libfuzzer_sys::fuzz_target;
use storage_spaces::cache::{CacheHeader, CacheIndex};

const OWNER: [u8; 16] = [7; 16];

fn set_crc(b: &mut [u8], field: usize) {
    b[field..field + 4].fill(0);
    let crc = crc32fast::hash(b);
    b[field..field + 4].copy_from_slice(&crc.to_le_bytes());
}

fuzz_target!(|data: &[u8]| {
    if data.len() < CacheHeader::SIZE {
        return;
    }
    let mut space = data.to_vec();
    space[0..8].copy_from_slice(b"SPCACHE\0");
    space[8..24].copy_from_slice(&OWNER);
    space[0x1c..0x20].copy_from_slice(&(CacheHeader::SIZE as u32).to_le_bytes());
    // Keep the slot area small and inside the input most of the time.
    let slot_size = 0x40 + (u32::from_le_bytes(space[0x38..0x3c].try_into().unwrap()) % 0x1000);
    let slot_count = u32::from_le_bytes(space[0x3c..0x40].try_into().unwrap()) % 64;
    space[0x30..0x38].copy_from_slice(&(CacheHeader::SIZE as u64).to_le_bytes());
    space[0x38..0x3c].copy_from_slice(&slot_size.to_le_bytes());
    space[0x3c..0x40].copy_from_slice(&slot_count.to_le_bytes());
    set_crc(&mut space[..CacheHeader::SIZE], 0x24);
    let slots_end = (CacheHeader::SIZE + (slot_size * slot_count) as usize).min(space.len());
    for slot in space[CacheHeader::SIZE..slots_end].chunks_exact_mut(slot_size as usize) {
        if slot[0] & 1 == 1 {
            slot[0..8].copy_from_slice(b"SPSLOT\0\0");
            slot[8..24].copy_from_slice(&OWNER);
            slot[0x1c..0x20].copy_from_slice(&slot_size.to_le_bytes());
            set_crc(slot, 0x24);
        }
    }
    let Ok(Some(header)) = CacheHeader::parse(&space) else {
        return;
    };
    let read = |offset: u64, buf: &mut [u8]| {
        buf.fill(0);
        if let Some(src) = usize::try_from(offset).ok().and_then(|o| space.get(o..)) {
            let n = src.len().min(buf.len());
            buf[..n].copy_from_slice(&src[..n]);
        }
        Ok(())
    };
    if let Ok(index) = CacheIndex::load(header, read) {
        for offset in [0, 4096, 1 << 20, u64::MAX / 2, u64::MAX] {
            let _ = index.lookup(offset);
        }
    }
});
