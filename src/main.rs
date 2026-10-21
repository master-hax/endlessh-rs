/* endlessh-rs
 *
 * an implementation of endlessh in rust
 */

use std::borrow::Cow;
use std::io::ErrorKind::Interrupted;
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use mio::net::TcpListener;
#[cfg(unix)]
use mio::{event, Interest};
use mio::{Events, Poll, Token};
use socket2::{Domain, Protocol, Socket, Type};
use std::time::{Duration, Instant};

use endlessh::{
    AnyListener, EndlesshFatalError, EndlesshOptions, EndlesshServer, NewLine, Verbosity,
};

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

impl std::str::FromStr for ListenerAddress {
    type Err = String;
    fn from_str(v: &str) -> Result<ListenerAddress, String> {
        if v == "disabled" {
            Ok(ListenerAddress::Disabled)
        } else if let Some(addr) = v.strip_prefix("ip:") {
            addr.parse::<SocketAddr>()
                .map(ListenerAddress::Ip)
                .map_err(|e| format!("bad ip address - {}", e))
        } else if let Some(path) = v.strip_prefix("unix:") {
            Ok(ListenerAddress::Unix(PathBuf::from(path)))
        } else {
            Err(
                "listener must be of the form \"disabled|ip:<socketaddr>|unix:<socketpath>\""
                    .to_string(),
            )
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

fn bind_error(purpose: &str, e: std::io::Error) -> EndlesshFatalError {
    std::io::Error::new(e.kind(), format!("failed to bind {purpose} socket: {e}")).into()
}

impl ListenerAddress {
    fn bind(&self, purpose: &str) -> Result<Option<AnyListener>, EndlesshFatalError> {
        let listener = match self {
            ListenerAddress::Disabled => return Ok(None),
            ListenerAddress::Ip(ip) => {
                // Explicit IPv6 listeners remain IPv6-only, allowing a
                // separate IPv4 listener to bind the same port.
                bind_ip_listener(*ip, true).map_err(|e| bind_error(purpose, e))?
            }
            #[cfg(unix)]
            ListenerAddress::Unix(path) => {
                let _ = remove_file(path);
                AnyListener::from(UnixListener::bind(path).map_err(|e| bind_error(purpose, e))?)
            }
            #[cfg(not(unix))]
            ListenerAddress::Unix(_) => {
                return Err(EndlesshFatalError::InvalidOptions(Cow::Borrowed(
                    "unix sockets are not supported on this platform",
                )))
            }
        };
        eprintln!("endlessh-rs listening for {purpose} connections on {self}");
        Ok(Some(listener))
    }
}

#[derive(Parser, Clone, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(
        long = "ssh-listen-address",
        help = "Repeatable SSH listen address (ip:<socketaddr> or unix:<socketpath>); default: dual-stack [::]:2222"
    )]
    ssh_listen_addresses: Vec<ListenerAddress>,
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
    #[arg(
        short,
        long,
        action = clap::ArgAction::Count,
        help = "increase logging verbosity (-v logs each connection, -vv for debug)"
    )]
    verbose: u8,
}

#[cfg(unix)]
struct SignalHandler {
    token: Token,
    signals: Signals,
    should_terminate: bool,
}

