//! An NBD server (fixed newstyle handshake, simple replies).
//!
//! The kernel NBD driver is attached to it with `nbd-client`; this module
//! only speaks the protocol over a connected stream. An export without a
//! sink is read-only and refuses writes with EPERM; trims are always
//! refused.

use std::io::{self, Read, Write};

use storage_spaces::io::ReadAt;

const NBDMAGIC: u64 = 0x4e42_444d_4147_4943;
const IHAVEOPT: u64 = 0x4948_4156_454f_5054;
const OPT_REPLY_MAGIC: u64 = 0x0003_e889_0455_65a9;
const REQUEST_MAGIC: u32 = 0x2560_9513;
const SIMPLE_REPLY_MAGIC: u32 = 0x6744_6698;

const FLAG_FIXED_NEWSTYLE: u16 = 1;
const FLAG_NO_ZEROES: u16 = 2;
const CLIENT_FLAG_NO_ZEROES: u32 = 2;

const OPT_EXPORT_NAME: u32 = 1;
const OPT_ABORT: u32 = 2;
const OPT_LIST: u32 = 3;
const OPT_INFO: u32 = 6;
const OPT_GO: u32 = 7;

const REP_ACK: u32 = 1;
const REP_SERVER: u32 = 2;
const REP_INFO: u32 = 3;
const REP_ERR_UNSUP: u32 = 0x8000_0001;
const REP_ERR_INVALID: u32 = 0x8000_0003;
const REP_ERR_UNKNOWN: u32 = 0x8000_0006;

const INFO_EXPORT: u16 = 0;
const INFO_BLOCK_SIZE: u16 = 3;

const TFLAG_HAS_FLAGS: u16 = 1;
const TFLAG_READ_ONLY: u16 = 2;
const TFLAG_SEND_FLUSH: u16 = 4;
const TFLAG_SEND_FUA: u16 = 8;
const TFLAG_CAN_MULTI_CONN: u16 = 1 << 8;

const CMD_READ: u16 = 0;
const CMD_WRITE: u16 = 1;
const CMD_DISC: u16 = 2;
const CMD_FLUSH: u16 = 3;
const CMD_FLAG_FUA: u16 = 1;

const EPERM: u32 = 1;
const EIO: u32 = 5;
const EINVAL: u32 = 22;

/// Largest read request served (the kernel sends at most max_sectors).
const MAX_REQUEST: u32 = 32 << 20;

/// Where the writes to a writable export go.
pub trait Sink: Sync {
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    /// Makes the writes so far durable.
    fn flush(&self) -> io::Result<()>;
}

/// What is exported: read-only without a sink.
pub struct Export<'a, R: ?Sized> {
    pub name: &'a str,
    pub source: &'a R,
    pub sink: Option<&'a dyn Sink>,
    pub size: u64,
    pub block_size: u32,
}

/// Serves one client connection until it disconnects.
pub fn serve<R: ReadAt + ?Sized, S: Read + Write>(export: &Export<'_, R>, mut conn: S) -> io::Result<()> {
    conn.write_all(&NBDMAGIC.to_be_bytes())?;
    conn.write_all(&IHAVEOPT.to_be_bytes())?;
    conn.write_all(&(FLAG_FIXED_NEWSTYLE | FLAG_NO_ZEROES).to_be_bytes())?;
    let client_flags = read_u32(&mut conn)?;
    let no_zeroes = client_flags & CLIENT_FLAG_NO_ZEROES != 0;

    loop {
        if read_u64(&mut conn)? != IHAVEOPT {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad option magic"));
        }
        let option = read_u32(&mut conn)?;
        let len = read_u32(&mut conn)?;
        if len > 64 << 10 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "option too long"));
        }
        let mut data = vec![0u8; len as usize];
        conn.read_exact(&mut data)?;
        match option {
            OPT_EXPORT_NAME => {
                if data != export.name.as_bytes() && !data.is_empty() {
                    return Ok(()); // the protocol has no error reply here
                }
                conn.write_all(&export.size.to_be_bytes())?;
                conn.write_all(&transmission_flags(export.sink.is_some()).to_be_bytes())?;
                if !no_zeroes {
                    conn.write_all(&[0u8; 124])?;
                }
                break;
            }
            OPT_ABORT => {
                option_reply(&mut conn, option, REP_ACK, &[])?;
                return Ok(());
            }
            OPT_LIST => {
                let mut entry = (export.name.len() as u32).to_be_bytes().to_vec();
                entry.extend_from_slice(export.name.as_bytes());
                option_reply(&mut conn, option, REP_SERVER, &entry)?;
                option_reply(&mut conn, option, REP_ACK, &[])?;
            }
            OPT_INFO | OPT_GO => {
                let Some(name) = parse_info_request(&data) else {
                    option_reply(&mut conn, option, REP_ERR_INVALID, &[])?;
                    continue;
                };
                if !name.is_empty() && name != export.name.as_bytes() {
                    option_reply(&mut conn, option, REP_ERR_UNKNOWN, &[])?;
                    continue;
                }
                let mut info = INFO_EXPORT.to_be_bytes().to_vec();
                info.extend_from_slice(&export.size.to_be_bytes());
                info.extend_from_slice(&transmission_flags(export.sink.is_some()).to_be_bytes());
                option_reply(&mut conn, option, REP_INFO, &info)?;
                let mut bs = INFO_BLOCK_SIZE.to_be_bytes().to_vec();
                bs.extend_from_slice(&export.block_size.to_be_bytes());
                bs.extend_from_slice(&export.block_size.max(4096).to_be_bytes());
                bs.extend_from_slice(&MAX_REQUEST.to_be_bytes());
                option_reply(&mut conn, option, REP_INFO, &bs)?;
                option_reply(&mut conn, option, REP_ACK, &[])?;
                if option == OPT_GO {
                    break;
                }
            }
            _ => option_reply(&mut conn, option, REP_ERR_UNSUP, &[])?,
        }
    }
    transmission(export, conn)
}

