//! Nodes of ReFS's B+-trees (Minstore). A node is the same structure
//! wherever it is: on a metadata page (its descriptor at page + 0x50), in
//! the value of a file record, in a $DATA record (its extent map).
//!
//! A node descriptor starts with the offset from itself to the node header;
//! the header holds the level (0 = leaf), the bounds of the key index (4
//! bytes per row: a 16-bit row offset relative to the header and a 16-bit
//! marker) and the row count. A row starts with a 16-byte row header: its
//! size, the offset and length of its key and of its value, both relative
//! to the row.

use crate::error::{Result, format_err};
use crate::util::{le16, le32};

#[derive(Debug, Clone)]
pub struct Node<'a> {
    buf: &'a [u8],
    header: usize,
    pub level: u8,
    pub flags: u8,
    offsets: Vec<usize>,
}

/// A key and a value.
#[derive(Debug, Clone, Copy)]
pub struct Row<'a> {
    pub key: &'a [u8],
    pub value: &'a [u8],
}

impl<'a> Node<'a> {
    /// The node whose descriptor starts at `buf[descriptor]`.
    pub fn at(buf: &'a [u8], descriptor: usize) -> Result<Self> {
        let rel = le32(buf, descriptor) as usize;
        let header = descriptor
            .checked_add(rel)
            .filter(|&h| rel != 0 && h.checked_add(0x28).is_some_and(|e| e <= buf.len()))
            .ok_or_else(|| format_err!("node header outside its {} bytes", buf.len()))?;
        Self::with_header(buf, header)
    }

    /// The node whose header starts at `buf[header]`.
    pub fn with_header(buf: &'a [u8], header: usize) -> Result<Self> {
        if header.checked_add(0x28).is_none_or(|e| e > buf.len()) {
            return Err(format_err!("node header outside its {} bytes", buf.len()));
        }
        let level = buf[header + 0x0c];
        let flags = buf[header + 0x0d];
        let start = le32(buf, header + 0x10) as usize;
        let count = le32(buf, header + 0x14) as usize;
        let end = le32(buf, header + 0x20) as usize;
        if end < start || (end - start) / 4 != count || header + end > buf.len() {
            return Err(format_err!(
                "node key index {start:#x}..{end:#x} does not hold {count} rows"
            ));
        }
        let offsets = (0..count)
            .map(|i| header + le16(buf, header + start + 4 * i) as usize)
            .collect();
        Ok(Node {
            buf,
            header,
            level,
            flags,
            offsets,
        })
    }

    pub fn is_leaf(&self) -> bool {
        self.level == 0
    }

    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// The rows in key order.
    pub fn rows(&self) -> impl Iterator<Item = Result<Row<'a>>> + '_ {
        self.offsets.iter().map(move |&at| {
            let b = self.buf;
            let field = |o: usize| le16(b, at + o) as usize;
            if at + 0x10 > b.len() {
                return Err(format_err!("row at {at:#x} outside its node"));
            }
            let (koff, klen, voff, vlen) = (field(4), field(6), field(0x0a), field(0x0c));
            let key = b.get(at + koff..at + koff + klen);
            let value = b.get(at + voff..at + voff + vlen);
            match (key, value) {
                (Some(key), Some(value)) => Ok(Row { key, value }),
                _ => Err(format_err!("row at {at:#x} reaches beyond its node")),
            }
        })
    }

    /// The raw records the key index points at (the rows of an extent map
    /// have no row header): from each offset, `size(record)` bytes.
    pub fn records<F: Fn(&[u8]) -> usize + 'a>(&self, size: F) -> impl Iterator<Item = Result<&'a [u8]>> + '_ {
        self.offsets.iter().map(move |&at| {
            let rest = self.buf.get(at..).unwrap_or(&[]);
            let n = size(rest);
            rest.get(..n)
                .ok_or_else(|| format_err!("record at {at:#x} reaches beyond its node"))
        })
    }

    /// Offset of the node header in its buffer.
    pub fn header(&self) -> usize {
        self.header
    }
}
