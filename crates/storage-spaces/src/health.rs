//! The health of a pool in Windows' terms (`HealthStatus` and
//! `OperationalStatus` of `Get-StoragePool`, `Get-VirtualDisk` and
//! `Get-PhysicalDisk`), predicted from the pool's metadata and the disks at
//! hand. The meanings follow Microsoft's documentation of the states; the
//! rules and the states Windows showed for the pools on record are in
//! docs/storage-spaces-format.md ("Health").
//!
//! States that live only in a running Windows (a repair in service, a space
//! detached by policy or set to manual attach, a pool set read-only by an
//! administrator) are not in the metadata and never predicted.

use std::fmt;

use crate::Pool;
use crate::error::Result;
use crate::format::DiskUsage;
use crate::io::ReadAt;
use crate::layout::{Layout, Redundancy};

/// `HealthStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HealthStatus {
    Healthy,
    Warning,
    Unhealthy,
}

impl fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            HealthStatus::Healthy => "Healthy",
            HealthStatus::Warning => "Warning",
            HealthStatus::Unhealthy => "Unhealthy",
        })
    }
}

/// A health status with its operational status (one or more of Windows'
/// words, in the order `Get-VirtualDisk` lists them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub health: HealthStatus,
    pub operational: Vec<&'static str>,
}

impl State {
    fn new(health: HealthStatus, operational: &[&'static str]) -> State {
        State {
            health,
            operational: operational.to_vec(),
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} / {}", self.health, self.operational.join(" "))
    }
}

#[derive(Debug, Clone)]
pub struct DiskHealth {
    pub id: u64,
    pub state: State,
}

#[derive(Debug, Clone)]
pub struct SpaceHealth {
    pub id: u64,
    pub name: String,
    pub state: State,
    /// Disk failures the space still survives (`None`: data is lost).
    pub failures_left: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Health {
    pub pool: State,
    /// Copies of the pool database on disks at hand, and in all.
    pub database_copies: (usize, usize),
    pub disks: Vec<DiskHealth>,
    pub spaces: Vec<SpaceHealth>,
}

/// The health Windows would show for `pool` with the disks at hand.
///
/// * A disk at hand is Healthy / OK, a missing one Warning / Lost
///   Communication (its usage, Retired for instance, is a separate
///   property).
/// * The pool is Healthy / OK with every disk; with disks missing, Warning /
///   Degraded while more than half of the pool database copies are at
///   hand, otherwise Unhealthy / Read-only (the pool lost its quorum).
/// * A space is Healthy / OK with every disk of the pool at hand and every
///   copy current, its hidden spaces (tiers, cache, dirty region log, parity
///   journal) included. With a disk of the pool missing or out-of-date
///   copies it is Warning / Degraded, also Incomplete with copies of its own
///   on missing disks; Unhealthy / No Redundancy Degraded when data is lost;
///   Unhealthy / Detached when the pool lost its quorum. These are the
///   states once the pool has started and its spaces are connected (a pool
///   arriving without a disk first shows them detached).
pub fn health<D: ReadAt>(pool: &Pool<D>) -> Result<Health> {
    let disks: Vec<DiskHealth> = pool
        .disks
        .values()
        .map(|d| DiskHealth {
            id: d.id,
            state: if d.member.is_some() {
                State::new(HealthStatus::Healthy, &["OK"])
            } else {
                State::new(HealthStatus::Warning, &["Lost Communication"])
            },
        })
        .collect();
    let copies = pool.disks.values().filter(|d| d.database_copy).count();
    let at_hand = pool
        .disks
        .values()
        .filter(|d| d.database_copy && d.member.is_some())
        .count();
    let missing = disks.iter().any(|d| d.state.health != HealthStatus::Healthy);
    let pool_state = if !missing {
        State::new(HealthStatus::Healthy, &["OK"])
    } else if at_hand * 2 > copies {
        State::new(HealthStatus::Warning, &["Degraded"])
    } else {
        State::new(HealthStatus::Unhealthy, &["Read-only"])
    };
    let present = |disk: u64| pool.disks.get(&disk).is_some_and(|d| d.member.is_some());
    let evacuating = |disk: u64| {
        pool.disks
            .get(&disk)
            .is_some_and(|d| d.member.is_some() && d.usage == DiskUsage::Retired)
    };
    let mut spaces = Vec::new();
    for space in pool.user_spaces() {
        // The space and every hidden space under it (tiers, write-back
        // cache, dirty region log, parity journal).
        let family = pool.family(space.id());
        let mut r = Redundancy {
            left: Some(u64::MAX),
            missing: false,
            stale: false,
        };
        for s in family.iter().filter(|s| !s.extents.is_empty()) {
            if let Some(policy) = &s.info.policy {
                r = r.and(Layout::new(policy, &s.extents)?.redundancy(present, evacuating));
            }
        }
        let mut words = Vec::new();
        let health = match r.left {
            // Without its quorum the pool detaches every space.
            _ if pool_state.health == HealthStatus::Unhealthy => {
                words.push("Detached");
                HealthStatus::Unhealthy
            }
            // Data lost: the space stays attached, failing what it lost.
            None => {
                words.extend(["No Redundancy", "Degraded"]);
                HealthStatus::Unhealthy
            }
            _ if !missing && !r.stale => {
                words.push("OK");
                HealthStatus::Healthy
            }
            // A disk of the pool missing degrades every space; those with
            // copies on it are also incomplete.
            _ => {
                words.push("Degraded");
                if r.missing {
                    words.push("Incomplete");
                }
                HealthStatus::Warning
            }
        };
        spaces.push(SpaceHealth {
            id: space.id(),
            name: space.name().to_owned(),
            state: State {
                health,
                operational: words,
            },
            failures_left: r.left,
        });
    }
    Ok(Health {
        pool: pool_state,
        database_copies: (at_hand, copies),
        disks,
        spaces,
    })
}
