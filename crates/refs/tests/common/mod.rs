//! Comparing a volume with Windows' listing of it (manifest.json of
//! tools/vm/New-RefsVolume.ps1, or of `refs fixture`).

use std::collections::BTreeMap;

use refs::checksum::sha256;
use refs::{Target, Volume};
use serde_json::Value;
use storage_spaces::io::ReadAt;

pub fn manifest(text: &str) -> Value {
    serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap()
}

fn hex(d: [u8; 32]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Every entry under `oid`, keyed by its "/"-separated path, except what
/// is under the directories in `skip`.
fn tree<D: ReadAt>(vol: &Volume<D>, oid: u64, prefix: &str, skip: &[&str], out: &mut BTreeMap<String, refs::Entry>) {
    for e in vol.read_dir(oid).unwrap() {
        let path = format!("{prefix}{}", e.name);
        // Links to directories are listed, not followed.
        if let (Target::Directory(child), false) = (&e.target, e.attributes & 0x400 != 0)
            && !skip.contains(&path.as_str())
        {
            tree(vol, *child, &format!("{path}/"), skip, out);
        }
        out.insert(path, e);
    }
}

fn read_all<D: ReadAt>(vol: &Volume<D>, stream: &refs::Stream) -> Vec<u8> {
    let mut data = vec![0u8; stream.size as usize];
    let mut at = 0;
    while at < data.len() {
        let n = vol.read_stream(stream, at as u64, &mut data[at..]).unwrap();
        assert!(n > 0);
        at += n;
    }
    data
}

/// What differs between the volume and the manifest: every listed file and
/// directory with its kind, attributes, times, link target and size, the
/// data and named streams where the manifest has their SHA-256, and
/// entries the manifest does not list. Prints a summary line.
pub fn compare<D: ReadAt>(vol: &Volume<D>, manifest: &Value) -> Vec<String> {
    let name = manifest["name"].as_str().unwrap();
    assert_eq!(vol.cluster, manifest["cluster_size"].as_u64().unwrap(), "{name}");
    // Fixtures leave out what is under some directories.
    let skip: Vec<&str> = manifest["fixture_excluded"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut entries = BTreeMap::new();
    tree(vol, refs::volume::ROOT_DIRECTORY, "", &skip, &mut entries);
    entries.retain(|p, _| p != "System Volume Information" && !p.starts_with("System Volume Information/"));
    let mut problems = Vec::new();
    let listed = manifest["entries"].as_array().unwrap();
    for m in listed {
        let path = m["path"].as_str().unwrap();
        let Some(e) = entries.get(path) else {
            problems.push(format!("{path}: missing"));
            continue;
        };
        if (m["kind"] == "dir") != (e.attributes & 0x10 != 0) {
            problems.push(format!("{path}: kind"));
        }
        if m["attributes"].as_u64().unwrap() as u32 != e.attributes {
            problems.push(format!("{path}: attributes {:#x} != {}", e.attributes, m["attributes"]));
        }
        for (field, ours) in [("created", e.times.created), ("written", e.times.modified)] {
            if m[field].as_u64().unwrap() != ours {
                problems.push(format!("{path}: {field} {ours} != {}", m[field]));
            }
        }
        if let Some(windows) = m["link_target"].as_str() {
            // Windows shows the print name of links, the substitute
            // name without "\??\" of junctions.
            let target = vol
                .open_file(e)
                .ok()
                .and_then(|f| f.reparse)
                .and_then(|r| r.link_target());
            let shown = target.map(|t| {
                if t.print.is_empty() {
                    t.substitute.trim_start_matches(r"\??\").to_owned()
                } else {
                    t.print
                }
            });
            if shown.as_deref() != Some(windows) {
                problems.push(format!("{path}: link to {shown:?}, Windows {windows:?}"));
            }
            continue;
        }
        if m["kind"] != "file" {
            continue;
        }
        if m["size"].as_u64().unwrap() != e.size {
            problems.push(format!("{path}: size {} != {}", e.size, m["size"]));
        }
        let file = match vol.open_file(e) {
            Ok(f) => f,
            Err(err) => {
                problems.push(format!("{path}: {err}"));
                continue;
            }
        };
        if file.data.as_ref().map_or(0, |s| s.size) != e.size {
            problems.push(format!(
                "{path}: data stream of {:?} bytes",
                file.data.as_ref().map(|s| s.size)
            ));
        }
        if let Some(sha) = m["sha256"].as_str() {
            let data = file.data.as_ref().map(|s| read_all(vol, s)).unwrap_or_default();
            if hex(sha256(&data)) != sha {
                problems.push(format!("{path}: data differs ({} bytes)", data.len()));
            }
        }
        for s in m["streams"].as_array().into_iter().flatten() {
            let sname = s["name"].as_str().unwrap();
            // Windows lists stream snapshots as "<name>:$SNAPSHOT".
            let found = match sname.strip_suffix(":$SNAPSHOT") {
                Some(snapshot) => file.snapshots.iter().find(|(n, _)| n == snapshot),
                None => file.streams.iter().find(|(n, _)| n == sname),
            };
            let Some((_, stream)) = found else {
                problems.push(format!("{path}:{sname}: missing"));
                continue;
            };
            if Some(stream.size) != s["size"].as_u64() {
                problems.push(format!("{path}:{sname}: size {} != {}", stream.size, s["size"]));
            }
            if let Some(sha) = s["sha256"].as_str()
                && hex(sha256(&read_all(vol, stream))) != sha
            {
                problems.push(format!("{path}:{sname}: data differs"));
            }
        }
    }
    for p in entries.keys() {
        if !listed.iter().any(|m| m["path"].as_str() == Some(p.as_str())) {
            problems.push(format!("{p}: not listed by Windows"));
        }
    }
    eprintln!(
        "{name}: {} entries listed by Windows, {} read, {} problems",
        listed.len(),
        entries.len(),
        problems.len()
    );
    for p in problems.iter().take(30) {
        eprintln!("  {p}");
    }
    problems
}
