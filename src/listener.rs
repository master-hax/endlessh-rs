/* abstracts over listener/stream transport (TCP, unix domain sockets, ...) as
 * a zero cost generic, so callers don't care which transport they were bound on
 */

use std::io::{Read, Write};

use mio::event;
use mio::net::{TcpListener, TcpStream};
#[cfg(unix)]
use mio::net::{UnixListener, UnixStream};

pub trait MioListener: event::Source {
    type Stream: MioStream;
    fn accept_stream(&self) -> std::io::Result<Self::Stream>;
}

impl MioListener for TcpListener {
    type Stream = TcpStream;
    fn accept_stream(&self) -> std::io::Result<TcpStream> {
        let (stream, _addr) = self.accept()?;
        Ok(stream)
    }
}

#[cfg(unix)]
impl MioListener for UnixListener {
    type Stream = UnixStream;
    fn accept_stream(&self) -> std::io::Result<UnixStream> {
        let (stream, _addr) = self.accept()?;
        Ok(stream)
    }
}

pub trait MioStream: Read + Write + event::Source {}
impl MioStream for TcpStream {}
#[cfg(unix)]
impl MioStream for UnixStream {}
