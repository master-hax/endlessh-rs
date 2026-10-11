/* abstracts over listener/stream transport (TCP, unix domain sockets, ...) as
 * a zero cost generic, so callers don't care which transport they were bound on
 */

use std::io::{Read, Result, Write};

use mio::event;
use mio::net::{TcpListener, TcpStream};
#[cfg(unix)]
use mio::net::{UnixListener, UnixStream};
use mio::{Interest, Registry, Token};

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

pub enum AnyListener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(UnixListener),
}

impl From<TcpListener> for AnyListener {
    fn from(listener: TcpListener) -> Self {
        AnyListener::Tcp(listener)
    }
}

#[cfg(unix)]
impl From<UnixListener> for AnyListener {
    fn from(listener: UnixListener) -> Self {
        AnyListener::Unix(listener)
    }
}

impl event::Source for AnyListener {
    fn register(&mut self, registry: &Registry, token: Token, interests: Interest) -> Result<()> {
        match self {
            AnyListener::Tcp(l) => l.register(registry, token, interests),
            #[cfg(unix)]
            AnyListener::Unix(l) => l.register(registry, token, interests),
        }
    }

    fn reregister(&mut self, registry: &Registry, token: Token, interests: Interest) -> Result<()> {
        match self {
            AnyListener::Tcp(l) => l.reregister(registry, token, interests),
            #[cfg(unix)]
            AnyListener::Unix(l) => l.reregister(registry, token, interests),
        }
    }

    fn deregister(&mut self, registry: &Registry) -> Result<()> {
        match self {
            AnyListener::Tcp(l) => l.deregister(registry),
            #[cfg(unix)]
            AnyListener::Unix(l) => l.deregister(registry),
        }
    }
}

impl MioListener for AnyListener {
    type Stream = AnyStream;
    fn accept_stream(&self) -> Result<AnyStream> {
        match self {
            AnyListener::Tcp(l) => l.accept_stream().map(AnyStream::Tcp),
            #[cfg(unix)]
            AnyListener::Unix(l) => l.accept_stream().map(AnyStream::Unix),
        }
    }
}

pub enum AnyStream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(UnixStream),
}

impl Read for AnyStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        match self {
            AnyStream::Tcp(s) => s.read(buf),
            #[cfg(unix)]
            AnyStream::Unix(s) => s.read(buf),
        }
    }
}

impl Write for AnyStream {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        match self {
            AnyStream::Tcp(s) => s.write(buf),
            #[cfg(unix)]
            AnyStream::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> Result<()> {
        match self {
            AnyStream::Tcp(s) => s.flush(),
            #[cfg(unix)]
            AnyStream::Unix(s) => s.flush(),
        }
    }
}

impl event::Source for AnyStream {
    fn register(&mut self, registry: &Registry, token: Token, interests: Interest) -> Result<()> {
        match self {
            AnyStream::Tcp(s) => s.register(registry, token, interests),
            #[cfg(unix)]
            AnyStream::Unix(s) => s.register(registry, token, interests),
        }
    }

    fn reregister(&mut self, registry: &Registry, token: Token, interests: Interest) -> Result<()> {
        match self {
            AnyStream::Tcp(s) => s.reregister(registry, token, interests),
            #[cfg(unix)]
            AnyStream::Unix(s) => s.reregister(registry, token, interests),
        }
    }

    fn deregister(&mut self, registry: &Registry) -> Result<()> {
        match self {
            AnyStream::Tcp(s) => s.deregister(registry),
            #[cfg(unix)]
            AnyStream::Unix(s) => s.deregister(registry),
        }
    }
}

impl MioStream for AnyStream {}