#[cfg(unix)]
impl SignalHandler {
    fn new(poll: &Poll, token: Token) -> Result<Self, EndlesshFatalError> {
        let mut signals = Signals::new(Signal::Terminate | Signal::Interrupt | Signal::User1)?;
        poll.registry()
            .register(&mut signals, token, Interest::READABLE)?;
        Ok(SignalHandler {
            token,
            signals,
            should_terminate: false,
        })
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
                    eprintln!("endlessh-rs: received SIGTERM, shutting down\n{}", stats);
                    self.should_terminate = true;
                    return true;
                }
                Ok(Some(Signal::Interrupt)) => {
                    eprintln!("endlessh-rs: received SIGINT, shutting down\n{}", stats);
                    self.should_terminate = true;
                    return true;
                }
                Ok(Some(Signal::User1)) => eprintln!("{}", stats),
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

fn run(args: &Args) -> Result<(), EndlesshFatalError> {
    let poll = Poll::new()?;
    let events = Events::with_capacity(128);

    let mut token_counter = 0usize;
    let mut ssh_listeners = Vec::with_capacity(args.ssh_listen_addresses.len().max(1));
    if args.ssh_listen_addresses.is_empty() {
        // Default to one IPv6 socket accepting IPv4-mapped and native IPv6.
        // Retain IPv4-only operation when IPv6 is unavailable.
        let address: SocketAddr = "[::]:2222".parse().unwrap();
        let listener = match bind_ip_listener(address, false) {
            Ok(listener) => listener,
            Err(error) if ipv6_unavailable(&error) => {
                eprintln!("IPv6 unavailable ({error}); trying IPv4-only");
                bind_ip_listener("0.0.0.0:2222".parse().unwrap(), true)
                    .map_err(|error| bind_error("ssh", error))?
            }
            Err(error) => return Err(bind_error("ssh", error)),
        };
        ssh_listeners.push((listener, next_token(&mut token_counter)));
    } else {
        for address in &args.ssh_listen_addresses {
            let listener = address.bind("ssh")?.ok_or_else(|| {
                EndlesshFatalError::InvalidOptions(Cow::Borrowed("ssh listener cannot be disabled"))
            })?;
            ssh_listeners.push((listener, next_token(&mut token_counter)));
        }
    }

    #[cfg(unix)]
    let signal_handler = SignalHandler::new(&poll, next_token(&mut token_counter))?;

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

    let endlessh_server = EndlesshServer::with_listeners(
        ssh_options,
        ssh_listeners,
        &poll,
        Verbosity::from(args.verbose),
    )?;

    #[cfg(feature = "metrics")]
    let metric_server = args
        .metrics_listen_address
        .bind("metrics")?
        .map(|listener| {
            MetricServer::new(
                &poll,
                listener,
                metric_server_token,
                metric_client_token_start..metric_client_token_start + args.metrics_max_clients,
            )
        })
        .transpose()?;

    event_loop(
        poll,
        events,
        endlessh_server,
        #[cfg(feature = "metrics")]
        metric_server,
        #[cfg(unix)]
        signal_handler,
    );
    Ok(())
}

fn main() {
    let args = &Args::parse();
    if let Err(e) = run(args) {
        eprintln!("endlessh-rs: {}", e);
        std::process::exit(1);
    }
}

fn bind_ip_listener(ip: SocketAddr, ipv6_only: bool) -> std::io::Result<AnyListener> {
    let domain = if ip.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    if ip.is_ipv6() {
        socket.set_only_v6(ipv6_only)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&ip.into())?;
    socket.listen(128)?;
    let std_listener: std::net::TcpListener = socket.into();
    Ok(AnyListener::from(TcpListener::from_std(std_listener)))
}

// Only use the IPv4 fallback when IPv6 or dual-stack sockets are unavailable.
// The raw OS codes cover unsupported address families, protocols and socket options.
fn ipv6_unavailable(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;

    if matches!(
        error.kind(),
        ErrorKind::Unsupported | ErrorKind::AddrNotAvailable
    ) {
        return true;
    }

    match error.raw_os_error() {
        #[cfg(target_os = "linux")]
        Some(97 | 93 | 92) => true, // EAFNOSUPPORT, EPROTONOSUPPORT, ENOPROTOOPT
        #[cfg(target_os = "windows")]
        Some(10047 | 10043 | 10042) => true, // WSAEAFNOSUPPORT, WSAEPROTONOSUPPORT, WSAENOPROTOOPT
        #[cfg(any(
            target_os = "macos",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        ))]
        Some(47 | 43 | 42) => true, // EAFNOSUPPORT, EPROTONOSUPPORT, ENOPROTOOPT
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_only_for_unavailable_ipv6() {
        use std::io::{Error, ErrorKind};

        assert!(ipv6_unavailable(&Error::from(ErrorKind::Unsupported)));
        assert!(ipv6_unavailable(&Error::from(ErrorKind::AddrNotAvailable)));
        assert!(!ipv6_unavailable(&Error::from(ErrorKind::AddrInUse)));
        assert!(!ipv6_unavailable(&Error::from(ErrorKind::PermissionDenied)));

        #[cfg(target_os = "linux")]
        assert!(ipv6_unavailable(&Error::from_raw_os_error(97)));
        #[cfg(target_os = "windows")]
        assert!(ipv6_unavailable(&Error::from_raw_os_error(10047)));
    }

    #[test]
    fn default_uses_implicit_dual_stack() {
        let args = Args::try_parse_from(["endlessh-rs"]).unwrap();
        assert!(args.ssh_listen_addresses.is_empty());
    }

    #[test]
    fn explicit_listeners_are_repeatable() {
        let args = Args::try_parse_from([
            "endlessh-rs",
            "--ssh-listen-address",
            "ip:127.0.0.1:2222",
            "--ssh-listen-address",
            "ip:[::1]:2222",
        ])
        .unwrap();
        assert_eq!(args.ssh_listen_addresses.len(), 2);
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn one_dual_stack_socket_accepts_ipv4_and_ipv6() {
        let listener = bind_ip_listener("[::]:0".parse().unwrap(), false).unwrap();
        let port = match &listener {
            AnyListener::Tcp(listener) => listener.local_addr().unwrap().port(),
            #[cfg(unix)]
            _ => panic!("expected TCP listener"),
        };
        let _ipv4 = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let _ipv6 = std::net::TcpStream::connect(("::1", port)).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ipv4_and_ipv6_wildcards_can_share_a_port() {
        let v4 = ListenerAddress::Ip("0.0.0.0:0".parse().unwrap())
            .bind("test")
            .unwrap()
            .unwrap();
        let port = match &v4 {
            AnyListener::Tcp(listener) => listener.local_addr().unwrap().port(),
            _ => panic!("expected TCP listener"),
        };
        let v6_addr = format!("[::]:{port}").parse().unwrap();
        let _v6 = ListenerAddress::Ip(v6_addr).bind("test").unwrap().unwrap();
    }
}
