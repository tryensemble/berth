//! Joining two byte streams, as the proxy does on both ends.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::UnixStream;

/// A socket that can be read and written from two threads at once.
pub trait Duplex: Read + Write + Send + Sized + 'static {
    fn try_clone(&self) -> io::Result<Self>;
    fn shutdown_write(&self);
}

impl Duplex for UnixStream {
    fn try_clone(&self) -> io::Result<Self> {
        UnixStream::try_clone(self)
    }
    fn shutdown_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
}

impl Duplex for TcpStream {
    fn try_clone(&self) -> io::Result<Self> {
        TcpStream::try_clone(self)
    }
    fn shutdown_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
}

/// Copies both ways until both sides are done, passing on each side's end of
/// input as a half-close.
pub fn splice(a: impl Duplex, b: impl Duplex) -> io::Result<()> {
    let (mut a_in, mut b_out) = (a.try_clone()?, b.try_clone()?);
    let up = std::thread::spawn(move || {
        let _ = io::copy(&mut a_in, &mut b_out);
        b_out.shutdown_write();
    });
    let (mut b_in, mut a_out) = (b, a);
    let _ = io::copy(&mut b_in, &mut a_out);
    a_out.shutdown_write();
    let _ = up.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn bytes_flow_both_ways_and_half_closes_pass_through() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        // An echo server that answers once its input ends.
        std::thread::spawn(move || {
            let (mut c, _) = server.accept().unwrap();
            let mut got = Vec::new();
            c.read_to_end(&mut got).unwrap();
            c.write_all(&got.to_ascii_uppercase()).unwrap();
        });

        let (mut caller, ours) = UnixStream::pair().unwrap();
        let tcp = TcpStream::connect(addr).unwrap();
        let joined = std::thread::spawn(move || splice(ours, tcp));

        caller.write_all(b"hello berth").unwrap();
        caller.shutdown(Shutdown::Write).unwrap();
        let mut answer = String::new();
        caller.read_to_string(&mut answer).unwrap();
        assert_eq!(answer, "HELLO BERTH");
        joined.join().unwrap().unwrap();
    }
}
