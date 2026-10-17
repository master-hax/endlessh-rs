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

pub struct LogTimestamp(jiff::Timestamp);

impl Display for LogTimestamp {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.3}", self.0)
    }
}

pub fn log_timestamp() -> LogTimestamp {
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
            NewLine::LF => &['\n' as u8],
            NewLine::CRLF => &['\r' as u8, '\n' as u8],
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
    pub started_time: Instant,
    pub last_known_time: Instant,
    pub connections_opened: usize,
    pub connections_closed: usize,
    pub bytes_generated: usize,
    pub bytes_sent: usize,
    pub trapped_time: Duration,
}

impl Default for EndlesshStats {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            started_time: now,
            last_known_time: now,
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
                "endlessh_uptime_seconds: {}\n",
                "endlessh_ssh_trapped_time_seconds: {}\n",
                "endlessh_ssh_connections_opened: {}\n",
                "endlessh_ssh_connections_closed: {}\n",
                "endlessh_ssh_bytes_generated: {}\n",
                "endlessh_ssh_bytes_sent: {}\n",
            ),
            self.last_known_time
                .duration_since(self.started_time)
                .as_secs(),
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
    listeners: Vec<ListenerEntry<Listener>>,
    next_accept: usize,
    line_buffer: [u8; SSH_LINE_BUFFER_SIZE],
    connections: VecDeque<EndlesshConnection<Listener::Stream>>,
    stats: EndlesshStats,
    options: EndlesshOptions,
    verbosity: Verbosity,
}

struct ListenerEntry<Listener: MioListener> {
    listener: Listener,
    token: Token,
    accept_available: bool,
}

struct EndlesshConnection<Stream: MioStream> {
    stream: Stream,
    connected_time: Instant,
    last_send_time: Option<Instant>,
    bytes_sent: usize,
}

// Limit each accept burst to keep signals and periodic banner writes
// responsive during connection floods.
const MAX_ACCEPTS_PER_TICK: usize = 128;
const PENDING_ACCEPT_RETRY: Duration = Duration::from_millis(20);

impl<Listener: MioListener> EndlesshServer<Listener> {
    /// Backwards-compatible constructor for a single listener.
    pub fn new(
        options: EndlesshOptions,
        listener: Listener,
        listener_token: Token,
        poll: &Poll,
        verbosity: Verbosity,
    ) -> Result<Self, EndlesshFatalError> {
        Self::with_listeners(options, vec![(listener, listener_token)], poll, verbosity)
    }

