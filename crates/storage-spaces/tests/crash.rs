//! Pools whose disks were pulled while Windows was writing (see
//! tools/vm/New-CrashPool.ps1). Every MiB we read from the crashed disks must
//! either equal what Windows shows after recovering the pool, or be refused;
//! silently different data is a failure.

mod common;

use std::fs::{self, File};
use std::path::Path;

use storage_spaces::{OpenOptions, Pool, UncleanParity};

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *t = c;
    }
    !data
        .iter()
        .fold(!0u32, |c, &b| table[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8))
}

#[test]
fn crashed_pools_read_like_windows_recovers_them() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/crash");
    let Ok(entries) = fs::read_dir(&root) else {
        eprintln!("no crash pools in {}, skipping", root.display());
        return;
    };
    let mut dirs: Vec<_> = entries
        .map(|e| e.unwrap().path())
        // Hidden directories are downloads in progress.
        .filter(|p| p.join("manifest.json").exists() && !p.file_name().unwrap().to_string_lossy().starts_with('.'))
        .collect();
    dirs.sort();
    for dir in dirs {
        let m = common::manifest(&dir);
        let name = m["space"]["name"].as_str().unwrap();
        let want: Vec<&str> = m["recovered_mib_crc32"].as_str().unwrap().split_whitespace().collect();
        let n = m["disks"].as_array().unwrap().len();
        let files = (0..n)
            .map(|i| File::open(dir.join(format!("disk{i}.img"))).unwrap())
            .collect();
        let pool = Pool::open(files).unwrap();
        let id = pool.find_space(name).unwrap().id();
        let strict = pool.open_space(id).unwrap();
        let lenient = pool
            .open_space_with(
                id,
                OpenOptions {
                    unclean_parity: UncleanParity::PreferData,
                },
            )
            .unwrap();
        let mut buf = vec![0u8; 1 << 20];
        let (mut refused, mut lenient_diff) = (0, 0);
        for (i, w) in want.iter().enumerate() {
            let offset = (i as u64) << 20;
            match strict.read_exact_at(&mut buf, offset) {
                Ok(()) => assert_eq!(
                    format!("{:08x}", crc32(&buf)),
                    *w,
                    "{name}: MiB {i} differs from Windows"
                ),
                Err(_) => refused += 1,
            }
            lenient.read_exact_at(&mut buf, offset).unwrap();
            if format!("{:08x}", crc32(&buf)) != *w {
                lenient_diff += 1;
            }
        }
        eprintln!(
            "{name}: {} MiB, refused {refused}, differing when preferring data {lenient_diff}",
            want.len()
        );
        assert!(refused <= 4, "{name}: {refused} MiB refused");
    }
}
