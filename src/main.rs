/* endlessh-rs
 *
 * an implementation of endlessh in rust
 */

use std::io::ErrorKind::Interrupted;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;

use clap::Parser;
use mio::net::TcpListener;
use mio::{event, Events, Interest, Poll, Token};
use std::time::{Duration, Instant};

use endlessh::{AnyListener, EndlesshOptions, EndlesshServer, NewLine};

#[cfg(unix)]
use endlessh::EndlesshStats;

#[cfg(unix)]
use {mio::net::UnixListener, std::fs::remove_file};

#[cfg(unix)]
use mio_signals::{Signal, Signals};

#[cfg(feature = "metrics")]
use endlessh::metrics::MetricServer;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum ListenerAddress {
    Ip(SocketAddr),
    Unix(PathBuf),
    Disabled,
}

impl From<&str> for ListenerAddress {
    fn from(v: &str) -> ListenerAddress {
        if v == "disabled" {
            ListenerAddress::Disabled
        } else if v.starts_with("ip:") {
            let to_parse: &str = &v[3..];
            match to_parse.parse::<SocketAddr>() {
                Ok(s) => ListenerAddress::Ip(s),
                Err(e) => panic!("bad ip address - {}", e),
            }
        } else if v.starts_with("unix:") {
            ListenerAddress::Unix(PathBuf::from(&v[5..]))
        } else {
            panic!("listener must be of the form \"disabled|ip:<socketaddr>|unix:<socketpath>\"")
        }
    }
}

impl std::fmt::Display for ListenerAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        match &self {
            &ListenerAddress::Ip(i) => {
                write!(f, "ip:{}", i)
            }
            &ListenerAddress::Unix(p) => {
                write!(f, "unix:{}", p.display())
            }
            &ListenerAddress::Disabled => {
                write!(f, "disabled")
            }
        }?;
        Ok(())
    }
}

impl ListenerAddress {
    fn bind(&self, purpose: &str) -> Option<AnyListener> {
        let listener = match self {
            ListenerAddress::Disabled => return None,
            ListenerAddress::Ip(ip) => AnyListener::from(
                TcpListener::bind(*ip)
                    .unwrap_or_else(|e| panic!("failed to bind {purpose} socket: {e}")),
            ),
            #[cfg(unix)]
            ListenerAddress::Unix(path) => {
                let _ = remove_file(path);
                AnyListener::from(
                    UnixListener::bind(path)
                        .unwrap_or_else(|e| panic!("failed to bind {purpose} socket: {e}")),
                )
            }
            #[cfg(not(unix))]
            ListenerAddress::Unix(_) => panic!("unix sockets are not supported on this platform"),
        };
        println!("endlessh-rs listening for {purpose} connections on {self}");
        Some(listener)
    }
}

#[derive(Parser, Clone, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(
        long,
        default_value_t=ListenerAddress::Ip(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 2222))),
        help = "address to accept ssh connections on, in the form \"ip:<socketaddr>\" or \"unix:<socketpath>\""
    )]
    ssh_listen_address: ListenerAddress,
    #[arg(
        long,
        default_value_t = 32,
        help = "length, in bytes, of each randomly generated banner line sent to ssh clients"
    )]
    ssh_banner_line_length: usize,
    #[arg(
        long,
        default_value_t = 4096,
        help = "maximum number of ssh clients to trap at once"
    )]
    ssh_max_clients: usize,
    #[arg(
        long,
        default_value_t = 10_000,
        help = "delay, in milliseconds, between banner lines sent to each trapped ssh client"
    )]
    ssh_message_delay_ms: u64,
    #[cfg(feature = "metrics")]
    #[arg(
        long,
        default_value_t = ListenerAddress::Disabled,
        help = "address to serve openmetrics/prometheus stats on, in the form \"disabled\", \"ip:<socketaddr>\", or \"unix:<socketpath>\""
    )]
    metrics_listen_address: ListenerAddress,
    #[cfg(feature = "metrics")]
    #[arg(
        long,
        default_value_t = 3,
        help = "maximum number of concurrent metrics http clients to serve at once"
    )]
    metrics_max_clients: usize,
}

#[cfg(unix)]
struct SignalHandler {
    token: Token,
    signals: Signals,
    should_terminate: bool,
}

