/* abstracts over listener/stream transport (TCP, unix domain sockets, ...) as
 * a zero cost generic, so callers don't care which transport they were bound on
 */

use std::io::{Read, Result, Write};
use std::net::{IpAddr, SocketAddr};

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

#[derive(Clone, Copy)]
pub enum HostDescription {
    Unix,
    Ip(IpAddr),
    Unknown,
}

impl std::fmt::Display for HostDescription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostDescription::Unix => write!(f, "unix"),
            HostDescription::Ip(ip) => write!(f, "{ip}"),
            HostDescription::Unknown => write!(f, "unknown"),
        }
    }
}

pub trait MioStream: Read + Write + event::Source {
    fn shrink_receive_buffer(&self) {}
    fn host_description(&self) -> HostDescription;
    fn port(&self) -> Option<u16> {
        None
    }
    fn raw_fd(&self) -> Option<i32> {
        None
    }
}

#[cfg(unix)]
fn set_recv_buffer_floor(socket: &impl std::os::fd::AsFd) {
    if let Err(e) = socket2::SockRef::from(socket).set_recv_buffer_size(1) {
        eprintln!("failed to shrink receive buffer: {}", e);
    }
}

impl MioStream for TcpStream {
    #[cfg(unix)]
    fn shrink_receive_buffer(&self) {
        set_recv_buffer_floor(self);
    }
    fn host_description(&self) -> HostDescription {
        self.peer_addr()
            .map(|a| HostDescription::Ip(a.ip()))
            .unwrap_or(HostDescription::Unknown)
    }
    fn port(&self) -> Option<u16> {
        self.peer_addr().ok().map(|a| a.port())
    }
    #[cfg(unix)]
    fn raw_fd(&self) -> Option<i32> {
        use std::os::fd::AsRawFd;
        Some(self.as_raw_fd())
    }
}
#[cfg(unix)]
impl MioStream for UnixStream {
    fn shrink_receive_buffer(&self) {
        set_recv_buffer_floor(self);
    }
    fn host_description(&self) -> HostDescription {
        HostDescription::Unix
    }
    fn raw_fd(&self) -> Option<i32> {
        use std::os::fd::AsRawFd;
        Some(self.as_raw_fd())
    }
}

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
            AnyListener::Tcp(l) => {
                let (stream, addr) = l.accept()?;
                Ok(AnyStream::Tcp(stream, addr))
            }
            #[cfg(unix)]
            AnyListener::Unix(l) => l.accept_stream().map(AnyStream::Unix),
        }
    }
}

pub enum AnyStream {
    Tcp(TcpStream, SocketAddr),
    #[cfg(unix)]
    Unix(UnixStream),
}

impl Read for AnyStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        match self {
            AnyStream::Tcp(s, _) => s.read(buf),
            #[cfg(unix)]
            AnyStream::Unix(s) => s.read(buf),
        }
    }
}

impl Write for AnyStream {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        match self {
            AnyStream::Tcp(s, _) => s.write(buf),
            #[cfg(unix)]
            AnyStream::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> Result<()> {
        match self {
            AnyStream::Tcp(s, _) => s.flush(),
            #[cfg(unix)]
            AnyStream::Unix(s) => s.flush(),
        }
    }
}

impl event::Source for AnyStream {
    fn register(&mut self, registry: &Registry, token: Token, interests: Interest) -> Result<()> {
        match self {
            AnyStream::Tcp(s, _) => s.register(registry, token, interests),
            #[cfg(unix)]
            AnyStream::Unix(s) => s.register(registry, token, interests),
        }
    }

    fn reregister(&mut self, registry: &Registry, token: Token, interests: Interest) -> Result<()> {
        match self {
            AnyStream::Tcp(s, _) => s.reregister(registry, token, interests),
            #[cfg(unix)]
            AnyStream::Unix(s) => s.reregister(registry, token, interests),
        }
    }

    fn deregister(&mut self, registry: &Registry) -> Result<()> {
        match self {
            AnyStream::Tcp(s, _) => s.deregister(registry),
            #[cfg(unix)]
            AnyStream::Unix(s) => s.deregister(registry),
        }
    }
}

impl MioStream for AnyStream {
    #[cfg(unix)]
    fn shrink_receive_buffer(&self) {
        match self {
            AnyStream::Tcp(s, _) => s.shrink_receive_buffer(),
            AnyStream::Unix(s) => s.shrink_receive_buffer(),
        }
    }
    fn host_description(&self) -> HostDescription {
        match self {
            AnyStream::Tcp(_, addr) => HostDescription::Ip(addr.ip()),
            #[cfg(unix)]
            AnyStream::Unix(s) => s.host_description(),
        }
    }
    fn port(&self) -> Option<u16> {
        match self {
            AnyStream::Tcp(_, addr) => Some(addr.port()),
            #[cfg(unix)]
            AnyStream::Unix(s) => s.port(),
        }
    }
    fn raw_fd(&self) -> Option<i32> {
        match self {
            AnyStream::Tcp(s, _) => s.raw_fd(),
            #[cfg(unix)]
            AnyStream::Unix(s) => s.raw_fd(),
        }
    }
}
