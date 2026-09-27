//! GPT and MBR parsing and the search for the Storage Spaces partition on an
//! arbitrary device.
#![no_main]

use libfuzzer_sys::fuzz_target;
use storage_spaces::gpt::{find_spaces_partition, read_partitions};
use storage_spaces::io::MemDevice;

fuzz_target!(|data: &[u8]| {
    let dev = MemDevice(data.to_vec());
    for sector in [512, 4096] {
        let _ = read_partitions(&dev, sector);
    }
    let _ = find_spaces_partition(&dev);
});