#[cfg(unix)]
impl SignalHandler {
    fn new(poll: &Poll, token: Token) -> Self {
        let mut signals = Signals::new(Signal::Terminate | Signal::Interrupt | Signal::User1)
            .expect("failed to create signal handler");
        poll.registry()
            .register(&mut signals, token, Interest::READABLE)
            .expect("failed to register signal handler");
        SignalHandler {
            token,
            signals,
            should_terminate: false,
        }
    }

    fn should_terminate(&self) -> bool {
        self.should_terminate
    }

    fn try_handle_event(&mut self, event: &event::Event, stats: &EndlesshStats) -> bool {
        if event.token() != self.token {
            return false;
        }
        loop {
            match self.signals.receive() {
                Ok(Some(Signal::Terminate)) => {
                    println!("endlessh-rs: received SIGTERM, shutting down\n{}", stats);
                    self.should_terminate = true;
                    return true;
                }
                Ok(Some(Signal::Interrupt)) => {
                    println!("endlessh-rs: received SIGINT, shutting down\n{}", stats);
                    self.should_terminate = true;
                    return true;
                }
                Ok(Some(Signal::User1)) => println!("{}", stats),
                Ok(Some(_)) => {}
                Ok(None) => return true,
                Err(e) => {
                    eprintln!("failed to read pending signal: {}", e);
                    return true;
                }
            }
        }
    }
}

fn next_token(counter: &mut usize) -> Token {
    let token = Token(*counter);
    *counter += 1;
    token
}

fn event_loop(
    mut poll: Poll,
    mut events: Events,
    mut endlessh_server: EndlesshServer<AnyListener>,
    #[cfg(feature = "metrics")] mut metric_server: Option<MetricServer<AnyListener>>,
    #[cfg(unix)] mut signal_handler: SignalHandler,
) {
    let mut timeout = None;
    loop {
        if let Err(err) = poll.poll(&mut events, timeout) {
            if err.kind() != Interrupted {
                eprintln!("failed to poll: {}", err);
            }
            continue;
        }
        let loop_time = Instant::now();
        for event in events.iter() {
            match event.token() {
                _ if endlessh_server.try_handle_event(event, &loop_time) => {}
                #[cfg(feature = "metrics")]
                _ if metric_server.as_mut().is_some_and(|m| {
                    m.try_handle_event(event, &mut poll, endlessh_server.stats())
                }) => {}
                #[cfg(unix)]
                _ if signal_handler.try_handle_event(event, endlessh_server.stats()) => {
                    if signal_handler.should_terminate() {
                        return;
                    }
                }
                rando_token => {
                    panic!("unexpected token {}", rando_token.0);
                }
            }
        }
        timeout = endlessh_server.handle_wakeup(&loop_time);
    }
}

fn main() {
    let args = &Args::parse();
    let poll = Poll::new().unwrap();
    let events = Events::with_capacity(128);

    let mut token_counter = 0usize;
    let ssh_server_token = next_token(&mut token_counter);

    #[cfg(unix)]
    let signal_handler = SignalHandler::new(&poll, next_token(&mut token_counter));

    #[cfg(feature = "metrics")]
    let metric_server_token = next_token(&mut token_counter);
    #[cfg(feature = "metrics")]
    let metric_client_token_start = next_token(&mut token_counter).0;

    let ssh_options = EndlesshOptions {
        banner_line_length: args.ssh_banner_line_length,
        max_clients: args.ssh_max_clients,
        message_delay: Duration::from_millis(args.ssh_message_delay_ms),
        newline: NewLine::LF,
    };

    let ssh_listener = args
        .ssh_listen_address
        .bind("ssh")
        .expect("ssh listener cannot be disabled");
    let endlessh_server = EndlesshServer::new(ssh_options, ssh_listener, ssh_server_token, &poll);

    #[cfg(feature = "metrics")]
    let metric_server = args.metrics_listen_address.bind("metrics").map(|listener| {
        MetricServer::new(
            &poll,
            listener,
            metric_server_token,
            metric_client_token_start..metric_client_token_start + args.metrics_max_clients,
        )
    });

    event_loop(
        poll,
        events,
        endlessh_server,
        #[cfg(feature = "metrics")]
        metric_server,
        #[cfg(unix)]
        signal_handler,
    );
}
