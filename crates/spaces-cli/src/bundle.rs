//! A .tar.gz of a few files (`spaces check --bundle`), without a
//! dependency: ustar headers, and gzip with stored deflate blocks (the
//! bundle is small, and readable by every tar).

/// The files to pack: (path in the archive, contents).
pub type Files = Vec<(String, Vec<u8>)>;

/// `files` as a gzip-compressed tar archive.
pub fn tar_gz(files: &Files, mtime: u64) -> Vec<u8> {
    gzip(&tar(files, mtime))
}

fn tar(files: &Files, mtime: u64) -> Vec<u8> {
    let mut out = Vec::new();
    for (path, data) in files {
        let mut h = [0u8; 512];
        let name = path.as_bytes();
        let n = name.len().min(99);
        h[..n].copy_from_slice(&name[..n]);
        let octal = |h: &mut [u8; 512], at: usize, width: usize, v: u64| {
            let s = format!("{v:0w$o}", w = width - 1);
            h[at..at + width - 1].copy_from_slice(&s.as_bytes()[s.len() - (width - 1)..]);
        };
        octal(&mut h, 100, 8, 0o644);
        octal(&mut h, 108, 8, 0);
        octal(&mut h, 116, 8, 0);
        octal(&mut h, 124, 12, data.len() as u64);
        octal(&mut h, 136, 12, mtime);
        h[156] = b'0';
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        // The checksum counts its own field as spaces.
        h[148..156].fill(b' ');
        let sum: u64 = h.iter().map(|&b| u64::from(b)).sum();
        let s = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(s.as_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(data);
        out.resize(out.len().next_multiple_of(512), 0);
    }
    out.resize(out.len() + 1024, 0);
    out
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    let mut blocks = data.chunks(0xffff).peekable();
    if blocks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(block) = blocks.next() {
        out.push(u8::from(blocks.peek().is_none()));
        let len = block.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

/// CRC-32 (IEEE 802.3), as gzip uses it.
fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *t = c;
    }
    !data
        .iter()
        .fold(!0u32, |c, &b| table[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a gzip of stored blocks holds.
    fn gunzip(gz: &[u8]) -> Vec<u8> {
        assert_eq!(&gz[..3], &[0x1f, 0x8b, 8]);
        let mut out = Vec::new();
        let mut at = 10;
        loop {
            let last = gz[at] & 1 == 1;
            assert_eq!(gz[at] & 6, 0, "stored blocks only");
            let len = u16::from_le_bytes([gz[at + 1], gz[at + 2]]) as usize;
            assert_eq!(!len as u16, u16::from_le_bytes([gz[at + 3], gz[at + 4]]));
            out.extend_from_slice(&gz[at + 5..at + 5 + len]);
            at += 5 + len;
            if last {
                break;
            }
        }
        assert_eq!(u32::from_le_bytes(gz[at..at + 4].try_into().unwrap()), crc32(&out));
        assert_eq!(
            u32::from_le_bytes(gz[at + 4..at + 8].try_into().unwrap()) as usize,
            out.len()
        );
        out
    }

    #[test]
    fn packs_files_any_tar_reads() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        let big = vec![7u8; 70_000];
        let files: Files = vec![
            ("reports/a.txt".into(), b"healthy\n".to_vec()),
            ("gpt/a-start.bin".into(), big.clone()),
        ];
        let gz = tar_gz(&files, 1_700_000_000);
        let tar = gunzip(&gz);
        assert_eq!(tar.len() % 512, 0);
        assert_eq!(&tar[..13], b"reports/a.txt");
        assert_eq!(&tar[257..263], b"ustar\0");
        assert_eq!(&tar[124..135], b"00000000010");
        let sum: u64 = tar[..512]
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                if (148..156).contains(&i) {
                    u64::from(b' ')
                } else {
                    u64::from(b)
                }
            })
            .sum();
        assert_eq!(&tar[148..154], format!("{sum:06o}").as_bytes());
        assert_eq!(&tar[512..520], b"healthy\n");
        assert_eq!(&tar[1024..1039], b"gpt/a-start.bin");
        assert_eq!(&tar[1536..1536 + big.len()], &big[..]);
        // When tar is at hand, it lists both.
        let dir = std::env::temp_dir().join(format!("spaces-bundle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("b.tar.gz");
        std::fs::write(&path, &gz).unwrap();
        if let Ok(out) = std::process::Command::new("tar").arg("-tzf").arg(&path).output() {
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            assert_eq!(String::from_utf8_lossy(&out.stdout), "reports/a.txt\ngpt/a-start.bin\n");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
