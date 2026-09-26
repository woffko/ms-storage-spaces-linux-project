//! The second parity of dual parity spaces, on the impulse pool written by
//! tools/vm/New-ImpulsePool.ps1 (7 columns; all-zero stripes with single
//! bytes set). Every unit read with any two disks failing must equal the
//! unit read from all disks.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use storage_spaces::Pool;
use storage_spaces::io::{ReadAt, SparseImage};

struct Failing {
    image: SparseImage,
    broken: Arc<AtomicBool>,
}

impl ReadAt for Failing {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        if self.broken.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("failed"));
        }
        self.image.read_exact_at(buf, offset)
    }

    fn size(&self) -> std::io::Result<u64> {
        self.image.size()
    }
}

#[test]
fn any_two_columns_are_rebuilt_from_p_and_q() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/imp7b");
    let images: Vec<SparseImage> = (0..7)
        .map(|i| SparseImage::read_from(File::open(dir.join(format!("disk{i}.fixture"))).unwrap()).unwrap())
        .collect();
    let stripe = 5 * 0x10000u64;
    let stripes = 15;
    let reference: Vec<u8> = {
        let pool = Pool::open(images.clone()).unwrap();
        let r = pool.open_space(pool.find_space("imp7b").unwrap().id()).unwrap();
        let mut v = vec![0u8; (stripe * stripes) as usize];
        r.read_exact_at(&mut v, 0).unwrap();
        v
    };
    assert!(reference.iter().any(|&b| b != 0));
    for a in 0..7 {
        for b in a + 1..7 {
            let flags: Vec<Arc<AtomicBool>> = (0..7).map(|_| Arc::new(AtomicBool::new(false))).collect();
            let devices: Vec<Failing> = images
                .iter()
                .zip(&flags)
                .map(|(image, f)| Failing {
                    image: image.clone(),
                    broken: f.clone(),
                })
                .collect();
            let pool = Pool::open(devices).unwrap();
            let r = pool.open_space(pool.find_space("imp7b").unwrap().id()).unwrap();
            flags[a].store(true, Ordering::Relaxed);
            flags[b].store(true, Ordering::Relaxed);
            let mut v = vec![0u8; reference.len()];
            r.read_exact_at(&mut v, 0)
                .unwrap_or_else(|e| panic!("disks {a}+{b}: {e}"));
            assert!(v == reference, "disks {a}+{b} rebuilt different data");
        }
    }
}
