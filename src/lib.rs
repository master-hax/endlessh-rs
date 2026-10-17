use std::borrow::Cow;
use std::fmt::Display;
use std::time::{Duration, Instant};

use mio::{event, Interest};
use mio::{Poll, Token};
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
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
    let duration =
        jiff::SignedDuration::new(since_epoch.as_secs() as i64, since_epoch.subsec_nanos() as i32);
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
    pub max_clients_per_ip: usize,
    pub banner_line_length: usize,
    pub message_delay: Duration,
    pub newline: NewLine,
}

impl Default for EndlesshOptions {
    fn default() -> Self {
        EndlesshOptions {
            max_clients: 4096,
            max_clients_per_ip: 16,
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
    listener: Listener,
    listener_token: Token,
    listener_accept_available: bool,
    line_buffer: [u8; SSH_LINE_BUFFER_SIZE],
    connections: VecDeque<EndlesshConnection<Listener::Stream>>,
    connections_by_ip: HashMap<IpAddr, usize>,
    stats: EndlesshStats,
    options: EndlesshOptions,
    verbosity: Verbosity,
}

struct EndlesshConnection<Stream: MioStream> {
    stream: Stream,
    peer_ip: Option<IpAddr>,
    connected_time: Instant,
    last_send_time: Option<Instant>,
    bytes_sent: usize,
}

/// Normalize IPv4-mapped IPv6 addresses for quota accounting.
fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip6) => ip6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip6)),
        ip4 => ip4,
    }
}

fn reserve_ip(counts: &mut HashMap<IpAddr, usize>, ip: Option<IpAddr>, limit: usize) -> bool {
    if let Some(ip) = ip {
        let current = counts.get(&ip).copied().unwrap_or(0);
        if current >= limit {
            return false;
        }
        *counts.entry(ip).or_insert(0) += 1;
    }
    true
}

fn release_ip(counts: &mut HashMap<IpAddr, usize>, ip: IpAddr) {
    if let Some(count) = counts.get_mut(&ip) {
        *count -= 1;
        if *count == 0 {
            counts.remove(&ip);
        }
    }
}

// Bound accept bursts, including rejected clients, to keep the event loop responsive.
const MAX_ACCEPTS_PER_TICK: usize = 128;
const PENDING_ACCEPT_RETRY: Duration = Duration::from_millis(20);

impl<Listener: MioListener> EndlesshServer<Listener> {
    /// Creates an [`EndlesshServer`] that accepts ssh connections on `listener`.
    pub fn new(
        options: EndlesshOptions,
        mut listener: Listener,
        listener_token: Token,
        poll: &Poll,
        verbosity: Verbosity,
    ) -> Result<Self, EndlesshFatalError> {
        if options.max_clients_per_ip == 0 {
            return Err(EndlesshFatalError::InvalidOptions(Cow::Borrowed(
                "ssh-max-clients-per-ip must be positive",
            )));
        }
        let newline_len = options.newline.get_data().len();
        if options.banner_line_length + newline_len > SSH_LINE_BUFFER_SIZE {
            return Err(EndlesshFatalError::InvalidOptions(Cow::Owned(format!(
                "banner_line_length must be at most {}",
                SSH_LINE_BUFFER_SIZE - newline_len
            ))));
        }
        let mut line_buffer = [0_u8; SSH_LINE_BUFFER_SIZE];
        line_buffer[options.banner_line_length..options.banner_line_length + newline_len]
            .copy_from_slice(&options.newline.get_data());
        let connections = VecDeque::with_capacity(options.max_clients);

        poll.registry()
            .register(&mut listener, listener_token, Interest::READABLE)?;

        Ok(EndlesshServer {
            listener,
            listener_token,
            listener_accept_available: false,
            line_buffer,
            connections,
            connections_by_ip: HashMap::new(),
            stats: EndlesshStats::default(),
            options,
            verbosity,
        })
    }

