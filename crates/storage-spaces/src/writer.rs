//! Writes to spaces (Stage 2 of docs/plan.md).
//!
//! A space opens for writing only when everything about its state is
//! understood, and only for what the writes can keep consistent the way
//! Windows would: so far in-place writes to allocated rows of simple spaces,
//! which change no metadata. Everything else is refused with the reason.

use crate::error::{Error, Result};
use crate::format::Resiliency;
use crate::io::WriteAt;
use crate::layout::Condition;
use crate::pool::Pool;
use crate::reader::{OpenOptions, SpaceReader};

/// Writes to a space; reads go through [`SpaceWriter::reader`].
pub struct SpaceWriter<'p, D> {
    reader: SpaceReader<'p, D>,
}

impl<'p, D: WriteAt> SpaceWriter<'p, D> {
    pub(crate) fn new(pool: &'p Pool<D>, id: u64) -> Result<Self> {
        let refuse = |why: String| {
            Err(Error::Unsupported(format!(
                "space {id} cannot be opened for writing: {why}"
            )))
        };
        if !pool.warnings.is_empty() {
            return refuse(format!(
                "the pool is not in a clean state ({})",
                pool.warnings.join("; ")
            ));
        }
        let reader = pool.open_space_with(id, OpenOptions::default())?;
        if reader.condition() != Condition::Healthy {
            return refuse(format!("it is {:?}", reader.condition()).to_lowercase());
        }
        if pool.children(id).any(|c| c.info.is_child && !c.extents.is_empty()) {
            return refuse("writes to tiered spaces are not supported yet".into());
        }
        let layout = reader.layout();
        if layout.resiliency != Resiliency::Simple {
            return refuse(format!("writes to {:?} spaces are not supported yet", layout.resiliency).to_lowercase());
        }
        if let Some(cache) = reader.cache()
            && (cache.cached_chunks() > 0 || cache.conflicting_chunks() > 0)
        {
            return refuse(format!(
                "its write-back cache holds {} chunks (writing through the cache is not supported yet)",
                cache.cached_chunks()
            ));
        }
        Ok(SpaceWriter { reader })
    }

    pub fn reader(&self) -> &SpaceReader<'p, D> {
        &self.reader
    }

    pub fn size(&self) -> u64 {
        self.reader.size()
    }

    /// Writes all of `buf` at `offset` of the space. A write into a row a
    /// thin space has not allocated is refused (allocation is not
    /// supported yet).
    pub fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> Result<()> {
        let size = self.size();
        if offset.checked_add(buf.len() as u64).is_none_or(|end| end > size) {
            return Err(Error::Pool(format!(
                "write of {} bytes at {offset:#x} beyond the end of the space ({size:#x})",
                buf.len()
            )));
        }
        let layout = self.reader.layout();
        let pool = self.reader.pool();
        while !buf.is_empty() {
            let loc = layout.locate(offset);
            let n = buf.len().min(loc.contiguous as usize);
            let Some((disk, slab)) = layout.physical(loc.column, 0, loc.row) else {
                return Err(Error::Unsupported(format!(
                    "write at {offset:#x} into a row the thin space has not allocated (allocation is not supported yet)"
                )));
            };
            if !pool.write_slab(disk, slab, loc.offset_in_slab, &buf[..n])? {
                return Err(Error::Pool(format!("disk {disk} of the space is not present")));
            }
            buf = &buf[n..];
            offset += n as u64;
        }
        Ok(())
    }

    /// Makes the writes so far durable on every member.
    pub fn flush(&self) -> Result<()> {
        self.reader.pool().flush_members()
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::path::Path;

    use crate::io::{Overlay, SparseImage};
    use crate::pool::Pool;
    use crate::testpattern::{BLOCK, fill_block};

    fn fixture(name: &str) -> Pool<Overlay<SparseImage>> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
        let disks = (0..)
            .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
            .map(|f| Overlay::new(SparseImage::read_from(f).unwrap()))
            .collect();
        Pool::open(disks).unwrap()
    }

    #[test]
    fn writes_to_a_simple_space_read_back_and_land_on_its_columns() {
        let pool = fixture("simple2c");
        let space = pool.find_space("simple2c").unwrap();
        let w = pool.open_space_rw(space.id()).unwrap();
        // Blocks across an interleave boundary and in the second row.
        let mut data = vec![0u8; 5 * BLOCK];
        for (i, block) in data.chunks_mut(BLOCK).enumerate() {
            fill_block(block, (i * BLOCK) as u64, "linux");
        }
        for offset in [0, 60 << 10, (256 << 20) + (4 << 10)] {
            w.write_all_at(&data, offset).unwrap();
            let mut back = vec![0u8; data.len()];
            w.reader().read_exact_at(&mut back, offset).unwrap();
            assert!(back == data, "at {offset:#x}");
        }
        w.flush().unwrap();
        // A reader opened afterwards sees the writes too.
        let r = pool.open_space(space.id()).unwrap();
        let mut back = vec![0u8; data.len()];
        r.read_exact_at(&mut back, 60 << 10).unwrap();
        assert!(back == data);
        assert!(w.write_all_at(&data, w.size() - BLOCK as u64).is_err());
    }

    #[test]
    fn spaces_that_writes_cannot_keep_consistent_are_refused() {
        for (pool, space, why) in [
            ("mirror2", "mirror2", "mirror spaces"),
            ("parity3", "parity3", "parity spaces"),
            ("tiered", "tiered", "tiered spaces"),
        ] {
            let pool = fixture(pool);
            let id = pool.find_space(space).unwrap().id();
            let err = pool.open_space_rw(id).err().unwrap().to_string();
            assert!(err.contains(why), "{space}: {err}");
        }
        // A missing disk makes the pool unclean.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/simple2c");
        let one = vec![Overlay::new(
            SparseImage::read_from(File::open(dir.join("disk0.fixture")).unwrap()).unwrap(),
        )];
        let pool = Pool::open(one).unwrap();
        let id = pool.find_space("simple2c").unwrap().id();
        assert!(
            pool.open_space_rw(id)
                .err()
                .unwrap()
                .to_string()
                .contains("not in a clean state")
        );
    }
}
