//! The write-back cache header, slot log and checkpoints. The input is the
//! cache space from its start; the harness sets signatures, the owner GUID
//! and CRCs (when the first byte of a slot or checkpoint area is odd) so
//! that inputs reach the slot and checkpoint parsers, as the cache of a
//! space with 512-byte or 4 KiB sectors (bit 0 of the second byte). The
//! cache model then logs writes taken from the input into a small log that
//! wraps (with checkpoints) and is destaged when full; loading it must give
//! exactly the writer's own map.
#![no_main]

use libfuzzer_sys::fuzz_target;
use storage_spaces::Guid;
use storage_spaces::cache::{CacheHeader, CacheIndex, CacheWriter, LogWrite, Lookup};

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
    // Checkpoint areas after the slot area, valid where the input says so.
    let cp_offset = slots_end as u64;
    let cp_size = 0x200 + (u32::from_le_bytes(space[0x48..0x4c].try_into().unwrap()) % 0x2000);
    space[0x40..0x48].copy_from_slice(&cp_offset.to_le_bytes());
    space[0x48..0x4c].copy_from_slice(&cp_size.to_le_bytes());
    space[0x4c..0x50].copy_from_slice(&2u32.to_le_bytes());
    set_crc(&mut space[..CacheHeader::SIZE], 0x24);
    for area in 0..2usize {
        let at = cp_offset as usize + area * cp_size as usize;
        let Some(cp) = space.get_mut(at..at + cp_size as usize) else {
            break;
        };
        if cp[0] & 1 == 1 {
            let len = (u32::from_le_bytes(cp[0x48..0x4c].try_into().unwrap()) % (cp_size - 0x200 + 1)) as usize;
            cp[0..8].copy_from_slice(b"SPCHECK\0");
            cp[8..24].copy_from_slice(&OWNER);
            cp[0x1c..0x20].copy_from_slice(&0x50u32.to_le_bytes());
            cp[0x40..0x44].copy_from_slice(&0x200u32.to_le_bytes());
            cp[0x48..0x4c].copy_from_slice(&(len as u32).to_le_bytes());
            let crc = crc32fast::hash(&cp[0x200..0x200 + len]);
            cp[0x30..0x34].copy_from_slice(&crc.to_le_bytes());
            cp[0x34..0x38].copy_from_slice(&(0x200 + len as u32).to_le_bytes());
            set_crc(&mut cp[..0x50], 0x24);
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
    let sector = if data[1] & 1 == 0 { 512 } else { 4096 };
    if let Ok(index) = CacheIndex::load(header, sector, read) {
        for offset in [0, 4096, 1 << 20, u64::MAX / 2, u64::MAX] {
            let _ = index.lookup(offset);
        }
    }
    // A quarter of the inputs also drive the model (it is slower).
    if data[0] & 3 == 0 {
        model_round_trip(data);
    }
});

/// Up to 300 writes of up to 256 sectors within 16 MiB, from the input,
/// logged by the model into a 64-slot cache of 512 KiB chunks with two
/// checkpoint areas, on a space with 512-byte or 4 KiB sectors (bit 1 of
/// the second byte); everything is destaged when the log or the blocks run
/// out. The log wraps behind checkpoints; loading it must map exactly what
/// the writer maps.
fn model_round_trip(data: &[u8]) {
    const CHUNK: u64 = 512 << 10;
    const SLOTS: usize = 64;
    const CP_SIZE: usize = 0x10000;
    let header = CacheHeader {
        owner_guid: Guid::from_mixed_endian(&OWNER),
        sequence: 1,
        slot_offset: 0,
        slot_size: 4096,
        slot_count: SLOTS as u32,
        checkpoint_offset: (SLOTS * 4096) as u64,
        checkpoint_size: CP_SIZE as u32,
        checkpoint_count: 2,
        data_offset: 5 << 20,
        chunk_size: CHUNK as u32,
        chunk_count: 2038,
    };
    let sector: u64 = if data[1] & 2 == 0 { 512 } else { 4096 };
    let mut writer = CacheWriter::new(header.clone(), sector as u32, 64).unwrap();
    let mut area = vec![0u8; SLOTS * 4096 + 2 * CP_SIZE];
    area[..4096].copy_from_slice(&writer.init_slot());
    let apply = |records: Vec<LogWrite>, area: &mut Vec<u8>| {
        for r in records {
            match r {
                LogWrite::Slot(i, page) => area[i * 4096..(i + 1) * 4096].copy_from_slice(&page),
                LogWrite::Checkpoint(a, page) => {
                    let at = SLOTS * 4096 + a * CP_SIZE;
                    area[at..at + page.len()].copy_from_slice(&page);
                }
            }
        }
    };
    for w in data.chunks_exact(4).take(300) {
        let offset = u64::from(u16::from_le_bytes([w[0], w[1]])) % ((16 << 20) / sector) * sector;
        let len = (u64::from(u16::from_le_bytes([w[2], w[3]]) % 256) + 1) * sector;
        let len = len.min((16 << 20) - offset);
        if writer.is_full_for(offset, len) {
            let cached: Vec<u64> = writer.cached().iter().map(|c| c.0).collect();
            let records = writer.destage(&cached);
            apply(records, &mut area);
        }
        if writer.is_full_for(offset, len) {
            continue;
        }
        let records = writer.write(offset, len);
        apply(records, &mut area);
    }
    let index = CacheIndex::load(header, sector as u32, |off: u64, buf: &mut [u8]| {
        buf.copy_from_slice(&area[off as usize..off as usize + buf.len()]);
        Ok(())
    })
    .unwrap();
    for n in (0..(16u64 << 20) / sector).step_by(7) {
        let offset = n * sector;
        match (index.lookup(offset), writer.lookup(offset)) {
            (Lookup::Hit { cache_offset, .. }, Some(expected)) => assert_eq!(cache_offset, expected, "sector {n}"),
            (Lookup::Miss { .. }, None) => {}
            (loaded, model) => panic!("sector {n}: loaded {loaded:?}, writer {model:?}"),
        }
    }
}
