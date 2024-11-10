/* endlessh-rs
 * 
 * an implementation of endlessh in rust
 */
use std::io::{Write, ErrorKind};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::process::exit;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};
use fastrand;
use clap::Parser;

const TCP_SERVER_TOKEN: Token = Token(0);
const LINE_BUFFER_SIZE: usize = 256;

#[derive(Parser,Clone,Debug)]
#[command(version, about, long_about = None)]
struct EndlesshArgs {
    #[arg(long, default_value_t=SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 2222)))]
    listen_address: SocketAddr,
    #[arg(long, default_value_t=32)]
    banner_line_length: usize,
    #[arg(long, default_value_t=4096)]
    max_clients: usize,
    #[arg(long, default_value_t=10_000)]
    message_delay_ms: u64,
}

impl EndlesshArgs {
    fn validate(self) -> EndlesshArgs {
        if self.banner_line_length >= LINE_BUFFER_SIZE {
            println!("banner line length can't be greater than or equal to {}, got {}", LINE_BUFFER_SIZE, self.banner_line_length);
            exit(1);
        }
        self
    }
}

struct ConnectedClient {
    stream: TcpStream,
    _address: SocketAddr,
    _connected_time: Instant,
    last_send_time: Option<Instant>,
    num_bytes_sent: usize,
}

// the SSH client will try to parse lines starting with "SSH-", ending the banner
// the "alphanumeric" distribution never generates '-' so we should be good
// see https://datatracker.ietf.org/doc/html/rfc4253#section-4.2
fn rand_line(buffer: &mut[u8]) {
    for element in buffer.iter_mut() {
        *element = fastrand::alphanumeric() as u8;
    }
}

fn try_accept_new_connections(
    tcp_accept_available: &mut bool,
    connected_clients: &mut VecDeque<ConnectedClient>,
    listener: &mut TcpListener,
    max_clients: &usize,
) {
    while *tcp_accept_available && connected_clients.len() < *max_clients {
        match listener.accept() {
            Ok((stream, address)) => {
                connected_clients.push_back(ConnectedClient {
                    stream,
                    _address: address,
                    _connected_time: Instant::now(),
                    last_send_time: None,
                    num_bytes_sent: 0,
                });
            },
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                *tcp_accept_available = false;
            }
            Err(e) => {
                panic!("failed to accept connection: {}", e);
            }
        };
    }
}

fn send_line(
    mut connected_client: ConnectedClient,
    to_send: &[u8],
    now: Instant,
) -> Option<ConnectedClient> {
    return match connected_client.stream.write(to_send) {
        Ok(0) => {
            // client disconnected, goodbye
            None
        },
        Ok(n) => {
            // send (at least partially) succeeded
            connected_client.num_bytes_sent += n;
            connected_client.last_send_time = Some(now);
            Some(connected_client)
        },
        Err(ref err) if err.kind() == ErrorKind::WouldBlock => {
            // couldn't send - oh well
            Some(connected_client)
        },
        Err(_e) => {
            // 🤷
            None
        },
    };
}

fn endlessh(args: &EndlesshArgs) {

    let mut line_buffer = [0_u8; LINE_BUFFER_SIZE];
    line_buffer[args.banner_line_length] = '\n' as u8;

    let mut poll = Poll::new().unwrap();
    let mut events = Events::with_capacity(128);
    let mut connected_clients: VecDeque<ConnectedClient> = VecDeque::with_capacity(args.max_clients);
    let mut current_timeout = None;
    let mut tcp_accept_available = false;
    
    let mut listener = TcpListener::bind(args.listen_address).unwrap();
    poll.registry().register(&mut listener, TCP_SERVER_TOKEN, Interest::READABLE).unwrap();

    println!("endlessh-rs listening on {}", args.listen_address);

    loop {
        if let Err(err) = poll.poll(&mut events, current_timeout) {
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            panic!("failed to poll: {}", err);
        }

        let loop_time = Instant::now();
        current_timeout = None;

        for event in events.iter() {
            match event.token() {
                TCP_SERVER_TOKEN => {
                    tcp_accept_available = true;
                    try_accept_new_connections(
                        &mut tcp_accept_available,
                        &mut connected_clients,
                        &mut listener,
                        &args.max_clients
                    )
                },
                rando_token => {
                    panic!("unexpected token {}", rando_token.0);
                },
            }
        }

        if !connected_clients.is_empty() {
            rand_line(&mut line_buffer[..args.banner_line_length]);
            while let Some(connected_client) = connected_clients.pop_front() {

                let send_or_wait: Option<Duration> = match connected_client.last_send_time {
                    None => {
                        // client has never received a line - send immediately
                        None
                    },
                    Some(last_send) => {
                        // client has received a line - send if the message window has elapsed 
                        (last_send + Duration::from_millis(args.message_delay_ms)).checked_duration_since(loop_time)
                    },
                };
                match send_or_wait {
                    None => {
                        match send_line(connected_client, &line_buffer[..args.banner_line_length+1], loop_time) {
                            Some(c) => connected_clients.push_back(c),
                            None => {
                                // drop the client
                            },
                        }
                    },
                    Some(need_to_wait) => {
                        current_timeout = Some(need_to_wait);
                        connected_clients.push_back(connected_client);
                        break;
                    }
                }
            }
        }

        // try to accept more connections in case we dropped from the max number of clients
        try_accept_new_connections(
            &mut tcp_accept_available,
            &mut connected_clients,
            &mut listener,
            &args.max_clients,
        );

    }
}

fn main() {
    endlessh(&EndlesshArgs::parse().validate());
}
