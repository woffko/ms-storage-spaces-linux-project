//! Health reports: the checks made before a space is attached or a file
//! system is mounted, each with its status and evidence, and the verdict
//! they add up to (see [`crate::guard`]).
//!
//! The verdict decides what may happen without being asked
//! ([`Verdict::Healthy`] only) and which flag allows reading anyway
//! ([`Verdict::flag`]). Check ids and status words are stable: scripts may
//! rely on them (`spaces check --json`).

use std::fmt::Write as _;

/// The result of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    Ok,
    /// Worth knowing, no problem (cached chunks, runs being written).
    Info,
    /// Something that differs from the ideal, but does not affect what is
    /// read (an older copy of the pool database that is not used).
    Warning,
    /// The check does not apply, or could not be made; the summary says why.
    Skipped,
    /// Windows' metadata shows redundancy reduced, but the data is
    /// complete.
    Degraded,
    /// Our cross-checks disagree: what would be read may not be what
    /// Windows reads.
    Suspect,
    /// The structure is broken or not understood, or data is lost.
    Failed,
}

impl Status {
    pub const ALL: [Status; 7] = [
        Status::Ok,
        Status::Info,
        Status::Warning,
        Status::Skipped,
        Status::Degraded,
        Status::Suspect,
        Status::Failed,
    ];

    /// The stable word for the status.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Info => "info",
            Status::Warning => "warning",
            Status::Skipped => "skipped",
            Status::Degraded => "degraded",
            Status::Suspect => "suspect",
            Status::Failed => "failed",
        }
    }

    /// The verdict a check of this status leads to: warnings, information
    /// and skipped checks leave a report healthy.
    pub fn verdict(self) -> Verdict {
        match self {
            Status::Ok | Status::Info | Status::Warning | Status::Skipped => Verdict::Healthy,
            Status::Degraded => Verdict::Degraded,
            Status::Suspect => Verdict::Suspect,
            Status::Failed => Verdict::Failed,
        }
    }

    /// Whether the status makes the report anything but healthy.
    pub fn is_problem(self) -> bool {
        self.verdict() != Verdict::Healthy
    }
}

/// What a report adds up to: the worst status of its checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Verdict {
    /// Every check passed (warnings allowed): attached and mounted
    /// automatically, written when asked.
    Healthy,
    /// Redundancy reduced, the data complete: read-only, with `--degraded`.
    Degraded,
    /// Our checks disagree with what was read: read-only, with `--force`.
    Suspect,
    /// Not read at all.
    Failed,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Healthy => "healthy",
            Verdict::Degraded => "degraded",
            Verdict::Suspect => "suspect",
            Verdict::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Verdict> {
        [Verdict::Healthy, Verdict::Degraded, Verdict::Suspect, Verdict::Failed]
            .into_iter()
            .find(|v| v.as_str().eq_ignore_ascii_case(s))
    }

    /// The flag that allows reading anyway, read-only: none for healthy
    /// (nothing to allow) and for failed (nothing allows it).
    pub fn flag(self) -> Option<&'static str> {
        match self {
            Verdict::Degraded => Some("--degraded"),
            Verdict::Suspect => Some("--force"),
            Verdict::Healthy | Verdict::Failed => None,
        }
    }

    /// Whether reading is allowed with the flags given (`--force` allows
    /// what `--degraded` does).
    pub fn allowed(self, degraded: bool, force: bool) -> bool {
        match self {
            Verdict::Healthy => true,
            Verdict::Degraded => degraded || force,
            Verdict::Suspect => force,
            Verdict::Failed => false,
        }
    }
}

/// Where the truth a check compares with comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truth {
    /// Windows' own metadata: what Windows recorded about the pool, its
    /// disks and spaces.
    Windows,
    /// Our cross-check: copies Windows keeps of the same structure, which
    /// must agree, or data checked against its own checksum or parity.
    CrossCheck,
    /// An invariant the format implies (a structure that lies inside what
    /// holds it, extents that do not overlap).
    Format,
}

impl Truth {
    pub fn as_str(self) -> &'static str {
        match self {
            Truth::Windows => "windows",
            Truth::CrossCheck => "cross-check",
            Truth::Format => "format",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Truth::Windows => "Windows' metadata",
            Truth::CrossCheck => "our cross-check of copies Windows keeps or of checksums",
            Truth::Format => "an invariant of the on-disk format",
        }
    }
}

