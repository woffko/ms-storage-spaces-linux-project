use std::fmt;

/// A GUID stored in big-endian byte order, as Storage Spaces metadata does.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Guid(pub [u8; 16]);

impl Guid {
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Guid)
    }

    /// Parses a GUID stored in the Microsoft mixed-endian layout (GPT, SPCACHE).
    pub fn from_mixed_endian(b: &[u8; 16]) -> Self {
        let mut g = *b;
        g[0..4].reverse();
        g[4..6].reverse();
        g[6..8].reverse();
        Guid(g)
    }

    /// The Microsoft mixed-endian layout (inverse of [`Guid::from_mixed_endian`]).
    pub fn to_mixed_endian(&self) -> [u8; 16] {
        let mut g = self.0;
        g[0..4].reverse();
        g[4..6].reverse();
        g[6..8].reverse();
        g
    }

    pub fn is_nil(&self) -> bool {
        self.0 == [0; 16]
    }
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
        )
    }
}

impl fmt::Debug for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_mixed_endian() {
        let raw = [
            0x8f, 0xaf, 0x5c, 0xe7, 0x80, 0xf6, 0xee, 0x4c, 0xaf, 0xa3, 0xb0, 0x01, 0xe5, 0x6e, 0xfc, 0x2d,
        ];
        let g = Guid::from_mixed_endian(&raw);
        assert_eq!(g.to_string(), "e75caf8f-f680-4cee-afa3-b001e56efc2d");
    }
}
