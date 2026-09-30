//! The complete record models (`storage_spaces::records`) and the disk
//! header: whatever decodes encodes back to the same bytes, so that a
//! management operation never changes a field it does not mean to.
#![no_main]

use libfuzzer_sys::fuzz_target;
use storage_spaces::format::DiskHeader;
use storage_spaces::records::{DiskBody, PoolBody, SpaceBody};

fuzz_target!(|data: &[u8]| {
    let Some((&selector, body)) = data.split_first() else { return };
    match selector % 5 {
        0 | 1 => {
            if let Ok(p) = PoolBody::decode(15 + selector % 2, body) {
                assert_eq!(p.encode().expect("a decoded pool record encodes"), body);
            }
        }
        2 => {
            if let Ok(d) = DiskBody::decode(body) {
                assert_eq!(d.encode(), body);
            }
        }
        3 => {
            for child in [false, true] {
                if let Ok(s) = SpaceBody::decode(child, body) {
                    assert_eq!(s.encode().expect("a decoded space record encodes"), body, "child {child}");
                }
            }
        }
        _ => {
            if let Ok(h) = DiskHeader::parse(body) {
                let again = DiskHeader::parse(&h.encode()).expect("an encoded header parses");
                assert_eq!(format!("{again:?}"), format!("{h:?}"));
            }
        }
    }
});