/// One piece of evidence: what was compared, where it is, and what was
/// expected and found (bytes in hex).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Evidence {
    pub what: String,
    /// Device and byte offset, LBA, and for an offset in a space the slab,
    /// disk and physical offset that hold it.
    pub location: Option<String>,
    pub expected: Option<String>,
    pub found: Option<String>,
}

impl Evidence {
    pub fn new(what: impl Into<String>) -> Evidence {
        Evidence {
            what: what.into(),
            ..Default::default()
        }
    }

    pub fn at(mut self, location: impl Into<String>) -> Evidence {
        self.location = Some(location.into());
        self
    }

    pub fn expected(mut self, expected: impl Into<String>) -> Evidence {
        self.expected = Some(expected.into());
        self
    }

    pub fn found(mut self, found: impl Into<String>) -> Evidence {
        self.found = Some(found.into());
        self
    }
}

/// One check: a stable id (`pool.quorum`, `space.partitions`,
/// `fs.refs.superblock`, ...), its status and a one-line summary, with the
/// evidence, where the truth comes from and what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub id: String,
    pub status: Status,
    pub summary: String,
    pub evidence: Vec<Evidence>,
    pub truth: Option<Truth>,
    /// The consequence and what to do (for a status other than ok).
    pub advice: Option<String>,
}

impl Check {
    pub fn new(id: impl Into<String>, status: Status, summary: impl Into<String>) -> Check {
        Check {
            id: id.into(),
            status,
            summary: summary.into(),
            evidence: Vec::new(),
            truth: None,
            advice: None,
        }
    }

    pub fn ok(id: impl Into<String>, summary: impl Into<String>) -> Check {
        Check::new(id, Status::Ok, summary)
    }

    pub fn skipped(id: impl Into<String>, why: impl Into<String>) -> Check {
        Check::new(id, Status::Skipped, why)
    }

    pub fn evidence(mut self, e: Evidence) -> Check {
        self.evidence.push(e);
        self
    }

    pub fn truth(mut self, truth: Truth) -> Check {
        self.truth = Some(truth);
        self
    }

    pub fn advice(mut self, advice: impl Into<String>) -> Check {
        self.advice = Some(advice.into());
        self
    }
}

/// The checks of one subject (a space with its pool and file systems, or a
/// file system), with the environment and the action taken.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// What the report is about, as a person reads it: `space "Data"
    /// (guid) of pool "Storage pool" (guid)`.
    pub subject: String,
    /// The pool's name and GUID.
    pub pool: Option<(String, String)>,
    /// The space's name and GUID.
    pub space: Option<(String, String)>,
    pub checks: Vec<Check>,
    /// Versions and the like: (name, value).
    pub environment: Vec<(String, String)>,
    /// What was done: "refused", "attached", "attached read-only (forced)".
    pub action: Option<String>,
    /// What to do, beyond the advice of the checks (the command that reads
    /// anyway, read-only).
    pub notes: Vec<String>,
}

impl Report {
    pub fn new(subject: impl Into<String>) -> Report {
        Report {
            subject: subject.into(),
            ..Default::default()
        }
    }

    /// The worst status of the checks, as a verdict.
    pub fn verdict(&self) -> Verdict {
        self.checks
            .iter()
            .map(|c| c.status.verdict())
            .max()
            .unwrap_or(Verdict::Healthy)
    }

    /// The checks that make the report not healthy, the worst first (in
    /// their order otherwise).
    pub fn problems(&self) -> Vec<&Check> {
        let mut p: Vec<&Check> = self.checks.iter().filter(|c| c.status.is_problem()).collect();
        p.sort_by_key(|c| std::cmp::Reverse(c.status));
        p
    }

    /// One line for a log or a refusal: the verdict and the worst check.
    pub fn headline(&self) -> String {
        match self.problems().first() {
            None => format!("{}: {}", self.subject, self.verdict().as_str().to_uppercase()),
            Some(c) => format!(
                "{}: {} ({}: {})",
                self.subject,
                self.verdict().as_str().to_uppercase(),
                c.id,
                c.summary
            ),
        }
    }