fn transmission_flags(writable: bool) -> u16 {
    let access = if writable { TFLAG_SEND_FUA } else { TFLAG_READ_ONLY };
    TFLAG_HAS_FLAGS | access | TFLAG_SEND_FLUSH | TFLAG_CAN_MULTI_CONN
}

/// Returns the export name of an NBD_OPT_INFO/GO request.
fn parse_info_request(data: &[u8]) -> Option<&[u8]> {
    let name_len = u32::from_be_bytes(data.get(0..4)?.try_into().ok()?) as usize;
    let name = data.get(4..4 + name_len)?;
    let count = u16::from_be_bytes(data.get(4 + name_len..6 + name_len)?.try_into().ok()?) as usize;
    (data.len() == 6 + name_len + 2 * count).then_some(name)
}

fn option_reply<W: Write>(w: &mut W, option: u32, reply: u32, data: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(20 + data.len());
    buf.extend_from_slice(&OPT_REPLY_MAGIC.to_be_bytes());
    buf.extend_from_slice(&option.to_be_bytes());
    buf.extend_from_slice(&reply.to_be_bytes());
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    buf.extend_from_slice(data);
    w.write_all(&buf)
}

fn transmission<R: ReadAt + ?Sized, S: Read + Write>(export: &Export<'_, R>, mut conn: S) -> io::Result<()> {
    let mut data = Vec::new();
    loop {
        let mut req = [0u8; 28];
        match conn.read_exact(&mut req) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            other => other?,
        }
        if u32::from_be_bytes(req[0..4].try_into().unwrap()) != REQUEST_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad request magic"));
        }
        let flags = u16::from_be_bytes(req[4..6].try_into().unwrap());
        let kind = u16::from_be_bytes(req[6..8].try_into().unwrap());
        let handle = &req[8..16];
        let offset = u64::from_be_bytes(req[16..24].try_into().unwrap());
        let length = u32::from_be_bytes(req[24..28].try_into().unwrap());
        let reply = |conn: &mut S, error: u32, payload: &[u8]| -> io::Result<()> {
            let mut head = [0u8; 16];
            head[0..4].copy_from_slice(&SIMPLE_REPLY_MAGIC.to_be_bytes());
            head[4..8].copy_from_slice(&error.to_be_bytes());
            head[8..16].copy_from_slice(handle);
            conn.write_all(&head)?;
            conn.write_all(payload)
        };
        match kind {
            CMD_READ => {
                let in_range = offset.checked_add(length as u64).is_some_and(|end| end <= export.size);
                if !in_range || length > MAX_REQUEST {
                    reply(&mut conn, EINVAL, &[])?;
                    continue;
                }
                data.resize(length as usize, 0);
                match export.source.read_exact_at(&mut data, offset) {
                    Ok(()) => reply(&mut conn, 0, &data)?,
                    Err(e) => {
                        eprintln!("read of {length} bytes at {offset:#x} failed: {e}");
                        reply(&mut conn, EIO, &[])?;
                    }
                }
            }
            CMD_FLUSH => match export.sink.map_or(Ok(()), |s| s.flush()) {
                Ok(()) => reply(&mut conn, 0, &[])?,
                Err(e) => {
                    eprintln!("flush failed: {e}");
                    reply(&mut conn, EIO, &[])?;
                }
            },
            // What the client wrote becomes durable when it disconnects.
            CMD_DISC => return export.sink.map_or(Ok(()), |s| s.flush()),
            // Writes carry a payload that must be consumed before replying.
            CMD_WRITE => {
                let in_range = offset.checked_add(length as u64).is_some_and(|end| end <= export.size);
                let Some(sink) = export.sink.filter(|_| in_range && length <= MAX_REQUEST) else {
                    io::copy(&mut (&mut conn).take(length as u64), &mut io::sink())?;
                    let error = if export.sink.is_some() { EINVAL } else { EPERM };
                    reply(&mut conn, error, &[])?;
                    continue;
                };
                data.resize(length as usize, 0);
                conn.read_exact(&mut data)?;
                let written = sink.write_all_at(&data, offset).and_then(|()| {
                    if flags & CMD_FLAG_FUA != 0 {
                        sink.flush()
                    } else {
                        Ok(())
                    }
                });
                match written {
                    Ok(()) => reply(&mut conn, 0, &[])?,
                    Err(e) => {
                        eprintln!("write of {length} bytes at {offset:#x} failed: {e}");
                        reply(&mut conn, EIO, &[])?;
                    }
                }
            }
            _ => reply(&mut conn, EPERM, &[])?,
        }
    }
}

fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn read_u64<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use storage_spaces::io::MemDevice;

    fn client_go(c: &mut UnixStream, name: &str) -> (u64, u16, u32) {
        let mut hello = [0u8; 18];
        c.read_exact(&mut hello).unwrap();
        assert_eq!(u64::from_be_bytes(hello[0..8].try_into().unwrap()), NBDMAGIC);
        c.write_all(&CLIENT_FLAG_NO_ZEROES.to_be_bytes()).unwrap();
        let mut opt = IHAVEOPT.to_be_bytes().to_vec();
        opt.extend_from_slice(&OPT_GO.to_be_bytes());
        let mut data = (name.len() as u32).to_be_bytes().to_vec();
        data.extend_from_slice(name.as_bytes());
        data.extend_from_slice(&1u16.to_be_bytes());
        data.extend_from_slice(&INFO_BLOCK_SIZE.to_be_bytes());
        opt.extend_from_slice(&(data.len() as u32).to_be_bytes());
        opt.extend_from_slice(&data);
        c.write_all(&opt).unwrap();
        let (mut size, mut flags, mut block) = (0, 0, 0);
        loop {
            let mut head = [0u8; 20];
            c.read_exact(&mut head).unwrap();
            let reply = u32::from_be_bytes(head[12..16].try_into().unwrap());
            let mut payload = vec![0u8; u32::from_be_bytes(head[16..20].try_into().unwrap()) as usize];
            c.read_exact(&mut payload).unwrap();
            match reply {
                REP_ACK => return (size, flags, block),
                REP_INFO if payload[0..2] == INFO_EXPORT.to_be_bytes() => {
                    size = u64::from_be_bytes(payload[2..10].try_into().unwrap());
                    flags = u16::from_be_bytes(payload[10..12].try_into().unwrap());
                }
                REP_INFO => block = u32::from_be_bytes(payload[2..6].try_into().unwrap()),
                other => panic!("unexpected reply {other:#x}"),
            }
        }
    }

    fn request(c: &mut UnixStream, kind: u16, offset: u64, len: u32) -> (u32, Vec<u8>) {
        request_with(
            c,
            kind,
            0,
            offset,
            &vec![0u8; if kind == CMD_WRITE { len as usize } else { 0 }],
            len,
        )
    }

    fn request_with(
        c: &mut UnixStream,
        kind: u16,
        flags: u16,
        offset: u64,
        payload: &[u8],
        len: u32,
    ) -> (u32, Vec<u8>) {
        let mut req = REQUEST_MAGIC.to_be_bytes().to_vec();
        req.extend_from_slice(&flags.to_be_bytes());
        req.extend_from_slice(&kind.to_be_bytes());
        req.extend_from_slice(&42u64.to_be_bytes());
        req.extend_from_slice(&offset.to_be_bytes());
        req.extend_from_slice(&len.to_be_bytes());
        c.write_all(&req).unwrap();
        c.write_all(payload).unwrap();
        let mut head = [0u8; 16];
        c.read_exact(&mut head).unwrap();
        assert_eq!(u64::from_be_bytes(head[8..16].try_into().unwrap()), 42);
        let error = u32::from_be_bytes(head[4..8].try_into().unwrap());
        let mut data = vec![
            0u8;
            if kind == CMD_READ && error == 0 {
                len as usize
            } else {
                0
            }
        ];
        c.read_exact(&mut data).unwrap();
        (error, data)
    }

    #[test]
    fn serves_reads_and_refuses_writes() {
        let dev = MemDevice((0..8192u32).map(|i| i as u8).collect());
        let (mut client, server) = UnixStream::pair().unwrap();
        let handle = std::thread::spawn(move || {
            let export = Export {
                name: "space",
                source: &dev,
                sink: None,
                size: 8192,
                block_size: 4096,
            };
            serve(&export, server).unwrap();
        });
        let (size, flags, block) = client_go(&mut client, "space");
        assert_eq!((size, block), (8192, 4096));
        assert_ne!(flags & TFLAG_READ_ONLY, 0);
        let (error, data) = request(&mut client, CMD_READ, 4096, 16);
        assert_eq!(error, 0);
        assert_eq!(data, (4096..4112u32).map(|i| i as u8).collect::<Vec<_>>());
        assert_eq!(request(&mut client, CMD_READ, 8190, 4).0, EINVAL);
        assert_eq!(request(&mut client, 1, 0, 512).0, EPERM);
        assert_eq!(request(&mut client, CMD_FLUSH, 0, 0).0, 0);
        let mut disc = REQUEST_MAGIC.to_be_bytes().to_vec();
        disc.extend_from_slice(&[0, 0, 0, 2]);
        disc.extend_from_slice(&[0u8; 20]);
        client.write_all(&disc).unwrap();
        handle.join().unwrap();
    }

    /// A writable in-memory export.
    struct Mem(std::sync::Mutex<Vec<u8>>, std::sync::atomic::AtomicUsize);

    impl ReadAt for Mem {
        fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
            let d = self.0.lock().unwrap();
            buf.copy_from_slice(&d[offset as usize..offset as usize + buf.len()]);
            Ok(())
        }
        fn size(&self) -> io::Result<u64> {
            Ok(self.0.lock().unwrap().len() as u64)
        }
    }

    impl Sink for Mem {
        fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
            self.0.lock().unwrap()[offset as usize..offset as usize + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn flush(&self) -> io::Result<()> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn a_writable_export_takes_writes_flushes_and_fua() {
        let mem = std::sync::Arc::new(Mem(std::sync::Mutex::new(vec![0; 8192]), Default::default()));
        let dev = mem.clone();
        let (mut client, server) = UnixStream::pair().unwrap();
        let handle = std::thread::spawn(move || {
            let export = Export {
                name: "space",
                source: &*dev,
                sink: Some(&*dev),
                size: 8192,
                block_size: 4096,
            };
            serve(&export, server).unwrap();
        });
        let (_, flags, _) = client_go(&mut client, "space");
        assert_eq!(flags & TFLAG_READ_ONLY, 0);
        assert_ne!(flags & TFLAG_SEND_FUA, 0);
        assert_eq!(request_with(&mut client, CMD_WRITE, 0, 4096, &[7; 16], 16).0, 0);
        assert_eq!(
            request(&mut client, CMD_READ, 4090, 12).1,
            [0, 0, 0, 0, 0, 0, 7, 7, 7, 7, 7, 7]
        );
        assert_eq!(mem.1.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(request_with(&mut client, CMD_WRITE, CMD_FLAG_FUA, 0, &[1; 4], 4).0, 0);
        assert_eq!(mem.1.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(request(&mut client, CMD_FLUSH, 0, 0).0, 0);
        assert_eq!(mem.1.load(std::sync::atomic::Ordering::Relaxed), 2);
        // Out of range: the payload is consumed and the write refused.
        assert_eq!(request_with(&mut client, CMD_WRITE, 0, 8190, &[1; 4], 4).0, EINVAL);
        assert_eq!(request(&mut client, CMD_READ, 8188, 4).1, [0; 4]);
        let mut disc = REQUEST_MAGIC.to_be_bytes().to_vec();
        disc.extend_from_slice(&[0, 0, 0, 2]);
        disc.extend_from_slice(&[0u8; 20]);
        client.write_all(&disc).unwrap();
        handle.join().unwrap();
    }
}
