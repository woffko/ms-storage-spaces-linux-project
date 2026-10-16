//! The largest numbers the metadata of a pool may hold. Records carry no
//! checksum, and every number in them is a varint of up to 64 bits: a value
//! beyond one of these limits (far beyond what Windows writes, see
//! docs/storage-spaces-format.md, "Plausible values") makes its space
//! unreadable, as metadata that is not understood, instead of a number the
//! readers compute with, loop over or allocate by.

/// Columns of a space or storage tier (the corpus: 16 at most).
pub const MAX_COLUMNS: u64 = 1024;

/// Copies of a column: the policy's, and one more than any extent's copy
/// number (the corpus: 3, while a copy is regenerated).
pub const MAX_COPIES: u64 = 16;

/// Local groups of a dual parity space (the corpus: 3).
pub const MAX_GROUPS: u64 = 64;

/// Disk failures a space tolerates (the corpus: 2).
pub const MAX_REDUNDANCY: u64 = 3;

/// Bytes of a space, of the part of its owner a storage tier or cache
/// covers, and of what its extents map (1 EiB; the corpus: 8 GiB).
pub const MAX_SPACE_SIZE: u64 = 1 << 60;

/// Slab numbers and counts of an extent (2^32 slabs of 256 MiB are 1 EiB).
pub const MAX_SLABS: u64 = 1 << 32;

/// Offsets inside the write-back cache and parity journal spaces (their
/// slot areas, checkpoints and data; real ones are GiBs).
pub const MAX_LOG_OFFSET: u64 = 1 << 56;