    /// The report for people.
    pub fn to_text(&self) -> String {
        let mut out = format!("{}: {}", self.subject, self.verdict().as_str().to_uppercase());
        if let Some(action) = &self.action {
            write!(out, ", {action}").unwrap();
        }
        out.push('\n');
        let width = self.checks.iter().map(|c| c.id.len()).max().unwrap_or(0).max(16);
        for c in &self.checks {
            let status = if c.status.is_problem() {
                c.status.as_str().to_uppercase()
            } else {
                c.status.as_str().to_owned()
            };
            writeln!(out, "  {status:<8} {:<width$}  {}", c.id, c.summary).unwrap();
            let pad = " ".repeat(11);
            for e in &c.evidence {
                writeln!(out, "{pad}{:<9} {}", "what", e.what).unwrap();
                for (label, value) in [("where", &e.location), ("expected", &e.expected), ("found", &e.found)] {
                    if let Some(v) = value {
                        writeln!(out, "{pad}{label:<9} {v}").unwrap();
                    }
                }
            }
            if let Some(t) = c.truth.filter(|_| !c.evidence.is_empty() || c.status.is_problem()) {
                writeln!(out, "{pad}{:<9} {}", "truth", t.describe()).unwrap();
            }
        }
        let advice: Vec<String> = self
            .problems()
            .iter()
            .filter_map(|c| c.advice.as_ref().map(|a| format!("{}: {a}", c.id)))
            .chain(
                self.checks
                    .iter()
                    .filter(|c| !c.status.is_problem() && c.status != Status::Ok)
                    .filter_map(|c| c.advice.as_ref().map(|a| format!("{}: {a}", c.id))),
            )
            .chain(self.notes.iter().cloned())
            .collect();
        if !advice.is_empty() {
            out.push_str("  what to do:\n");
            let mut seen = std::collections::HashSet::new();
            for a in advice.iter().filter(|a| seen.insert(a.as_str())) {
                out.push_str(&wrap(a, "    - ", "      ", 78));
            }
        }
        if !self.environment.is_empty() {
            let env: Vec<String> = self.environment.iter().map(|(k, v)| format!("{k} {v}")).collect();
            out.push_str(&wrap(&env.join(", "), "  environment: ", "    ", 78));
        }
        out
    }

    /// The report for programs (`spaces check --json`).
    pub fn to_json(&self) -> String {
        let mut s = String::new();
        self.json().write(&mut s, 0);
        s.push('\n');
        s
    }

    fn json(&self) -> Json {
        let named = |v: &Option<(String, String)>| match v {
            Some((name, guid)) => Json::Obj(vec![
                ("name".into(), Json::Str(name.clone())),
                ("guid".into(), Json::Str(guid.clone())),
            ]),
            None => Json::Null,
        };
        let opt = |v: &Option<String>| v.clone().map_or(Json::Null, Json::Str);
        let checks = self
            .checks
            .iter()
            .map(|c| {
                Json::Obj(vec![
                    ("id".into(), Json::Str(c.id.clone())),
                    ("status".into(), Json::Str(c.status.as_str().into())),
                    ("summary".into(), Json::Str(c.summary.clone())),
                    (
                        "truth".into(),
                        c.truth.map_or(Json::Null, |t| Json::Str(t.as_str().into())),
                    ),
                    (
                        "evidence".into(),
                        Json::Arr(
                            c.evidence
                                .iter()
                                .map(|e| {
                                    Json::Obj(vec![
                                        ("what".into(), Json::Str(e.what.clone())),
                                        ("where".into(), opt(&e.location)),
                                        ("expected".into(), opt(&e.expected)),
                                        ("found".into(), opt(&e.found)),
                                    ])
                                })
                                .collect(),
                        ),
                    ),
                    ("advice".into(), opt(&c.advice)),
                ])
            })
            .collect();
        Json::Obj(vec![
            ("subject".into(), Json::Str(self.subject.clone())),
            ("pool".into(), named(&self.pool)),
            ("space".into(), named(&self.space)),
            ("verdict".into(), Json::Str(self.verdict().as_str().into())),
            ("action".into(), opt(&self.action)),
            ("checks".into(), Json::Arr(checks)),
            (
                "notes".into(),
                Json::Arr(self.notes.iter().cloned().map(Json::Str).collect()),
            ),
            (
                "environment".into(),
                Json::Obj(
                    self.environment
                        .iter()
                        .map(|(k, v)| (k.clone(), Json::Str(v.clone())))
                        .collect(),
                ),
            ),
        ])
    }
}

/// Several reports as one JSON array.
pub fn reports_to_json(reports: &[Report]) -> String {
    let mut s = String::new();
    Json::Arr(reports.iter().map(Report::json).collect()).write(&mut s, 0);
    s.push('\n');
    s
}

