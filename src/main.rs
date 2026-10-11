/* endlessh-rs
 *
 * an implementation of endlessh in rust
 */

use std::io::ErrorKind::Interrupted;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;

use clap::Parser;
use mio::net::TcpListener;
use mio::{Events, Poll, Token};
use std::time::{Duration, Instant};

use endlessh::{EndlesshOptions, EndlesshServer, MioListener, NewLine};

#[cfg(unix)]
use {mio::net::UnixListener, std::fs::remove_file};

const SSH_SERVER_TOKEN: Token = Token(0);
#[cfg(feature = "metrics")]
use endlessh::metrics::MetricServer;
#[cfg(feature = "metrics")]
const METRIC_SERVER_TOKEN: Token = Token(1);
#[cfg(feature = "metrics")]
const METRIC_CLIENT_TOKEN_START: usize = 2;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum MultiListener {
    Ip(SocketAddr),
    Unix(PathBuf),
    Disabled,
}

impl From<&str> for MultiListener {
    fn from(v: &str) -> MultiListener {
        if v == "disabled" {
            MultiListener::Disabled
        } else if v.starts_with("ip:") {
            let to_parse: &str = &v[3..];
            match to_parse.parse::<SocketAddr>() {
                Ok(s) => MultiListener::Ip(s),
                Err(e) => panic!("bad ip address - {}", e),
            }
        } else if v.starts_with("unix:") {
            MultiListener::Unix(PathBuf::from(&v[5..]))
        } else {
            panic!("listener must be of the form \"disabled|ip:<socketaddr>|unix:<socketpath>\"")
        }
    }
}

impl std::fmt::Display for MultiListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        match &self {
            &MultiListener::Ip(i) => {
                write!(f, "ip:{}", i)
            }
            &MultiListener::Unix(p) => {
                write!(f, "unix:{}", p.display())
            }
            &MultiListener::Disabled => {
                write!(f, "disabled")
            }
        }?;
        Ok(())
    }
}

#[derive(Parser, Clone, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(
        long,
        default_value_t=MultiListener::Ip(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 2222))),
        help = "address to accept ssh connections on, in the form \"ip:<socketaddr>\" or \"unix:<socketpath>\""
    )]
    ssh_listen_address: MultiListener,
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
        default_value_t = MultiListener::Disabled,
        help = "address to serve openmetrics/prometheus stats on, in the form \"disabled\", \"ip:<socketaddr>\", or \"unix:<socketpath>\""
    )]
    metrics_listen_address: MultiListener,
    #[cfg(feature = "metrics")]
    #[arg(
        long,
        default_value_t = 3,
        help = "maximum number of concurrent metrics http clients to serve at once"
    )]
    metrics_max_clients: usize,
}

#[cfg(feature = "metrics")]
fn event_loop<SshListener: MioListener, MetricsListener: MioListener>(
    mut poll: Poll,
    mut events: Events,
    mut endlessh_server: EndlesshServer<SshListener>,
    mut metric_server: Option<MetricServer<MetricsListener>>,
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
                _ if metric_server.as_mut().is_some_and(|m| {
                    m.try_handle_event(event, &mut poll, endlessh_server.stats())
                }) => {}
                rando_token => {
                    panic!("unexpected token {}", rando_token.0);
                }
            }
        }
        timeout = endlessh_server.handle_wakeup(&loop_time);
    }
}

#[cfg(not(feature = "metrics"))]
fn event_loop<SshListener: MioListener>(
    mut poll: Poll,
    mut events: Events,
    mut endlessh_server: EndlesshServer<SshListener>,
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
                rando_token => {
                    panic!("unexpected token {}", rando_token.0);
                }
            }
        }
        timeout = endlessh_server.handle_wakeup(&loop_time);
    }
}

/// binds the metrics listener (if enabled) and hands off to [`event_loop`].
///
/// this can't be folded into `event_loop` directly: the metrics listener's transport
/// (TCP vs unix) is also chosen at runtime, so each transport needs its own call site
/// for the compiler to monomorphize `MetricServer<MetricsListener>` against.
#[cfg(feature = "metrics")]
fn run<Listener: MioListener>(
    poll: Poll,
    events: Events,
    endlessh_server: EndlesshServer<Listener>,
    args: &Args,
) {
    match &args.metrics_listen_address {
        MultiListener::Disabled => event_loop(
            poll,
            events,
            endlessh_server,
            None::<MetricServer<TcpListener>>,
        ),
        MultiListener::Ip(ip) => {
            let tcp_listener =
                TcpListener::bind(*ip).expect("failed to bind to metrics TCP socket");
            println!(
                "endlessh-rs listening for metrics connections on {}",
                args.metrics_listen_address
            );
            let metric_server = MetricServer::new(
                &poll,
                tcp_listener,
                METRIC_SERVER_TOKEN,
                METRIC_CLIENT_TOKEN_START..METRIC_CLIENT_TOKEN_START + args.metrics_max_clients,
            );
            event_loop(poll, events, endlessh_server, Some(metric_server));
        }
        #[cfg(unix)]
        MultiListener::Unix(path) => {
            let _ = remove_file(path);
            let unix_listener =
                UnixListener::bind(path).expect("failed to bind to metrics unix socket");
            println!(
                "endlessh-rs listening for metrics connections on {}",
                args.metrics_listen_address
            );
            let metric_server = MetricServer::new(
                &poll,
                unix_listener,
                METRIC_SERVER_TOKEN,
                METRIC_CLIENT_TOKEN_START..METRIC_CLIENT_TOKEN_START + args.metrics_max_clients,
            );
            event_loop(poll, events, endlessh_server, Some(metric_server));
        }
        #[cfg(not(unix))]
        MultiListener::Unix(_) => {
            panic!("unix sockets are not supported on this platform")
        }
    }
}

#[cfg(not(feature = "metrics"))]
fn run<Listener: MioListener>(
    poll: Poll,
    events: Events,
    endlessh_server: EndlesshServer<Listener>,
    _args: &Args,
) {
    event_loop(poll, events, endlessh_server);
}

fn main() {
    let args = &Args::parse();
    let poll = Poll::new().unwrap();
    let events = Events::with_capacity(128);

    let ssh_options = EndlesshOptions {
        banner_line_length: args.ssh_banner_line_length,
        max_clients: args.ssh_max_clients,
        message_delay: Duration::from_millis(args.ssh_message_delay_ms),
        newline: NewLine::LF,
    };

    match &args.ssh_listen_address {
        MultiListener::Ip(ip) => {
            let tcp_listener = TcpListener::bind(*ip).expect("failed to bind to ssh TCP socket");
            println!(
                "endlessh-rs listening for ssh connections on {}",
                args.ssh_listen_address
            );
            let endlessh_server =
                EndlesshServer::new(ssh_options, tcp_listener, SSH_SERVER_TOKEN, &poll);
            run(poll, events, endlessh_server, args);
        }
        #[cfg(unix)]
        MultiListener::Unix(path) => {
            let _ = remove_file(path);
            let unix_listener =
                UnixListener::bind(path).expect("failed to bind to ssh unix socket");
            println!(
                "endlessh-rs listening for ssh connections on {}",
                args.ssh_listen_address
            );
            let endlessh_server =
                EndlesshServer::new(ssh_options, unix_listener, SSH_SERVER_TOKEN, &poll);
            run(poll, events, endlessh_server, args);
        }
        #[cfg(not(unix))]
        MultiListener::Unix(_) => {
            panic!("unix sockets are not supported on this platform")
        }
        MultiListener::Disabled => {
            panic!("ssh listener cannot be disabled")
        }
    }
}
