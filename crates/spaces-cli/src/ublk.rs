//! Exposing a space as a ublk block device (`/dev/ublkbN`): read-only, or
//! writable through a space writer (`--rw`).
//!
//! Each hardware queue runs in its own thread with a synchronous handler
//! that reads the requested range through the space reader (or writer)
//! straight into the queue's I/O buffer, or writes it from there. A
//! writable device declares a volatile cache, so the kernel sends flushes
//! (and turns FUA writes into a write and a flush); what is written is
//! flushed once more when the device goes away. The device lives until the
//! process receives SIGINT/SIGTERM or the device is deleted.

use std::fs::File;

use anyhow::{Context, Result};
use libublk::io::{BufDescList, UblkDev, UblkIOCtx, UblkQueue};
use libublk::{BufDesc, UblkFlags, UblkIORes};
use storage_spaces::io::ReadAt;
use storage_spaces::{Pool, SpaceWriter};

const QUEUES: u16 = 2;
const DEPTH: u16 = 64;
const IO_BUF_BYTES: u32 = 1 << 20;

/// Serves `source` (the space reader, or the writer of a writable device)
/// until the device is removed. `on_ready` receives the block device path
/// once the kernel exposes it.
pub fn serve(
    pool: &'static Pool<File>,
    source: &'static dyn ReadAt,
    writer: Option<&'static SpaceWriter<'static, File>>,
    on_ready: impl FnOnce(&str) + Send + Sync + 'static,
) -> Result<()> {
    let ctrl = libublk::ctrl::UblkCtrlBuilder::default()
        .name("storage-spaces")
        .nr_queues(QUEUES)
        .depth(DEPTH)
        .io_buf_bytes(IO_BUF_BYTES)
        .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
        .build()
        .context("cannot create the ublk device (is the ublk_drv module loaded?)")?;
    let dev_id = ctrl.dev_info().dev_id;

    let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM])?;
    std::thread::spawn(move || {
        if signals.forever().next().is_some()
            && let Ok(ctrl) = libublk::ctrl::UblkCtrl::new_simple(dev_id as i32)
        {
            let _ = ctrl.kill_dev();
        }
    });

    let size = source.size()?;
    let logical_shift = pool.logical_sector_size.trailing_zeros() as u8;
    let physical_shift = pool.physical_sector_size.max(pool.logical_sector_size).trailing_zeros() as u8;
    let tgt_init = move |dev: &mut UblkDev| {
        dev.set_default_params(size);
        let basic = &mut dev.tgt.params.basic;
        basic.attrs = if writer.is_some() {
            libublk::sys::UBLK_ATTR_VOLATILE_CACHE
        } else {
            libublk::sys::UBLK_ATTR_READ_ONLY
        };
        basic.logical_bs_shift = logical_shift;
        basic.physical_bs_shift = physical_shift;
        basic.io_min_shift = physical_shift;
        basic.io_opt_shift = physical_shift;
        if writer.is_some_and(|w| w.discards()) {
            // Discards give whole rows of thin spaces back to the pool.
            dev.tgt.params.types |= libublk::sys::UBLK_PARAM_TYPE_DISCARD;
            dev.tgt.params.discard = libublk::sys::ublk_param_discard {
                discard_granularity: 1 << physical_shift,
                max_discard_sectors: 1 << 22,
                max_discard_segments: 1,
                ..Default::default()
            };
        }
        Ok(())
    };
    let queue_fn = move |qid: u16, dev: &UblkDev| run_queue(qid, dev, source, writer);
    let ready = move |ctrl: &libublk::ctrl::UblkCtrl| on_ready(&format!("/dev/ublkb{}", ctrl.dev_info().dev_id));
    ctrl.run_target(tgt_init, queue_fn, ready)
        .context("ublk device failed")?;
    let _ = ctrl.del_dev();
    if let Some(w) = writer {
        w.flush().context("cannot flush the space")?;
    }
    Ok(())
}

fn run_queue(qid: u16, dev: &UblkDev, source: &dyn ReadAt, writer: Option<&SpaceWriter<'_, File>>) {
    let mut bufs = dev.alloc_queue_io_bufs();
    let queue = match UblkQueue::new(qid, dev)
        .and_then(|q| q.submit_fetch_commands_unified(BufDescList::Slices(Some(&bufs))))
    {
        Ok(q) => q,
        Err(e) => {
            eprintln!("ublk queue {qid} setup failed: {e}");
            return;
        }
    };
    queue.wait_and_handle_io(move |q: &UblkQueue, tag: u16, _: &UblkIOCtx| {
        let iod = q.get_iod(tag);
        let offset = iod.start_sector << 9;
        let len = (iod.nr_sectors as usize) << 9;
        let buf = bufs[tag as usize].as_mut_slice();
        let result = match iod.op_flags & 0xff {
            libublk::sys::UBLK_IO_OP_READ if len <= buf.len() => match source.read_exact_at(&mut buf[..len], offset) {
                Ok(()) => len as i32,
                Err(e) => {
                    eprintln!("read of {len} bytes at {offset:#x} failed: {e}");
                    -libc_errno::EIO
                }
            },
            libublk::sys::UBLK_IO_OP_READ => -libc_errno::EINVAL,
            libublk::sys::UBLK_IO_OP_WRITE => match writer {
                Some(_) if len > buf.len() => -libc_errno::EINVAL,
                Some(w) => match w.write_all_at(&buf[..len], offset) {
                    Ok(()) => len as i32,
                    Err(e) => {
                        eprintln!("write of {len} bytes at {offset:#x} failed: {e}");
                        -libc_errno::EIO
                    }
                },
                None => -libc_errno::EROFS,
            },
            libublk::sys::UBLK_IO_OP_DISCARD => match writer.map(|w| w.discard(offset, len as u64)) {
                Some(Ok(())) => 0,
                Some(Err(e)) => {
                    eprintln!("discard of {len} bytes at {offset:#x} failed: {e}");
                    -libc_errno::EIO
                }
                None => -libc_errno::EROFS,
            },
            libublk::sys::UBLK_IO_OP_FLUSH => match writer.map_or(Ok(()), |w| w.flush()) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("flush failed: {e}");
                    -libc_errno::EIO
                }
            },
            _ if writer.is_some() => -libc_errno::EOPNOTSUPP,
            _ => -libc_errno::EROFS,
        };
        let desc = BufDesc::Slice(bufs[tag as usize].as_slice());
        if let Err(e) = q.complete_io_cmd_unified(tag, desc, Ok(UblkIORes::Result(result))) {
            eprintln!("ublk completion failed: {e}");
        }
    });
}

/// The few errno values the handler returns (Linux numbering).
mod libc_errno {
    pub const EIO: i32 = 5;
    pub const EINVAL: i32 = 22;
    pub const EROFS: i32 = 30;
    pub const EOPNOTSUPP: i32 = 95;
}
