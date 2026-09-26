//! CRC-32 (IEEE 802.3, as in zlib), used by the cache structures.

const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static TABLE: [u32; 256] = make_table();

pub fn crc32(data: &[u8]) -> u32 {
    !data
        .iter()
        .fold(!0u32, |c, &b| TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8))
}

/// CRC-32 of `data` with the 4-byte checksum field at `field` treated as zero.
pub fn crc32_excluding(data: &[u8], field: usize) -> u32 {
    let mut copy = data.to_vec();
    copy[field..field + 4].fill(0);
    crc32(&copy)
}

#[cfg(test)]
mod tests {
    #[test]
    fn known_value() {
        assert_eq!(super::crc32(b"123456789"), 0xCBF4_3926);
    }
}
