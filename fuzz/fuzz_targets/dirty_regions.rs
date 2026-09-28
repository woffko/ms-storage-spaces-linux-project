//! Dirty region tracking headers (SPACEDRT) of mirror spaces. The input is
//! the tracking space; the harness fixes the signature and CRC of both
//! header copies when their first byte after the signature is odd. Valid
//! copies must survive encoding and decoding unchanged.
#![no_main]

use libfuzzer_sys::fuzz_target;
use storage_spaces::drt::DirtyRegions;

fn fix(h: &mut [u8]) {
    h[0..8].copy_from_slice(b"SPACEDRT");
    let count = (u32::from_le_bytes(h[0x10..0x14].try_into().unwrap()) % 64) as usize;
    h[0x10..0x14].copy_from_slice(&(count as u32).to_le_bytes());
    let span = 0x18 + 8 * count;
    h[0x14..0x18].fill(0);
    let crc = crc32fast::hash(&h[..span]);
    h[0x14..0x18].copy_from_slice(&crc.to_le_bytes());
}

fuzz_target!(|data: &[u8]| {
    let mut space = data.to_vec();
    space.resize(space.len().max(0x4000), 0);
    let len = space.len();
    if space[8] & 1 == 1 {
        fix(&mut space[..0x1000]);
    }
    let second = len - 0x2000;
    if space[second + 8] & 1 == 1 {
        fix(&mut space[second..second + 0x1000]);
    }
    let read = |off: u64, buf: &mut [u8]| {
        buf.fill(0);
        if let Some(src) = space.get(off as usize..) {
            let n = src.len().min(buf.len());
            buf[..n].copy_from_slice(&src[..n]);
        }
        Ok(())
    };
    if let Ok(Some(d)) = DirtyRegions::load(len as u64, read) {
        let _ = d.dirty_runs();
        let _ = d.is_dirty(0);
        // Encoding a valid copy reproduces the part its checksum covers,
        // and decodes to the same header.
        for c in d.copies() {
            if let Some(h) = &c.header {
                let page = h.encode();
                let covered = 0x18 + 8 * h.runs.len();
                assert_eq!(page[..covered], c.page[..covered]);
                let again = DirtyRegions::load(0x4000, |off, buf| {
                    buf.fill(0);
                    if off == 0 {
                        buf.copy_from_slice(&page);
                    }
                    Ok(())
                })
                .unwrap()
                .unwrap();
                assert_eq!(again.copies()[0].header.as_ref(), Some(h));
                // The model's next header decodes to what it lists.
                let mut w = d.writer();
                w.clean(|r| r % 2 == 1);
                if let Some((_, next)) = w.write(u64::MAX) {
                    let listed = DirtyRegions::load(0x4000, |off, buf| {
                        buf.fill(0);
                        if off == 0 {
                            buf.copy_from_slice(&next);
                        }
                        Ok(())
                    })
                    .unwrap()
                    .unwrap();
                    assert_eq!(listed.copies()[0].header.as_ref().unwrap().runs, w.runs());
                }
            }
        }
    }
});
