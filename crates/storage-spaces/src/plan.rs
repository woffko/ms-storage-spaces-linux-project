//! Management operations as plans: ordered steps of writes, each step made
//! durable (every device it touched flushed) before the next begins, so
//! that a crash leaves the pool as it was before some step. A plan is
//! computed first and can be shown without writing anything (dry run).

use std::collections::BTreeSet;
use std::fmt;

use crate::error::Result;
use crate::io::WriteAt;

/// A device of a plan: a member of the pool (index into the devices the
/// pool was opened with) or a disk new to it (index into the new disks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    Member(usize),
    New(usize),
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Member(i) => write!(f, "device {i}"),
            Target::New(i) => write!(f, "new disk {i}"),
        }
    }
}

/// One thing a step does.
#[derive(Debug, Clone)]
pub enum Action {
    Write {
        target: Target,
        offset: u64,
        bytes: Vec<u8>,
    },
    /// Copies `len` bytes (a slab being moved), read when applied.
    Copy {
        from: Target,
        from_offset: u64,
        to: Target,
        to_offset: u64,
        len: u64,
    },
}

/// Actions made durable together.
#[derive(Debug, Clone)]
pub struct Step {
    pub what: String,
    pub actions: Vec<Action>,
}

/// A management operation: what it changes, and the steps that change it.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub summary: Vec<String>,
    pub steps: Vec<Step>,
}

impl Plan {
    pub fn step(&mut self, what: impl Into<String>, actions: Vec<Action>) {
        if !actions.is_empty() {
            self.steps.push(Step {
                what: what.into(),
                actions,
            });
        }
    }

    /// Bytes the plan writes (copies included).
    pub fn bytes(&self) -> u64 {
        self.steps
            .iter()
            .flat_map(|s| &s.actions)
            .map(|a| match a {
                Action::Write { bytes, .. } => bytes.len() as u64,
                Action::Copy { len, .. } => *len,
            })
            .sum()
    }

    /// Carries the plan out on the pool's devices (`members`) and the new
    /// disks (`new`), flushing every device a step touched before the next.
    pub fn apply<M: WriteAt, N: WriteAt>(&self, members: &[M], new: &[N]) -> Result<()> {
        let device = |t: Target| -> Result<&dyn WriteAt> {
            match t {
                Target::Member(i) => members.get(i).map(|d| d as &dyn WriteAt),
                Target::New(i) => new.get(i).map(|d| d as &dyn WriteAt),
            }
            .ok_or_else(|| crate::Error::Pool(format!("the plan writes to {t}, which is not at hand")))
        };
        for step in &self.steps {
            let mut touched = BTreeSet::new();
            for action in &step.actions {
                match action {
                    Action::Write { target, offset, bytes } => {
                        device(*target)?.write_all_at(bytes, *offset)?;
                        touched.insert(*target);
                    }
                    Action::Copy {
                        from,
                        from_offset,
                        to,
                        to_offset,
                        len,
                    } => {
                        let mut buf = vec![0u8; (*len).min(4 << 20) as usize];
                        let mut done = 0;
                        while done < *len {
                            let n = buf.len().min((*len - done) as usize);
                            device(*from)?.read_exact_at(&mut buf[..n], from_offset + done)?;
                            device(*to)?.write_all_at(&buf[..n], to_offset + done)?;
                            done += n as u64;
                        }
                        touched.insert(*to);
                    }
                }
            }
            for t in touched {
                device(t)?.flush()?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for line in &self.summary {
            writeln!(f, "{line}")?;
        }
        for (i, step) in self.steps.iter().enumerate() {
            let bytes: u64 = step
                .actions
                .iter()
                .map(|a| match a {
                    Action::Write { bytes, .. } => bytes.len() as u64,
                    Action::Copy { len, .. } => *len,
                })
                .sum();
            let targets: BTreeSet<String> = step
                .actions
                .iter()
                .map(|a| match a {
                    Action::Write { target, .. } | Action::Copy { to: target, .. } => target.to_string(),
                })
                .collect();
            writeln!(
                f,
                "step {}: {} ({} bytes to {})",
                i + 1,
                step.what,
                bytes,
                targets.into_iter().collect::<Vec<_>>().join(", ")
            )?;
        }
        Ok(())
    }
}
