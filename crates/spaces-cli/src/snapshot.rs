//! Snapshots of member disks taken on the Windows VM by
//! tools/vm/Invoke-Scenario.ps1 ("SSSNAP01"): the disk size, then runs of
//! 4 KiB pages, either stored (kind 1) or blocks of the verification
//! pattern (kind 2, regenerated here); zero pages are left out.

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use storage_spaces::testpattern::{BLOCK, fill_block_raw};

const MAGIC: &[u8; 8] = b"SSSNAP01";

/// Writes the disk a snapshot describes into a new sparse raw image.
pub fn to_raw(snapshot: &Path, output: &Path) -> Result<(u64, u64)> {
    let mut r = BufReader::new(File::open(snapshot).with_context(|| format!("cannot open {}", snapshot.display()))?);
    let out = File::create_new(output).with_context(|| format!("cannot create {}", output.display()))?;
    convert(&mut r, &out)
}

/// Returns the number of stored and of pattern pages.
fn convert(r: &mut impl Read, out: &File) -> Result<(u64, u64)> {
    let mut head = [0u8; 16];
    r.read_exact(&mut head)?;
    if &head[..8] != MAGIC {
        bail!("not a disk snapshot");
    }
    let size = u64::from_le_bytes(head[8..].try_into().unwrap());
    out.set_len(size)?;
    let (mut stored, mut pattern) = (0, 0);
    let mut kind = [0u8; 1];
    let mut page = vec![0u8; BLOCK];
    loop {
        match r.read_exact(&mut kind) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        let mut run = [0u8; 12];
        r.read_exact(&mut run)?;
        let offset = u64::from_le_bytes(run[..8].try_into().unwrap());
        let pages = u32::from_le_bytes(run[8..].try_into().unwrap()) as u64;
        if !offset.is_multiple_of(BLOCK as u64) || offset + pages * BLOCK as u64 > size {
            bail!("snapshot run at {offset:#x} outside the disk");
        }
        match kind[0] {
            1 => {
                for i in 0..pages {
                    r.read_exact(&mut page)?;
                    out.write_all_at(&page, offset + i * BLOCK as u64)?;
                }
                stored += pages;
            }
            2 => {
                let mut p = [0u8; 24];
                r.read_exact(&mut p)?;
                let start = u64::from_le_bytes(p[..8].try_into().unwrap());
                let tag: [u8; 16] = p[8..].try_into().unwrap();
                for i in 0..pages {
                    fill_block_raw(&mut page, start + i * BLOCK as u64, &tag);
                    out.write_all_at(&page, offset + i * BLOCK as u64)?;
                }
                pattern += pages;
            }
            k => bail!("unknown snapshot run kind {k}"),
        }
    }
    (&*out).flush()?;
    Ok((stored, pattern))
}

#[cfg(test)]
mod tests {
    use super::*;
    use storage_spaces::testpattern::fill_block;

    #[test]
    fn stored_and_pattern_runs_become_the_disk() {
        let mut snap = Vec::new();
        snap.extend_from_slice(MAGIC);
        snap.extend_from_slice(&(64 * 1024u64).to_le_bytes());
        // One stored page at 8 KiB.
        snap.push(1);
        snap.extend_from_slice(&0x2000u64.to_le_bytes());
        snap.extend_from_slice(&1u32.to_le_bytes());
        snap.extend_from_slice(&[0xab; BLOCK]);
        // Two pattern pages at 16 KiB holding pattern offsets 1 MiB on.
        snap.push(2);
        snap.extend_from_slice(&0x4000u64.to_le_bytes());
        snap.extend_from_slice(&2u32.to_le_bytes());
        snap.extend_from_slice(&0x100000u64.to_le_bytes());
        snap.extend_from_slice(b"tag\0\0\0\0\0\0\0\0\0\0\0\0\0");

        let dir = std::env::temp_dir().join(format!("spaces-snap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disk.img");
        let _ = std::fs::remove_file(&path);
        let out = File::create_new(&path).unwrap();
        assert_eq!(convert(&mut snap.as_slice(), &out).unwrap(), (1, 2));
        let disk = std::fs::read(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(disk.len(), 64 * 1024);
        assert!(disk[..0x2000].iter().all(|&b| b == 0));
        assert!(disk[0x2000..0x3000].iter().all(|&b| b == 0xab));
        let mut expected = vec![0u8; BLOCK];
        fill_block(&mut expected, 0x101000, "tag");
        assert_eq!(disk[0x5000..0x6000], expected[..]);
        assert!(disk[0x6000..].iter().all(|&b| b == 0));
    }
}
