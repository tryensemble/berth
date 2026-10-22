//! Frames of an exec stream: a type byte, a big-endian u32 length, the data.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

/// Input for the command.
pub const STDIN: u8 = 0;
pub const STDOUT: u8 = 1;
pub const STDERR: u8 = 2;
/// The command's exit code: i32, big-endian.
pub const EXIT: u8 = 3;
/// A new terminal size: rows u16, cols u16.
pub const RESIZE: u8 = 4;
/// The caller's stdin ended.
pub const EOF: u8 = 5;
/// The command started: its pid in the berth, u32.
pub const STARTED: u8 = 6;
/// The command couldn't be run: text.
pub const ERROR: u8 = 7;

/// The largest frame either side accepts.
pub const MAX_FRAME: usize = 16 << 20;

pub fn write_frame(w: &mut impl Write, kind: u8, data: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(5 + data.len());
    buf.push(kind);
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    buf.extend_from_slice(data);
    w.write_all(&buf)
}

/// The next frame, or None when the stream ended cleanly between frames.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(head[1..].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::other("frame too large"));
    }
    let mut data = vec![0u8; len];
    r.read_exact(&mut data)?;
    Ok(Some((head[0], data)))
}

/// A frame writer several threads share.
#[derive(Clone)]
pub struct Frames(pub Arc<Mutex<UnixStream>>);

impl Frames {
    pub fn new(s: UnixStream) -> Self {
        Frames(Arc::new(Mutex::new(s)))
    }

    pub fn send(&self, kind: u8, data: &[u8]) -> io::Result<()> {
        write_frame(&mut *self.0.lock().unwrap(), kind, data)
    }

    pub fn shutdown(&self) {
        let _ = self.0.lock().unwrap().shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, STDOUT, b"hello").unwrap();
        write_frame(&mut buf, EXIT, &7i32.to_be_bytes()).unwrap();
        write_frame(&mut buf, EOF, &[]).unwrap();
        assert_eq!(&buf[..5], &[STDOUT, 0, 0, 0, 5]);

        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r).unwrap(), Some((STDOUT, b"hello".to_vec())));
        assert_eq!(read_frame(&mut r).unwrap(), Some((EXIT, 7i32.to_be_bytes().to_vec())));
        assert_eq!(read_frame(&mut r).unwrap(), Some((EOF, vec![])));
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn a_stream_cut_inside_a_frame_is_an_error() {
        let mut buf = Vec::new();
        write_frame(&mut buf, STDOUT, b"hello").unwrap();
        buf.truncate(7);
        assert_eq!(read_frame(&mut &buf[..]).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn oversized_frames_are_refused() {
        let mut head = vec![STDIN];
        head.extend_from_slice(&(MAX_FRAME as u32 + 1).to_be_bytes());
        assert!(read_frame(&mut &head[..]).is_err());
    }

    #[test]
    fn shared_writers_interleave_whole_frames() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let frames = Frames::new(a);
        let threads: Vec<_> = (0..4u8)
            .map(|i| {
                let f = frames.clone();
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        f.send(STDOUT, &[i; 100]).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        frames.shutdown();
        let mut n = 0;
        while let Some((kind, data)) = read_frame(&mut b).unwrap() {
            assert_eq!(kind, STDOUT);
            assert!(data.iter().all(|&x| x == data[0]));
            n += 1;
        }
        assert_eq!(n, 200);
    }
}
