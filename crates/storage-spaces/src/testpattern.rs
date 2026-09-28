//! The verification pattern written by `tools/vm/New-TestPool.ps1`.
//!
//! Every 4096-byte block at byte offset `O` holds "SSPATTRN", `O` as u64 LE,
//! a 16-byte tag and a splitmix64 stream seeded with `O`.

pub const BLOCK: usize = 4096;
pub const MAGIC: &[u8; 8] = b"SSPATTRN";

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Produces the expected content of the block at `offset`.
pub fn fill_block(block: &mut [u8], offset: u64, tag: &str) {
    let mut t = [0u8; 16];
    let n = tag.len().min(16);
    t[..n].copy_from_slice(&tag.as_bytes()[..n]);
    fill_block_raw(block, offset, &t);
}

/// [`fill_block`] with the 16 tag bytes as stored.
pub fn fill_block_raw(block: &mut [u8], offset: u64, tag: &[u8; 16]) {
    assert_eq!(block.len(), BLOCK);
    block[0..8].copy_from_slice(MAGIC);
    block[8..16].copy_from_slice(&offset.to_le_bytes());
    block[16..32].copy_from_slice(tag);
    let mut state = offset;
    for word in block[32..].as_chunks_mut::<8>().0 {
        word.copy_from_slice(&splitmix(&mut state).to_le_bytes());
    }
}

/// Checks a buffer that starts at `offset` (both multiples of [`BLOCK`]).
/// Returns the offset of the first mismatching block.
pub fn verify(buf: &[u8], offset: u64, tag: &str) -> Option<u64> {
    let mut expected = [0u8; BLOCK];
    for (i, block) in buf.as_chunks::<BLOCK>().0.iter().enumerate() {
        let at = offset + (i * BLOCK) as u64;
        fill_block(&mut expected, at, tag);
        if *block != expected {
            return Some(at);
        }
    }
    None
}
