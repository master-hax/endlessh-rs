use std::borrow::Cow;
use std::fmt::Display;
use std::time::{Duration, Instant};

use mio::{event, Interest};
use mio::{Poll, Token};
use std::collections::VecDeque;
use std::fmt::Formatter;
use std::io::{ErrorKind, Write};

mod listener;
pub use listener::{AnyListener, HostDescription, MioListener, MioStream};

#[cfg(feature = "metrics")]
pub mod metrics;

const SSH_LINE_BUFFER_SIZE: usize = 256;

struct LogTimestamp(jiff::Timestamp);

impl Display for LogTimestamp {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.3}", self.0)
    }
}

fn log_timestamp() -> LogTimestamp {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let duration = jiff::SignedDuration::new(
        since_epoch.as_secs() as i64,
        since_epoch.subsec_nanos() as i32,
    );
    LogTimestamp(jiff::Timestamp::from_duration(duration).unwrap_or(jiff::Timestamp::UNIX_EPOCH))
}

pub enum NewLine {
    LF,
    #[allow(dead_code)]
    CRLF,
}

impl NewLine {
    fn get_data(&self) -> &[u8] {
        match self {
            NewLine::LF => b"\n",
            NewLine::CRLF => b"\r\n",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verbosity {
    Quiet,
    Info,
    Debug,
}

impl From<u8> for Verbosity {
    fn from(count: u8) -> Self {
        match count {
            0 => Verbosity::Quiet,
            1 => Verbosity::Info,
            _ => Verbosity::Debug,
        }
    }
}

#[derive(Debug)]
pub enum EndlesshFatalError {
    InvalidOptions(Cow<'static, str>),
    Io(std::io::Error),
}

impl Display for EndlesshFatalError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            EndlesshFatalError::InvalidOptions(msg) => write!(f, "{msg}"),
            EndlesshFatalError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EndlesshFatalError {}

impl From<std::io::Error> for EndlesshFatalError {
    fn from(e: std::io::Error) -> Self {
        EndlesshFatalError::Io(e)
    }
}

pub struct EndlesshOptions {
    pub max_clients: usize,
    pub banner_line_length: usize,
    pub message_delay: Duration,
    pub newline: NewLine,
}

impl Default for EndlesshOptions {
    fn default() -> Self {
        EndlesshOptions {
            max_clients: 4096,
            banner_line_length: 32,
            message_delay: Duration::from_secs(10),
            newline: NewLine::LF,
        }
    }
}

pub struct EndlesshStats {
    pub last_known_time: Instant,
    pub connections_opened: usize,
    pub connections_closed: usize,
    pub bytes_generated: usize,
    pub bytes_sent: usize,
    pub trapped_time: Duration,
}

impl Default for EndlesshStats {
    fn default() -> Self {
        Self {
            last_known_time: Instant::now(),
            trapped_time: Duration::ZERO,
            connections_opened: 0,
            connections_closed: 0,
            bytes_generated: 0,
            bytes_sent: 0,
        }
    }
}

impl Display for EndlesshStats {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(
            f,
            concat!(
                "endlessh_ssh_trapped_time_seconds: {}\n",
                "endlessh_ssh_connections_opened: {}\n",
                "endlessh_ssh_connections_closed: {}\n",
                "endlessh_ssh_bytes_generated: {}\n",
                "endlessh_ssh_bytes_sent: {}\n",
            ),
            self.trapped_time.as_secs(),
            self.connections_opened,
            self.connections_closed,
            self.bytes_generated,
            self.bytes_sent,
        )?;
        Ok(())
    }
}

pub struct EndlesshServer<Listener: MioListener> {
    listener: Listener,
    listener_token: Token,
    listener_accept_available: bool,
    line_buffer: [u8; SSH_LINE_BUFFER_SIZE],
    connections: VecDeque<EndlesshConnection<Listener::Stream>>,
    stats: EndlesshStats,
    options: EndlesshOptions,
    verbosity: Verbosity,
}

struct EndlesshConnection<Stream: MioStream> {
    stream: Stream,
    connected_time: Instant,
    last_send_time: Option<Instant>,
    bytes_sent: usize,
}

impl<Listener: MioListener> EndlesshServer<Listener> {
    /// Creates an [`EndlesshServer`] that accepts ssh connections on `listener`.
    pub fn new(
        options: EndlesshOptions,
        mut listener: Listener,
        listener_token: Token,
        poll: &Poll,
        verbosity: Verbosity,
    ) -> Result<Self, EndlesshFatalError> {
        let newline_len = options.newline.get_data().len();
        if options.banner_line_length + newline_len > SSH_LINE_BUFFER_SIZE {
            return Err(EndlesshFatalError::InvalidOptions(Cow::Owned(format!(
                "banner_line_length must be at most {}",
                SSH_LINE_BUFFER_SIZE - newline_len
            ))));
        }
        let mut line_buffer = [0_u8; SSH_LINE_BUFFER_SIZE];
        line_buffer[options.banner_line_length..options.banner_line_length + newline_len]
            .copy_from_slice(options.newline.get_data());
        let connections = VecDeque::with_capacity(options.max_clients);

        poll.registry()
            .register(&mut listener, listener_token, Interest::READABLE)?;

        Ok(EndlesshServer {
            listener,
            listener_token,
            listener_accept_available: false,
            line_buffer,
            connections,
            stats: EndlesshStats::default(),
            options,
            verbosity,
        })
    }

    /// Returns `true` if the event was handled, otherwise `false`.
    pub fn try_handle_event(&mut self, event: &event::Event, now: &Instant) -> bool {
        assert!(*now >= self.stats.last_known_time, "time went backwards!");
        self.stats.last_known_time = *now;
        if self.listener_token == event.token() {
            self.listener_accept_available = true;
            self.accept_new_connections(now);
            true
        } else {
            false
        }
    }

    pub fn handle_wakeup(&mut self, now: &Instant) -> Option<Duration> {
        assert!(*now >= self.stats.last_known_time, "time went backwards!");
        self.stats.last_known_time = *now;
        let mut generated_line = false;
        // visit at most one connection per queued slot, so re-queued/new connections can't loop forever
        let mut remaining = self.connections.len();
        while remaining > 0 {
            remaining -= 1;
            let Some(connection) = self.connections.pop_front() else {
                break;
            };
            let send_or_wait: Option<Duration> = match connection.last_send_time {
                None => {
                    // connection has never received a line - send immediately
                    None
                }
                Some(last_send) => {
                    // connection has received a line before - send if the message window has elapsed
                    (last_send + self.options.message_delay).checked_duration_since(*now)
                }
            };

            match send_or_wait {
                None => {
                    if !generated_line {
                        Self::rand_line(&mut self.line_buffer[..self.options.banner_line_length]);
                        self.stats.bytes_generated += self.options.banner_line_length;
                        generated_line = true;
                    }
                    match self.send_line(connection, now) {
                        Some(c) => self.connections.push_back(c),
                        None => {
                            // drop the connection
                            self.stats.connections_closed += 1;
                            self.accept_new_connections(now);
                        }
                    }
                }
                Some(need_to_wait) => {
                    self.connections.push_back(connection);
                    return Some(need_to_wait);
                }
            }
        }
        if self.connections.is_empty() {
            // no connections left to wait on
            None
        } else {
            // all queued connections were just sent to - retry after message_delay
            Some(self.options.message_delay)
        }
    }

    pub fn stats(&mut self) -> &EndlesshStats {
        &self.stats
    }

    fn accept_new_connections(&mut self, now: &Instant) {
        while self.listener_accept_available && self.connections.len() < self.options.max_clients {
            match self.listener.accept_stream() {
                Ok(stream) => {
                    stream.shrink_receive_buffer();
                    if self.verbosity >= Verbosity::Info {
                        println!(
                            "{} ACCEPT host={}{}{} n={}/{}",
                            log_timestamp(),
                            stream.host_description(),
                            stream
                                .port()
                                .map(|port| format!(" port={port}"))
                                .unwrap_or_default(),
                            stream
                                .raw_fd()
                                .map(|fd| format!(" fd={fd}"))
                                .unwrap_or_default(),
                            self.connections.len() + 1,
                            self.options.max_clients
                        );
                    }
                    self.connections.push_back(EndlesshConnection {
                        stream,
                        connected_time: *now,
                        last_send_time: None,
                        bytes_sent: 0,
                    });
                    self.stats.connections_opened += 1;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    self.listener_accept_available = false;
                }
                Err(e) => {
                    eprintln!("failed to accept ssh connection: {}", e);
                    break;
                }
            };
        }
    }

    // the SSH client will try to parse lines starting with "SSH-", ending the banner
    // the "alphanumeric" distribution never generates '-' so should not be a problem
    // see https://datatracker.ietf.org/doc/html/rfc4253#section-4.2 for more
    fn rand_line(buffer: &mut [u8]) {
        for element in buffer.iter_mut() {
            *element = fastrand::alphanumeric() as u8;
        }
    }

    fn send_line(
        &mut self,
        mut connection: EndlesshConnection<Listener::Stream>,
        now: &Instant,
    ) -> Option<EndlesshConnection<Listener::Stream>> {
        match connection.stream.write(
            &self.line_buffer
                [..self.options.banner_line_length + self.options.newline.get_data().len()],
        ) {
            Ok(0) => {
                // connection closed, goodbye 👋
                self.log_close(&connection, now);
                None
            }
            Ok(n) => {
                // send (at least partially) succeeded
                self.stats.bytes_sent += n;
                connection.bytes_sent += n;
                self.stats.trapped_time += now.duration_since(
                    connection
                        .last_send_time
                        .unwrap_or(connection.connected_time),
                );

                connection.last_send_time = Some(*now);
                Some(connection)
            }
            Err(ref err) if err.kind() == ErrorKind::WouldBlock => {
                // avoid busy-loop when all connections are blocked
                connection.last_send_time = Some(*now);
                Some(connection)
            }
            Err(_e) => {
                // 🤷 goodbye 👋
                self.log_close(&connection, now);
                None
            }
        }
    }

    fn log_close(&self, connection: &EndlesshConnection<Listener::Stream>, now: &Instant) {
        if self.verbosity >= Verbosity::Info {
            println!(
                "{} CLOSE host={}{}{} time={:.3} bytes={}",
                log_timestamp(),
                connection.stream.host_description(),
                connection
                    .stream
                    .port()
                    .map(|port| format!(" port={port}"))
                    .unwrap_or_default(),
                connection
                    .stream
                    .raw_fd()
                    .map(|fd| format!(" fd={fd}"))
                    .unwrap_or_default(),
                now.duration_since(connection.connected_time).as_secs_f64(),
                connection.bytes_sent
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mio::Registry;

    #[derive(Clone)]
    struct BlockedStream {
        write_attempts: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl std::io::Read for BlockedStream {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
    }

    impl Write for BlockedStream {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            self.write_attempts.set(self.write_attempts.get() + 1);
            Err(std::io::Error::from(ErrorKind::WouldBlock))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl event::Source for BlockedStream {
        fn register(&mut self, _r: &Registry, _t: Token, _i: Interest) -> std::io::Result<()> {
            Ok(())
        }
        fn reregister(&mut self, _r: &Registry, _t: Token, _i: Interest) -> std::io::Result<()> {
            Ok(())
        }
        fn deregister(&mut self, _r: &Registry) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl MioStream for BlockedStream {
        fn host_description(&self) -> HostDescription {
            HostDescription::Unknown
        }
    }

    struct NoAcceptListener;

    impl event::Source for NoAcceptListener {
        fn register(&mut self, _r: &Registry, _t: Token, _i: Interest) -> std::io::Result<()> {
            Ok(())
        }
        fn reregister(&mut self, _r: &Registry, _t: Token, _i: Interest) -> std::io::Result<()> {
            Ok(())
        }
        fn deregister(&mut self, _r: &Registry) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl MioListener for NoAcceptListener {
        type Stream = BlockedStream;
        fn accept_stream(&self) -> std::io::Result<BlockedStream> {
            Err(std::io::Error::from(ErrorKind::WouldBlock))
        }
    }

    // once a connection's write returns WouldBlock, it must not be retried again until
    // message_delay elapses - retrying on every wakeup while blocked is the busy loop
    #[test]
    fn blocked_write_is_not_retried_before_message_delay() {
        let poll = Poll::new().unwrap();
        let message_delay = Duration::from_secs(10);
        let mut server = EndlesshServer::new(
            EndlesshOptions {
                message_delay,
                ..Default::default()
            },
            NoAcceptListener,
            Token(0),
            &poll,
            Verbosity::Quiet,
        )
        .unwrap();

        let write_attempts = std::rc::Rc::new(std::cell::Cell::new(0));
        let now = Instant::now();
        for _ in 0..5 {
            server.connections.push_back(EndlesshConnection {
                stream: BlockedStream {
                    write_attempts: write_attempts.clone(),
                },
                connected_time: now,
                last_send_time: None,
                bytes_sent: 0,
            });
        }

        // first wakeup attempts to send to every connection, all of which block
        server.handle_wakeup(&now);
        assert_eq!(write_attempts.get(), 5);

        // a wakeup before message_delay has elapsed must not retry any blocked write
        server.handle_wakeup(&(now + message_delay - Duration::from_millis(1)));
        assert_eq!(write_attempts.get(), 5);

        // once message_delay has elapsed, the blocked connections are retried
        server.handle_wakeup(&(now + message_delay + Duration::from_millis(1)));
        assert_eq!(write_attempts.get(), 10);
    }

    struct HardErrorStream;

    impl std::io::Read for HardErrorStream {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
    }

    impl Write for HardErrorStream {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("connection reset"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl event::Source for HardErrorStream {
        fn register(&mut self, _r: &Registry, _t: Token, _i: Interest) -> std::io::Result<()> {
            Ok(())
        }
        fn reregister(&mut self, _r: &Registry, _t: Token, _i: Interest) -> std::io::Result<()> {
            Ok(())
        }
        fn deregister(&mut self, _r: &Registry) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl MioStream for HardErrorStream {
        fn host_description(&self) -> HostDescription {
            HostDescription::Unknown
        }
    }

    // a listener with an endless accept backlog, so a dropped connection is always
    // immediately replaced with a fresh, ready-to-send one
    struct InfiniteAcceptListener;

    impl event::Source for InfiniteAcceptListener {
        fn register(&mut self, _r: &Registry, _t: Token, _i: Interest) -> std::io::Result<()> {
            Ok(())
        }
        fn reregister(&mut self, _r: &Registry, _t: Token, _i: Interest) -> std::io::Result<()> {
            Ok(())
        }
        fn deregister(&mut self, _r: &Registry) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl MioListener for InfiniteAcceptListener {
        type Stream = HardErrorStream;
        fn accept_stream(&self) -> std::io::Result<HardErrorStream> {
            Ok(HardErrorStream)
        }
    }

    // a connection that keeps getting dropped and replaced must not be revisited
    // within the same wakeup - only connections queued at the start may be visited
    #[test]
    fn dropped_connections_replaced_mid_wakeup_are_not_revisited() {
        let poll = Poll::new().unwrap();
        let max_clients = 50;
        let mut server = EndlesshServer::new(
            EndlesshOptions {
                max_clients,
                ..Default::default()
            },
            InfiniteAcceptListener,
            Token(0),
            &poll,
            Verbosity::Quiet,
        )
        .unwrap();
        server.listener_accept_available = true;

        let now = Instant::now();
        for _ in 0..5 {
            server.connections.push_back(EndlesshConnection {
                stream: HardErrorStream,
                connected_time: now,
                last_send_time: None,
                bytes_sent: 0,
            });
        }

        server.handle_wakeup(&now);

        // only the 5 connections queued at the start of the wakeup were visited and
        // dropped, even though each drop was immediately backfilled from the backlog
        assert_eq!(server.stats.connections_closed, 5);
        assert_eq!(server.connections.len(), max_clients);
    }
}
