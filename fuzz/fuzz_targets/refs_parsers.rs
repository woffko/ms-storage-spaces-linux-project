//! The ReFS parsers that take raw bytes: the boot sector (with its
//! checksum), page references and headers, B+-tree nodes with their rows
//! and raw records, from any descriptor offset the input names, and the
//! LZ4 blocks of compressed containers.
#![no_main]

use libfuzzer_sys::fuzz_target;
use refs::boot::BootSector;
use refs::node::Node;
use refs::page::{PageHeader, PageRef};

fuzz_target!(|data: &[u8]| {
    if let Ok(boot) = BootSector::parse(data) {
        let _ = (boot.cluster_size(), boot.volume_size());
    }
    if let Ok(r) = PageRef::parse(data) {
        let _ = r.verifies(data);
    }
    let _ = PageHeader::parse(data);
    let Some((&at, rest)) = data.split_first() else {
        return;
    };
    if let Ok(out) = refs::compress::lz4_block(rest, usize::from(at) * 256) {
        assert_eq!(out.len(), usize::from(at) * 256);
    }
    for node in [
        Node::at(rest, usize::from(at)),
        Node::with_header(rest, usize::from(at)),
    ]
    .into_iter()
    .flatten()
    {
        let _ = (node.is_leaf(), node.len(), node.header());
        for row in node.rows().take(1000) {
            let Ok(row) = row else { break };
            let _ = PageRef::parse(row.value);
        }
        for record in node
            .records(|r| {
                r.get(0x0a..0x0c)
                    .map_or(0, |b| usize::from(u16::from_le_bytes([b[0], b[1]])))
            })
            .take(1000)
        {
            if record.is_err() {
                break;
            }
        }
    }
});