/// Bytes in hex for evidence: at most `max` of them, then "...", and
/// "(all zero)" when every byte is zero.
pub fn hex(bytes: &[u8], max: usize) -> String {
    let mut s: Vec<String> = bytes.iter().take(max).map(|b| format!("{b:02x}")).collect();
    if bytes.len() > max {
        s.push("...".into());
    }
    let mut out = s.join(" ");
    if !bytes.is_empty() && bytes.iter().all(|&b| b == 0) {
        out.push_str(" (all zero)");
    }
    out
}

/// A signature as text when it is printable ASCII, in hex otherwise.
pub fn signature(bytes: &[u8]) -> String {
    if !bytes.is_empty() && bytes.iter().all(|&b| (0x20..0x7f).contains(&b)) {
        format!("{:?}", String::from_utf8_lossy(bytes))
    } else {
        hex(bytes, 16)
    }
}

/// `text` wrapped at `width` columns, the first line after `first`, the
/// others after `rest`.
fn wrap(text: &str, first: &str, rest: &str, width: usize) -> String {
    let mut out = String::new();
    let mut line = first.to_owned();
    let mut empty = true;
    for word in text.split_whitespace() {
        if !empty && line.len() + 1 + word.len() > width {
            out.push_str(line.trim_end());
            out.push('\n');
            line = rest.to_owned();
            empty = true;
        }
        if !empty {
            line.push(' ');
        }
        line.push_str(word);
        empty = false;
    }
    out.push_str(line.trim_end());
    out.push('\n');
    out
}

/// Just enough JSON to write reports (no dependency for it).
enum Json {
    Null,
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn write(&self, out: &mut String, indent: usize) {
        let pad = |n: usize| "  ".repeat(n);
        match self {
            Json::Null => out.push_str("null"),
            Json::Str(s) => quote(s, out),
            Json::Arr(items) if items.is_empty() => out.push_str("[]"),
            Json::Arr(items) => {
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    out.push_str(&pad(indent + 1));
                    item.write(out, indent + 1);
                    out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
                }
                out.push_str(&pad(indent));
                out.push(']');
            }
            Json::Obj(fields) if fields.is_empty() => out.push_str("{}"),
            Json::Obj(fields) => {
                out.push_str("{\n");
                for (i, (k, v)) in fields.iter().enumerate() {
                    out.push_str(&pad(indent + 1));
                    quote(k, out);
                    out.push_str(": ");
                    v.write(out, indent + 1);
                    out.push_str(if i + 1 < fields.len() { ",\n" } else { "\n" });
                }
                out.push_str(&pad(indent));
                out.push('}');
            }
        }
    }
}

