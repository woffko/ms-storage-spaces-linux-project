//! Predictions of what Windows writes, checked against the states of the
//! scenarios in tools/scenarios.sh (fixtures captured from the snapshots
//! tools/vm/Invoke-Scenario.ps1 took between the steps).

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

use storage_spaces::Pool;
use storage_spaces::drt::{DirtyRegions, DrtWriter};
use storage_spaces::format::SLAB_SIZE;
use storage_spaces::io::SparseImage;

fn state(scenario: &str, label: &str) -> Pool<SparseImage> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/scenarios")
        .join(scenario)
        .join(label);
    let mut disks = Vec::new();
    while let Ok(f) = File::open(dir.join(format!("disk{}.fixture", disks.len()))) {
        disks.push(SparseImage::read_from(f).unwrap());
    }
    Pool::open(disks).unwrap()
}

/// The dirty region log of the only user space, the size of its tracking
/// space (the second header copy sits 8 KiB before its end) and the virtual
/// slab where the extent run holding each byte offset starts.
fn log(pool: &Pool<SparseImage>) -> (DirtyRegions, u64, impl Fn(u64) -> u64 + '_) {
    let space = pool.user_spaces().next().unwrap();
    let reader = pool.open_space(space.id()).unwrap();
    let log = reader.dirty_regions().unwrap().clone();
    let size = log.copies()[1].offset + 0x2000;
    let layout = reader.layout().clone();
    let run_of = move |offset: u64| layout.run_start_offset(layout.locate(offset).row) / SLAB_SIZE;
    (log, size, run_of)
}

/// The steps of a scenario with their start in seconds of the day (UTC).
fn steps(scenario: &str) -> Vec<(String, f64)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/scenarios")
        .join(scenario)
        .join("scenario.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let steps: Vec<(String, f64)> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let t = &e["started"].as_str().unwrap()[11..];
            let secs = t[..2].parse::<f64>().unwrap() * 3600.0
                + t[3..5].parse::<f64>().unwrap() * 60.0
                + t[6..].split('+').next().unwrap().parse::<f64>().unwrap();
            (e["step"].as_str().unwrap().to_string(), secs)
        })
        .collect();
    assert!(
        steps.windows(2).all(|w| w[0].1 <= w[1].1),
        "{scenario} crosses midnight"
    );
    steps
}

/// Seconds without writes after which Windows leaves a run out of the next
/// header it writes: runs idle for 29 s were kept (m5drt), runs idle for
/// 35 s dropped (m5drt2).
const CLEAN_AFTER: f64 = 32.0;

/// Replays the writes, disconnects and reconnects of a scenario on
/// [`DrtWriter`], the model of the mirror dirty region log, and compares
/// both header pages with every snapshot, byte for byte. `first` is the
/// snapshot to start from, or `None` for a new space. Returns the number of
/// snapshots compared.
fn replay_dirty_region_log(scenario: &str, first: Option<&str>) -> usize {
    let steps = steps(scenario);
    let label = |s: &str| {
        s.strip_prefix("snap:")
            .map(|l| l.split(':').next().unwrap().to_string())
    };
    let any = steps.iter().find_map(|(s, _)| label(s)).unwrap();
    let pool = state(scenario, &any);
    let (_, size, run_of) = log(&pool);
    let mut writer = match first {
        Some(l) => log(&state(scenario, l)).0.writer(),
        None => DrtWriter::new(),
    };
    let mut pages = [writer.page(), writer.page()];
    if let Some(l) = first {
        let (disk, _, _) = log(&state(scenario, l));
        pages = [disk.copies()[0].page.clone(), disk.copies()[1].page.clone()];
    }
    let mut last_write: HashMap<u64, f64> = HashMap::new();
    let mut checked = 0;
    let mut started = first.is_none();
    for (step, at) in &steps {
        let a: Vec<&str> = step.split(':').collect();
        if !started {
            started = label(step).as_deref() == first;
            continue;
        }
        match a[0] {
            "write" => {
                let offset = a[2].parse::<u64>().unwrap() * 1024;
                let len = a[3].parse::<u64>().unwrap() * 1024;
                for run in run_of(offset)..=run_of(offset + len - 1) {
                    if !writer.runs().contains(&run) {
                        writer.clean(|r| last_write.get(&r).is_some_and(|&t| at - t > CLEAN_AFTER));
                        let (at_end, page) = writer.write(run).unwrap();
                        pages[usize::from(at_end)] = page;
                    }
                    last_write.insert(run, *at);
                }
            }
            "disconnect" => {
                let page = writer.disconnect();
                pages = [page.clone(), page];
                last_write.clear();
            }
            // Attaching loads the log from the newest copy.
            "connect" => {
                writer = DirtyRegions::load(size, |off, buf| {
                    buf.copy_from_slice(&pages[usize::from(off != 0)]);
                    Ok(())
                })
                .unwrap()
                .unwrap()
                .writer();
            }
            "snap" => {
                let (windows, _, _) = log(&state(scenario, a[1]));
                for (i, c) in windows.copies().iter().enumerate() {
                    assert!(
                        c.page == pages[i],
                        "{scenario} {}: copy at {:#x}\n Windows {}\n model   {}",
                        a[1],
                        c.offset,
                        hex(&c.page[..0x40]),
                        hex(&pages[i][..0x40])
                    );
                }
                checked += 1;
            }
            _ => {}
        }
    }
    checked
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// m5drt: a two-way mirror of four 256 MiB extent runs written in the order
/// 0, 2, 0, 1, 3, then disconnected, reconnected and written in run 1.
/// m5drt2: writes into runs 0 to 6 after 5 to 180 s without writes.
/// Windows wrote exactly the header pages the model predicts, stale entries
/// included: the first write into a run adds it in the next generation (odd
/// generations at the end copy, even ones at the start), after the runs idle
/// for longer than about 30 s were removed; a write into a listed run
/// changes nothing; a disconnect removes every run and writes generation 0
/// into both copies; attaching loads the listed runs of the newest copy.
#[test]
fn mirror_dirty_region_log_follows_the_writes() {
    assert_eq!(replay_dirty_region_log("m5drt", Some("s0")), 7);
    assert_eq!(replay_dirty_region_log("m5drt2", None), 7);
}
