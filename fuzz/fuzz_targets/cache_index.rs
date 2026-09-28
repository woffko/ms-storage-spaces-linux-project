//! The write-back cache header and slot log. The input is the cache space
//! from its start; the harness sets signatures, the owner GUID and CRCs
//! (when the first byte of a slot is odd) so that inputs reach the slot
//! parser. The cache model then writes a log for writes taken from the
//! input, which must read back as exactly the sectors written.
#![no_main]

use libfuzzer_sys::fuzz_target;
use storage_spaces::Guid;
use storage_spaces::cache::{CacheHeader, CacheIndex, CacheWriter, Lookup};

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
    model_round_trip(data);
});

/// Writes of up to 128 KiB within 16 MiB, from the input, logged by the
/// model into a 64-slot cache of 512 KiB chunks.
fn model_round_trip(data: &[u8]) {
    const CHUNK: u64 = 512 << 10;
    let header = CacheHeader {
        owner_guid: Guid::from_mixed_endian(&OWNER),
        sequence: 1,
        slot_offset: 0,
        slot_size: 4096,
        slot_count: 64,
        data_offset: 5 << 20,
        chunk_size: CHUNK as u32,
        chunk_count: 2038,
    };
    let mut writer = CacheWriter::new(header.clone(), 64);
    let mut area = vec![0u8; 64 * 4096];
    area[..4096].copy_from_slice(&writer.init_slot());
    let mut written = vec![false; (16 << 20) / 512];
    for w in data.chunks_exact(4).take(63) {
        let offset = u64::from(u16::from_le_bytes([w[0], w[1]]) % 32768) * 512;
        let len = (u64::from(u16::from_le_bytes([w[2], w[3]]) % 256) + 1) * 512;
        let len = len.min((16 << 20) - offset);
        for (index, page) in writer.write(offset, len) {
            area[index * 4096..(index + 1) * 4096].copy_from_slice(&page);
        }
        written[(offset / 512) as usize..((offset + len) / 512) as usize].fill(true);
    }
    let index = CacheIndex::load(header, |off: u64, buf: &mut [u8]| {
        buf.copy_from_slice(&area[off as usize..off as usize + buf.len()]);
        Ok(())
    })
    .unwrap();
    for (sector, &w) in written.iter().enumerate().step_by(7) {
        let offset = sector as u64 * 512;
        match index.lookup(offset) {
            Lookup::Hit { cache_offset, .. } => {
                assert!(w, "sector {sector} cached but never written");
                assert_eq!(cache_offset % CHUNK, offset % CHUNK);
            }
            Lookup::Miss { .. } => assert!(!w, "sector {sector} written but not cached"),
        }
    }
}