    /// Returns `true` if the event was handled, otherwise `false`.
    pub fn try_handle_event(&mut self, event: &event::Event, now: &Instant) -> bool {
        assert!(*now >= self.stats.last_known_time, "time went backwards!");
        self.stats.last_known_time = *now;
        return if self.listener_token == event.token() {
            self.listener_accept_available = true;
            self.accept_new_connections(now);
            true
        } else {
            false
        };
    }

    pub fn handle_wakeup(&mut self, now: &Instant) -> Option<Duration> {
        assert!(*now >= self.stats.last_known_time, "time went backwards!");
        self.stats.last_known_time = *now;
        let mut generated_line = false;
        while let Some(connection) = self.connections.pop_front() {
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
                    return Some(if self.listener_accept_available
                        && self.connections.len() < self.options.max_clients
                    {
                        need_to_wait.min(PENDING_ACCEPT_RETRY)
                    } else {
                        need_to_wait
                    });
                }
            }
        }
        if self.listener_accept_available && self.connections.len() < self.options.max_clients {
            self.accept_new_connections(now);
            return Some(PENDING_ACCEPT_RETRY);
        }
        None
    }

    pub fn stats(&mut self) -> &EndlesshStats {
        &self.stats
    }

    fn accept_new_connections(&mut self, now: &Instant) {
        let mut attempts = 0;
        while self.listener_accept_available
            && self.connections.len() < self.options.max_clients
            && attempts < MAX_ACCEPTS_PER_TICK
        {
            match self.listener.accept_stream() {
                Ok(stream) => {
                    attempts += 1;
                    let peer_ip = match stream.host_description() {
                        HostDescription::Ip(ip) => Some(canonical_ip(ip)),
                        _ => None,
                    };
                    if !reserve_ip(
                        &mut self.connections_by_ip,
                        peer_ip,
                        self.options.max_clients_per_ip,
                    ) {
                        if self.verbosity >= Verbosity::Debug {
                            println!(
                                "{} REJECT host={} reason=per-ip-limit",
                                log_timestamp(),
                                stream.host_description()
                            );
                        }
                        continue;
                    }
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
                        peer_ip,
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
                if let Some(ip) = connection.peer_ip {
                    release_ip(&mut self.connections_by_ip, ip);
                }
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
                // couldn't send - oh well
                Some(connection)
            }
            Err(_e) => {
                // 🤷 goodbye 👋
                self.log_close(&connection, now);
                if let Some(ip) = connection.peer_ip {
                    release_ip(&mut self.connections_by_ip, ip);
                }
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
mod per_ip_tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn ipv4_mapped_ipv6_uses_ipv4_quota() {
        let ipv4 = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let mapped = IpAddr::V6(Ipv4Addr::new(192, 0, 2, 1).to_ipv6_mapped());
        assert_eq!(canonical_ip(ipv4), canonical_ip(mapped));
    }

    #[test]
    fn quota_counts_active_connections_and_releases_on_close() {
        let ip = Some("192.0.2.1".parse().unwrap());
        let other = Some("192.0.2.2".parse().unwrap());
        let mut counts = HashMap::new();
        assert!(reserve_ip(&mut counts, ip, 2));
        assert!(reserve_ip(&mut counts, ip, 2));
        assert!(!reserve_ip(&mut counts, ip, 2));
        assert!(reserve_ip(&mut counts, other, 2));
        release_ip(&mut counts, ip.unwrap());
        assert!(reserve_ip(&mut counts, ip, 2));
        release_ip(&mut counts, ip.unwrap());
        release_ip(&mut counts, ip.unwrap());
        assert!(!counts.contains_key(&ip.unwrap()));
    }

    #[test]
    fn unix_clients_do_not_consume_ip_quota() {
        let mut counts = HashMap::new();
        assert!(reserve_ip(&mut counts, None, 1));
        assert!(counts.is_empty());
    }
}
