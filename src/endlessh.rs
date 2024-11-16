
use std::fmt::Display;
use std::time::{Instant,Duration};

use std::collections::VecDeque;
use mio::net::{TcpListener,TcpStream};
use mio::{Poll, Token};
use mio::{Interest,event};
use std::io::{ErrorKind, Write};
use std::fmt::Formatter;

const SSH_LINE_BUFFER_SIZE: usize = 256;

pub enum NewLine {
    LF,
    CRLF,
}

impl NewLine {
    fn get_data(&self) -> &[u8] {
        match self {
            NewLine::LF => &[ '\n' as u8 ],
            NewLine::CRLF => &[ '\r' as u8, '\n' as u8 ],
        }
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
            connections_opened: 0,
            connections_closed: 0,
            bytes_generated: 0,
            bytes_sent: 0,
            trapped_time: Duration::ZERO,
        }
    }
}

impl Display for EndlesshStats {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f,
            concat!(
                "endlessh_ssh_uptime_seconds: {}\n",
                "endlessh_ssh_connections_opened: {}\n",
                "endlessh_ssh_connections_closed: {}\n",
                "endlessh_ssh_bytes_generated: {}\n",
                "endlessh_ssh_bytes_sent: {}\n",
                "endlessh_ssh_trapped_time_seconds: {}\n",
            ),
            self.last_known_time.duration_since(self.started_time).as_secs(),
            self.connections_opened,
            self.connections_closed,
            self.bytes_generated,
            self.bytes_sent,
            self.trapped_time.as_secs(),
        )?;
        Ok(())
    }
}

pub struct EndlesshServer {
    listener: TcpListener,
    listener_token: Token,
    listener_accept_available: bool,
    line_buffer: [u8; SSH_LINE_BUFFER_SIZE],
    clients: VecDeque<EndlesshClient>,
    stats: EndlesshStats,
    options: EndlesshOptions,
}

struct EndlesshClient {
    stream: TcpStream,
    connected_time: Instant,
    last_send_time: Option<Instant>,
}

impl EndlesshServer {

    pub fn create(options: EndlesshOptions, mut listener: TcpListener, listener_token: Token, poll: &Poll) -> Self {
        let mut line_buffer = [0_u8; SSH_LINE_BUFFER_SIZE];
        assert!(options.banner_line_length + options.newline.get_data().len() <= SSH_LINE_BUFFER_SIZE);
        line_buffer[options.banner_line_length..options.banner_line_length+options.newline.get_data().len()].copy_from_slice(&options.newline.get_data());
        let clients = VecDeque::with_capacity(options.max_clients);

        poll.registry().register(&mut listener, listener_token, Interest::READABLE).unwrap();

        EndlesshServer {
            listener,
            listener_token,
            listener_accept_available: false,
            line_buffer,
            clients,
            stats: EndlesshStats::default(),
            options,
        }
    }

    pub fn try_handle_event(&mut self, event: &event::Event, now: &Instant) -> bool {
        assert!(*now >= self.stats.last_known_time, "time went backwards!");
        self.stats.last_known_time = *now;
        return if self.listener_token == event.token() {
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
        while let Some(client) = self.clients.pop_front() {

            let send_or_wait: Option<Duration> = match client.last_send_time {
                None => {
                    // client has never received a line - send immediately
                    None
                },
                Some(last_send) => {
                    // client has received a line before - send if the message window has elapsed 
                    (last_send + self.options.message_delay).checked_duration_since(*now)
                },
            };

            match send_or_wait {
                None => {
                    if !generated_line {
                        Self::rand_line(&mut self.line_buffer[..self.options.banner_line_length]);
                        self.stats.bytes_generated += self.options.banner_line_length;
                        generated_line = true;
                    }
                    match self.send_line(client, now) {
                        Some(c) => self.clients.push_back(c),
                        None => {
                            // drop the client
                            self.accept_new_connections(now);
                        },
                    }
                },
                Some(need_to_wait) => {
                    self.clients.push_back(client);
                    return Some(need_to_wait);
                }
            }

        }
        None
    }

    pub fn stats(&mut self) -> &EndlesshStats {
        &self.stats
    }

    fn accept_new_connections(&mut self, now: &Instant) {
        while self.listener_accept_available && self.clients.len() < self.options.max_clients {
            match self.listener.accept() {
                Ok((stream, _address)) => {
                    self.clients.push_back(EndlesshClient {
                        stream,
                        connected_time: *now,
                        last_send_time: None,
                    });
                    self.stats.connections_opened += 1;
                },
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    self.listener_accept_available = false;
                }
                Err(e) => {
                    panic!("failed to accept connection: {}", e);
                }
            };
        }
    }

    // the SSH client will try to parse lines starting with "SSH-", ending the banner
    // the "alphanumeric" distribution never generates '-' so should not be a problem
    // see https://datatracker.ietf.org/doc/html/rfc4253#section-4.2 for more
    fn rand_line(buffer: &mut[u8]) {
        for element in buffer.iter_mut() {
            *element = fastrand::alphanumeric() as u8;
        }
    }

    fn send_line(&mut self, mut client: EndlesshClient, now: &Instant) -> Option<EndlesshClient> {
        match client.stream.write(&self.line_buffer[..self.options.banner_line_length + self.options.newline.get_data().len()]) {
            Ok(0) => {
                // client disconnected, goodbye 👋
                None
            },
            Ok(n) => {
                // send (at least partially) succeeded
                self.stats.bytes_sent += n;
                self.stats.trapped_time += now.duration_since(client.last_send_time.unwrap_or(client.connected_time));
                
                client.last_send_time = Some(*now);
                Some(client)
            },
            Err(ref err) if err.kind() == ErrorKind::WouldBlock => {
                // couldn't send - oh well
                Some(client)
            },
            Err(_e) => {
                // 🤷 goodbye 👋
                None
            },
        }
    }

}