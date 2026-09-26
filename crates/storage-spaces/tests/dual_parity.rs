//! The parity codes of dual parity spaces, on impulse pools written by
//! tools/vm/New-ImpulsePool.ps1 (all-zero stripes with single bytes set):
//! 7 to 10 columns (P and Q) and 11+ columns (local reconstruction code).
//! Every unit read with any two disks failing must equal the unit read from
//! all disks, and the impulses must be where the manifest puts them.

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

fn manifest(dir: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
    serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap()
}

#[test]
fn any_two_columns_are_rebuilt() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let mut dirs: Vec<_> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().path()).collect();
    dirs.sort();
    for dir in dirs {
        let m = manifest(&dir);
        let name = m["space"]["name"].as_str().unwrap();
        let columns = m["space"]["columns"].as_u64().unwrap();
        let n = m["disks"].as_array().unwrap().len();
        let images: Vec<SparseImage> = (0..n)
            .map(|i| SparseImage::read_from(File::open(dir.join(format!("disk{i}.fixture"))).unwrap()).unwrap())
            .collect();
        let stripe = m["space"]["stripe_size"].as_u64().unwrap_or((columns - 2) * 0x10000);
        let stripes = m["impulses"].as_array().unwrap().len() as u64;
        let reference: Vec<u8> = {
            let pool = Pool::open(images.clone()).unwrap();
            let r = pool.open_space(pool.find_space(name).unwrap().id()).unwrap();
            let mut v = vec![0u8; (stripe * stripes) as usize];
            r.read_exact_at(&mut v, 0).unwrap();
            v
        };
        let mut expected = vec![0u8; reference.len()];
        for (s, list) in m["impulses"].as_array().unwrap().iter().enumerate() {
            for imp in list.as_array().unwrap() {
                let v: Vec<u64> = imp.as_array().unwrap().iter().map(|x| x.as_u64().unwrap()).collect();
                expected[(s as u64 * stripe + v[0] * 0x10000 + v[1]) as usize] = v[2] as u8;
            }
        }
        assert!(expected.iter().any(|&b| b != 0), "{name}");
        assert!(
            reference == expected,
            "{name}: impulses not where the manifest puts them"
        );
        for a in 0..n {
            for b in a + 1..n {
                let flags: Vec<Arc<AtomicBool>> = (0..n).map(|_| Arc::new(AtomicBool::new(false))).collect();
                let devices: Vec<Failing> = images
                    .iter()
                    .zip(&flags)
                    .map(|(image, f)| Failing {
                        image: image.clone(),
                        broken: f.clone(),
                    })
                    .collect();
                let pool = Pool::open(devices).unwrap();
                let r = pool.open_space(pool.find_space(name).unwrap().id()).unwrap();
                flags[a].store(true, Ordering::Relaxed);
                flags[b].store(true, Ordering::Relaxed);
                let mut v = vec![0u8; reference.len()];
                r.read_exact_at(&mut v, 0)
                    .unwrap_or_else(|e| panic!("{name}: disks {a}+{b}: {e}"));
                assert!(v == reference, "{name}: disks {a}+{b} rebuilt different data");
            }
        }
    }
}
