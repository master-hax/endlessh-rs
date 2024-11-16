/* endlessh-rs
 * 
 * an implementation of endlessh in rust
 */

mod endlessh;

use std::io::ErrorKind::Interrupted;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;

use std::time::{Duration, Instant};
use mio::net::TcpListener;
use mio::{Events, Poll, Token};
use clap::Parser;

use endlessh::{EndlesshOptions, EndlesshServer};

#[cfg(unix)]
use {
    mio::net::{UnixListener,UnixStream},
    std::fs::remove_file,
};

#[cfg(feature = "metrics")]
mod metrics;
#[cfg(feature = "metrics")]
use metrics::MetricServer;
#[cfg(feature = "metrics")]
const METRIC_SERVER_TOKEN: Token = Token(1);
#[cfg(feature = "metrics")]
const METRIC_CLIENT_TOKEN_START: usize = 2;

#[cfg(feature = "metrics")]
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum MultiListener {
    Ip(SocketAddr),
    Unix(PathBuf),
    Disabled,
}

#[cfg(feature = "metrics")]
impl From<&str> for MultiListener {
    fn from(v: &str) -> MultiListener {
        if v == "disabled" {
            MultiListener::Disabled
        } else if v.starts_with("ip:") {
            let to_parse: &str = &v[3..];
            match to_parse.parse::<SocketAddr>() {
                Ok(s) =>  MultiListener::Ip(s),
                Err(e) => panic!("bad ip address - {}", e),
            }
        } else if v.starts_with("unix:") {
            MultiListener::Unix(PathBuf::from(&v[5..]))
        } else {
            panic!("listener must be of the form \"disabled|ip:<socketaddr>|unix:<socketpath>\"")
        }
    }
}

#[cfg(feature = "metrics")]
impl std::fmt::Display for MultiListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        match &self {
            &MultiListener::Ip(i) => {
                write!(f, "ip:{}", i)
            },
            &MultiListener::Unix(p) => { 
                write!(f, "unix:{}", p.display())
            },
            &MultiListener::Disabled => {
                write!(f, "disabled")
            },
        }?;
        Ok(())
    }
}

const SSH_SERVER_TOKEN: Token = Token(0);

#[derive(Parser,Clone,Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(long, default_value_t=SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 2222)))]
    ssh_listen_address: SocketAddr,
    #[arg(long, default_value_t=32)]
    ssh_banner_line_length: usize,
    #[arg(long, default_value_t=4096)]
    ssh_max_clients: usize,
    #[arg(long, default_value_t=10_000)]
    ssh_message_delay_ms: u64,
    #[cfg(feature = "metrics")]
    #[arg(long, default_value_t=MultiListener::Disabled)]
    metrics_listen_address: MultiListener,
    #[cfg(feature = "metrics")]
    #[arg(long, default_value_t=3)]
    metrics_max_clients: usize,
}

fn event_loop(
    mut poll: Poll,
    mut events: Events,
    mut endlessh_server: EndlesshServer, 
    #[cfg(feature = "metrics")]
    mut metric_server: Option<MetricServer>,
) {
    let mut timeout = None;
    let mut loop_time = Instant::now();
    loop {
    
        if let Err(err) = poll.poll(&mut events, timeout) {
            if err.kind() == Interrupted {
                continue;
            }
            panic!("failed to poll: {}", err);
        }
        loop_time = Instant::now();
        for event in events.iter() {
            match event.token() {
                _ if endlessh_server.try_handle_event(event, &loop_time) => {},
                #[cfg(feature = "metrics")]
                _ if metric_server.as_mut().is_some_and(|m| m.try_handle_event(event, &mut poll, endlessh_server.stats())) => {},
                rando_token => {
                    panic!("unexpected token {}", rando_token.0);
                },

            }
        }
        timeout = endlessh_server.handle_wakeup(&loop_time);
    }
}
 

fn main() {
    let args = &Args::parse();
    let poll = Poll::new().unwrap();
    let events = Events::with_capacity(128);

    let ssh_listener: TcpListener = TcpListener::bind(args.ssh_listen_address).expect("failed to bind to ssh socket");

    let endlessh_server = EndlesshServer::create(
        EndlesshOptions {
            banner_line_length: args.ssh_banner_line_length,
            max_clients: args.ssh_max_clients,
            message_delay: Duration::from_millis(args.ssh_message_delay_ms),
            newline: endlessh::NewLine::LF,
        },
        ssh_listener,
        SSH_SERVER_TOKEN,
        &poll
    );

    println!("endlessh-rs listening for ssh connections on ip:{}", args.ssh_listen_address);

    #[cfg(feature = "metrics")]
    let metric_server: Option<MetricServer> = match &args.metrics_listen_address {
        MultiListener::Disabled => None,
        MultiListener::Ip(ip) => {
            let tcp_listener = TcpListener::bind(*ip).expect("failed to bind to TCP socket");
            println!("endlessh-rs listening for metrics connections on {}", args.metrics_listen_address);
            Some(MetricServer::new_tcp(&poll, tcp_listener, METRIC_SERVER_TOKEN, METRIC_CLIENT_TOKEN_START..METRIC_CLIENT_TOKEN_START+args.metrics_max_clients))
        },
        #[cfg(unix)]
        MultiListener::Unix(path) => {
            let _ = remove_file(path);
            let unix_listener = UnixListener::bind(path).expect("failed to bind to unix socket");
            println!("endlessh-rs listening for metrics connections on {}", args.metrics_listen_address);
            Some(MetricServer::new_unix(&poll, unix_listener, METRIC_SERVER_TOKEN, METRIC_CLIENT_TOKEN_START..METRIC_CLIENT_TOKEN_START+args.metrics_max_clients))
        },
        #[cfg(not(unix))]
        MultiListener::Unix(_) => {
            panic!("unix sockets are not supported on this platform")
        },
    };

    event_loop(
        poll,
        events,
        endlessh_server,
        #[cfg(feature = "metrics")]
        metric_server
    );

}
