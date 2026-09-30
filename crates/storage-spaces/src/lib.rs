//! Read access to Microsoft Storage Spaces pools.
//!
//! ```no_run
//! use std::fs::File;
//! use storage_spaces::Pool;
//!
//! let disks = vec![File::open("/dev/sdb")?, File::open("/dev/sdc")?];
//! let pool = Pool::open(disks)?;
//! for space in pool.user_spaces() {
//!     let reader = pool.open_space(space.id())?;
//!     let mut sector = [0u8; 512];
//!     reader.read_exact_at(&mut sector, 0)?;
//! }
//! # Ok::<(), storage_spaces::Error>(())
//! ```

pub mod cache;
mod crc;
pub mod create;
pub mod database;
pub mod drt;
mod error;
pub mod format;
pub mod gf16;
pub mod gpt;
mod guid;
pub mod io;
pub mod journal;
pub mod layout;
pub mod manage;
pub mod ops;
pub mod plan;
mod pool;
mod reader;
pub mod records;
pub mod segments;
#[doc(hidden)]
pub mod testpattern;
mod writer;

pub use error::{Error, Result};
pub use guid::Guid;
pub use layout::Condition;
pub use pool::{Member, PhysicalDisk, Pool, Space};
pub use reader::{OpenOptions, SpaceReader, SpaceStream, UncleanParity};
pub use writer::SpaceWriter;