    /// Register multiple listeners in one event loop with a shared global limit.
    pub fn with_listeners(
        options: EndlesshOptions,
        listeners: Vec<(Listener, Token)>,
        poll: &Poll,
        verbosity: Verbosity,
    ) -> Result<Self, EndlesshFatalError> {
        if listeners.is_empty() {
            return Err(EndlesshFatalError::InvalidOptions(Cow::Borrowed(
                "at least one ssh listener is required",
            )));
        }
        if options.max_clients == 0 {
            return Err(EndlesshFatalError::InvalidOptions(Cow::Borrowed(
                "ssh-max-clients must be positive",
            )));
        }
        if options.message_delay.is_zero() {
            return Err(EndlesshFatalError::InvalidOptions(Cow::Borrowed(
                "ssh-message-delay-ms must be positive",
            )));
        }
        let newline_len = options.newline.get_data().len();
        if options.banner_line_length > SSH_LINE_BUFFER_SIZE - newline_len {
            return Err(EndlesshFatalError::InvalidOptions(Cow::Owned(format!(
                "banner_line_length must be at most {}",
                SSH_LINE_BUFFER_SIZE - newline_len
            ))));
        }
        let mut line_buffer = [0_u8; SSH_LINE_BUFFER_SIZE];
        line_buffer[options.banner_line_length..options.banner_line_length + newline_len]
            .copy_from_slice(options.newline.get_data());
        let connections = VecDeque::with_capacity(options.max_clients);
        let mut listener_entries = Vec::with_capacity(listeners.len());

        for (mut listener, token) in listeners {
            if listener_entries
                .iter()
                .any(|entry: &ListenerEntry<Listener>| entry.token == token)
            {
                return Err(EndlesshFatalError::InvalidOptions(Cow::Borrowed(
                    "listener tokens must be distinct",
                )));
            }
            poll.registry()
                .register(&mut listener, token, Interest::READABLE)?;
            listener_entries.push(ListenerEntry {
                listener,
                token,
                accept_available: false,
            });
        }

        Ok(EndlesshServer {
            listeners: listener_entries,
            next_accept: 0,
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
        if let Some(entry) = self
            .listeners
            .iter_mut()
            .find(|entry| entry.token == event.token())
        {
            entry.accept_available = true;
            self.accept_new_connections(now);
            true
        } else {
            false
        }
    }

    pub fn handle_wakeup(&mut self, now: &Instant) -> Option<Duration> {
        assert!(*now >= self.stats.last_known_time, "time went backwards!");
        self.stats.last_known_time = *now;
        // Mio may use edge-triggered readiness. Retry listeners left ready
        // when the previous accept burst reached its budget.
        if self.has_pending_accepts() {
            self.accept_new_connections(now);
        }

        let mut generated_line = false;
        // Process each existing connection once. A waiting client must not
        // prevent newly accepted clients from receiving their first line.
        let queued = self.connections.len();
        for _ in 0..queued {
            let mut connection = self.connections.pop_front().unwrap();
            let send_or_wait = connection.last_send_time.and_then(|last_send| {
                (last_send + self.options.message_delay).checked_duration_since(*now)
            });

            if send_or_wait.is_some() {
                self.connections.push_back(connection);
                continue;
            }

            if !generated_line {
                Self::rand_line(&mut self.line_buffer[..self.options.banner_line_length]);
                self.stats.bytes_generated += self.options.banner_line_length;
                generated_line = true;
            }
            if self.send_line(&mut connection, now) {
                self.connections.push_back(connection);
            } else {
                self.stats.connections_closed += 1;
                self.accept_new_connections(now);
            }
        }

        // Schedule the earliest next line across both address families.
        let next_banner = self
            .connections
            .iter()
            .map(|connection| {
                connection
                    .last_send_time
                    .map_or(PENDING_ACCEPT_RETRY, |last_send| {
                        (last_send + self.options.message_delay).saturating_duration_since(*now)
                    })
            })
            .min();
        self.next_timeout(next_banner)
    }

    fn has_pending_accepts(&self) -> bool {
        self.connections.len() < self.options.max_clients
            && self.listeners.iter().any(|entry| entry.accept_available)
    }

    fn next_timeout(&self, banner_wait: Option<Duration>) -> Option<Duration> {
        if self.has_pending_accepts() {
            Some(banner_wait.map_or(PENDING_ACCEPT_RETRY, |wait| wait.min(PENDING_ACCEPT_RETRY)))
        } else {
            banner_wait
        }
    }

    pub fn stats(&mut self) -> &EndlesshStats {
        &self.stats
    }

    fn accept_new_connections(&mut self, now: &Instant) {
        // Alternate ready listeners so a busy IPv4 source cannot starve IPv6 (or vice versa).
        let mut attempts = 0;
        while self.connections.len() < self.options.max_clients && attempts < MAX_ACCEPTS_PER_TICK {
            let mut accepted = false;
            let listener_count = self.listeners.len();
            for offset in 0..listener_count {
                let index = (self.next_accept + offset) % listener_count;
                if !self.listeners[index].accept_available {
                    continue;
                }
                match self.listeners[index].listener.accept_stream() {
                    Ok(stream) => {
                        attempts += 1;
                        accepted = true;
                        self.next_accept = (index + 1) % listener_count;
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
                        break;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        self.listeners[index].accept_available = false;
                    }
                    Err(e) => {
                        eprintln!("failed to accept ssh connection: {}", e);
                        self.listeners[index].accept_available = false;
                    }
                }
            }
            if !accepted {
                break;
            }
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
        connection: &mut EndlesshConnection<Listener::Stream>,
        now: &Instant,
    ) -> bool {
        match connection.stream.write(
            &self.line_buffer
                [..self.options.banner_line_length + self.options.newline.get_data().len()],
        ) {
            Ok(0) => {
                self.log_close(connection, now);
                false
            }
            Ok(n) => {
                self.stats.bytes_sent += n;
                connection.bytes_sent += n;
                self.stats.trapped_time += now.duration_since(
                    connection
                        .last_send_time
                        .unwrap_or(connection.connected_time),
                );
                connection.last_send_time = Some(*now);
                true
            }
            Err(ref err) if err.kind() == ErrorKind::WouldBlock => true,
            Err(_) => {
                self.log_close(connection, now);
                false
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

