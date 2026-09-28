//! Decoding of single database records: type, record version, body. Records
//! that decode must survive the encoders: extents re-encode to the same
//! record, and an edited space record decodes to the edit.
#![no_main]

use libfuzzer_sys::fuzz_target;
use storage_spaces::format::{
    Cursor, RawRecord, Record, SPACE_SECURITY_DESCRIPTOR, SpaceEdit, edit_space_record, encode_string, encode_varint,
};

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let raw = RawRecord {
        id: 1,
        kind: data[0] % 8,
        version: data[1],
        body: data[2..].to_vec(),
    };
    match Record::decode(&raw) {
        Ok(Record::Extent(e)) => {
            let again = RawRecord {
                body: e.encode(u64::from(data[1])),
                ..raw.clone()
            };
            match Record::decode(&again) {
                Ok(Record::Extent(back)) => assert_eq!(format!("{back:?}"), format!("{e:?}")),
                other => panic!("re-encoded extent decodes as {other:?}"),
            }
        }
        Ok(Record::Space(s)) if !s.is_child && s.policy.is_some() => {
            let edit = SpaceEdit {
                sequence: u64::from(data[1]) << 20,
                name: Some("edited"),
                size: Some(u64::from(data[0]) << 30),
                security_descriptor: Some(&SPACE_SECURITY_DESCRIPTOR),
            };
            if let Ok(body) = edit_space_record(&raw.body, &edit) {
                let again = RawRecord { body, ..raw.clone() };
                match Record::decode(&again) {
                    Ok(Record::Space(back)) => {
                        assert_eq!(back.name, "edited");
                        assert_eq!(back.size, edit.size);
                        assert_eq!(back.guid, s.guid);
                        assert_eq!(format!("{:?}", back.policy), format!("{:?}", s.policy));
                    }
                    other => panic!("edited space decodes as {other:?}"),
                }
            }
        }
        _ => {}
    }
    // Integers and strings read back as written.
    let v = u64::from_le_bytes(
        data.iter()
            .copied()
            .chain([0; 8])
            .take(8)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap(),
    );
    assert_eq!(Cursor::new(&encode_varint(v)).varint().unwrap(), v);
    let text = String::from_utf8_lossy(&data[2..]).replace('\0', "");
    assert_eq!(Cursor::new(&encode_string(&text)).string().unwrap(), text);
});
