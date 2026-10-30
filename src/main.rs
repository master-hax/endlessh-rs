/* endlessh-rs
 *
 * an implementation of endlessh in rust
 */

use std::borrow::Cow;
use std::io::ErrorKind::Interrupted;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;

use clap::Parser;
use mio::net::TcpListener;
use mio::{Events, Poll, Token};
use std::time::{Duration, Instant};

use endlessh::{
    AnyListener, EndlesshFatalError, EndlesshOptions, EndlesshServer, NewLine, Verbosity,
};

#[cfg(any(feature = "metrics", unix))]
use endlessh::EndlesshStats;

#[cfg(unix)]
use {mio::net::UnixListener, std::fs::remove_file};

#[cfg(unix)]
use endlessh::signals::SignalHandler;

#[cfg(feature = "metrics")]
use endlessh::metrics::{MetricServer, MetricsStats};

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
        match self {
            ListenerAddress::Ip(i) => {
                write!(f, "ip:{}", i)
            }
            ListenerAddress::Unix(p) => {
                write!(f, "unix:{}", p.display())
            }
            ListenerAddress::Disabled => {
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
                AnyListener::from(TcpListener::bind(*ip).map_err(|e| bind_error(purpose, e))?)
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
        long,
        default_value_t=ListenerAddress::Ip(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 2222))),
        help = "address to accept ssh connections on, in the form \"disabled\", \"ip:<socketaddr>\", or \"unix:<socketpath>\""
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
    #[arg(
        short,
        long,
        action = clap::ArgAction::Count,
        help = "increase logging verbosity (-v logs each connection, -vv for debug)"
    )]
    verbose: u8,
}

fn next_token(counter: &mut usize) -> Token {
    let token = Token(*counter);
    *counter += 1;
    token
}

// process-level stats combined with ssh stats for the metrics endpoint and signal dumps
#[cfg(any(feature = "metrics", unix))]
struct MetricsSnapshot<'a> {
    uptime: Duration,
    ssh: &'a EndlesshStats,
    #[cfg(feature = "metrics")]
    metrics: MetricsStats,
}

#[cfg(feature = "metrics")]
impl std::fmt::Display for MetricsSnapshot<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "endlessh_uptime_seconds: {}\n{}{}",
            self.uptime.as_secs(),
            self.ssh,
            self.metrics
        )
    }
}

#[cfg(all(not(feature = "metrics"), unix))]
impl std::fmt::Display for MetricsSnapshot<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "endlessh_uptime_seconds: {}\n{}",
            self.uptime.as_secs(),
            self.ssh
        )
    }
}

fn event_loop(
    mut poll: Poll,
    mut events: Events,
    mut endlessh_server: Option<EndlesshServer<AnyListener>>,
    #[cfg(feature = "metrics")] mut metric_server: Option<MetricServer<AnyListener>>,
    #[cfg(unix)] mut signal_handler: SignalHandler,
    #[cfg(any(feature = "metrics", unix))] start_time: Instant,
) {
    // reported as ssh stats when the ssh listener is disabled
    #[cfg(any(feature = "metrics", unix))]
    let disabled_ssh_stats = EndlesshStats::default();

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
                _ if endlessh_server
                    .as_mut()
                    .is_some_and(|s| s.try_handle_event(event, &loop_time)) => {}
                #[cfg(feature = "metrics")]
                _ if metric_server.as_mut().is_some_and(|m| {
                    let snapshot = MetricsSnapshot {
                        uptime: loop_time.duration_since(start_time),
                        ssh: endlessh_server
                            .as_mut()
                            .map(|s| s.stats())
                            .unwrap_or(&disabled_ssh_stats),
                        metrics: m.stats(),
                    };
                    m.try_handle_event(event, &mut poll, &snapshot)
                }) => {}
                #[cfg(unix)]
                _ if {
                    let snapshot = MetricsSnapshot {
                        uptime: loop_time.duration_since(start_time),
                        ssh: endlessh_server
                            .as_mut()
                            .map(|s| s.stats())
                            .unwrap_or(&disabled_ssh_stats),
                        #[cfg(feature = "metrics")]
                        metrics: metric_server
                            .as_mut()
                            .map(|m| m.stats())
                            .unwrap_or_default(),
                    };
                    signal_handler.try_handle_event(event, &snapshot)
                } =>
                {
                    if signal_handler.should_terminate() {
                        return;
                    }
                }
                rando_token => {
                    panic!("unexpected token {}", rando_token.0);
                }
            }
        }
        timeout = endlessh_server
            .as_mut()
            .and_then(|s| s.handle_wakeup(&loop_time));
    }
}

fn run(args: &Args) -> Result<(), EndlesshFatalError> {
    #[cfg(any(feature = "metrics", unix))]
    let start_time = Instant::now();
    let poll = Poll::new()?;
    let events = Events::with_capacity(128);

    let mut token_counter = 0usize;
    let ssh_server_token = next_token(&mut token_counter);

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

    let endlessh_server = args
        .ssh_listen_address
        .bind("ssh")?
        .map(|listener| {
            EndlesshServer::new(
                ssh_options,
                listener,
                ssh_server_token,
                &poll,
                Verbosity::from(args.verbose),
            )
        })
        .transpose()?;

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

    #[cfg(feature = "metrics")]
    if endlessh_server.is_none() && metric_server.is_none() {
        return Err(EndlesshFatalError::InvalidOptions(Cow::Borrowed(
            "at least one of the ssh or metrics listener must be enabled",
        )));
    }
    #[cfg(not(feature = "metrics"))]
    if endlessh_server.is_none() {
        return Err(EndlesshFatalError::InvalidOptions(Cow::Borrowed(
            "ssh listener cannot be disabled",
        )));
    }

    event_loop(
        poll,
        events,
        endlessh_server,
        #[cfg(feature = "metrics")]
        metric_server,
        #[cfg(unix)]
        signal_handler,
        #[cfg(any(feature = "metrics", unix))]
        start_time,
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