fn quote(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32).unwrap(),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Report {
        let mut r = Report::new("space \"Data\" (3f8c1d2e) of pool \"Storage pool\" (6a2b9e41)");
        r.pool = Some(("Storage pool".into(), "6a2b9e41".into()));
        r.space = Some(("Data".into(), "3f8c1d2e".into()));
        r.checks
            .push(Check::ok("pool.members", "1 of 1 disks (/dev/vdb2)").truth(Truth::Windows));
        r.checks.push(Check::new(
            "space.cache",
            Status::Info,
            "38 chunks cached (3 partly), copies agree",
        ));
        r.checks.push(
            Check::new(
                "space.partitions",
                Status::Suspect,
                "the backup GPT header is not valid",
            )
            .evidence(
                Evidence::new("backup GPT header")
                    .at("LBA 536870911 (space byte 0x1ffffffff000)")
                    .expected("\"EFI PART\"")
                    .found(hex(&[0; 512], 8)),
            )
            .truth(Truth::CrossCheck)
            .advice("the primary GPT is valid; report it with the bundle"),
        );
        r.checks.push(Check::skipped("space.journal", "not a parity space"));
        r.action = Some("refused".into());
        r.notes
            .push("To read it anyway, read-only: sudo spaces attach --force".into());
        r.environment.push(("spaces".into(), "1.2.0".into()));
        r
    }

    #[test]
    fn the_verdict_is_the_worst_status() {
        let mut r = sample();
        assert_eq!(r.verdict(), Verdict::Suspect);
        r.checks
            .push(Check::new("space.state", Status::Degraded, "Warning / Degraded"));
        assert_eq!(r.verdict(), Verdict::Suspect);
        assert_eq!(r.problems()[0].id, "space.partitions");
        r.checks
            .push(Check::new("space.layout", Status::Failed, "overlapping extents"));
        assert_eq!(r.verdict(), Verdict::Failed);
        // Information, warnings and skipped checks leave it healthy.
        let mut fine = Report::new("x");
        for s in [Status::Ok, Status::Info, Status::Warning, Status::Skipped] {
            fine.checks.push(Check::new("c", s, "s"));
        }
        assert_eq!(fine.verdict(), Verdict::Healthy);
        assert!(fine.problems().is_empty());
    }

    #[test]
    fn flags_allow_reading_by_verdict() {
        assert!(Verdict::Healthy.allowed(false, false));
        assert!(!Verdict::Degraded.allowed(false, false));
        assert!(Verdict::Degraded.allowed(true, false));
        assert!(Verdict::Degraded.allowed(false, true));
        assert!(!Verdict::Suspect.allowed(true, false));
        assert!(Verdict::Suspect.allowed(false, true));
        assert!(!Verdict::Failed.allowed(true, true));
        assert_eq!(Verdict::Degraded.flag(), Some("--degraded"));
        assert_eq!(Verdict::Suspect.flag(), Some("--force"));
        assert_eq!(Verdict::Failed.flag(), None);
        for v in [Verdict::Healthy, Verdict::Degraded, Verdict::Suspect, Verdict::Failed] {
            assert_eq!(Verdict::parse(v.as_str()), Some(v));
        }
    }

    #[test]
    fn text_shows_every_check_with_its_evidence() {
        let text = sample().to_text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "space \"Data\" (3f8c1d2e) of pool \"Storage pool\" (6a2b9e41): SUSPECT, refused"
        );
        assert!(lines[1].starts_with("  ok       pool.members      "), "{text}");
        assert!(
            lines[3].starts_with("  SUSPECT  space.partitions  the backup GPT"),
            "{text}"
        );
        assert!(text.contains("           where     LBA 536870911 (space byte 0x1ffffffff000)\n"));
        assert!(text.contains("           expected  \"EFI PART\"\n"));
        assert!(text.contains("           found     00 00 00 00 00 00 00 00 ... (all zero)\n"));
        assert!(text.contains("           truth     our cross-check"));
        assert!(text.contains("  skipped  space.journal     not a parity space\n"));
        assert!(text.contains("    - space.partitions: the primary GPT is valid"));
        assert!(text.contains("    - To read it anyway, read-only: sudo spaces attach --force\n"));
        assert!(text.ends_with("  environment: spaces 1.2.0\n"));
        // An ok check without evidence names no source.
        assert!(!lines[1].contains("truth"));
        assert_eq!(text.matches("truth").count(), 1);
    }

    #[test]
    fn headline_names_the_worst_check() {
        assert_eq!(
            sample().headline(),
            "space \"Data\" (3f8c1d2e) of pool \"Storage pool\" (6a2b9e41): SUSPECT \
             (space.partitions: the backup GPT header is not valid)"
        );
    }

    #[test]
    fn json_has_stable_ids_and_statuses() {
        let json = sample().to_json();
        assert!(json.contains("\"verdict\": \"suspect\""), "{json}");
        assert!(json.contains("\"id\": \"space.partitions\""));
        assert!(json.contains("\"status\": \"skipped\""));
        assert!(json.contains("\"truth\": \"cross-check\""));
        assert!(json.contains("\"where\": \"LBA 536870911 (space byte 0x1ffffffff000)\""));
        assert!(json.contains("\"expected\": \"\\\"EFI PART\\\"\""));
        assert!(json.contains("\"space\": {\n    \"name\": \"Data\""));
        // Balanced and parseable: quotes escaped, no trailing commas.
        assert_eq!(json.matches('{').count(), json.matches('}').count());
        assert!(!json.contains(",\n}") && !json.contains(",\n]"));
        let mut s = String::new();
        quote("a\"b\\c\nd\u{1}", &mut s);
        assert_eq!(s, "\"a\\\"b\\\\c\\nd\\u0001\"");
        assert!(reports_to_json(&[sample(), sample()]).starts_with("[\n  {\n"));
    }

    #[test]
    fn hex_shows_zeros_and_signatures() {
        assert_eq!(hex(&[0, 0], 8), "00 00 (all zero)");
        assert_eq!(hex(&[1, 2, 3], 2), "01 02 ...");
        assert_eq!(signature(b"EFI PART"), "\"EFI PART\"");
        assert_eq!(signature(&[0x45, 0, 1]), "45 00 01");
    }

    #[test]
    fn wraps_long_advice() {
        let w = wrap(&"word ".repeat(30), "    - ", "      ", 40);
        assert!(w.lines().all(|l| l.len() <= 40), "{w}");
        assert!(w.lines().skip(1).all(|l| l.starts_with("      word")));
    }
}
