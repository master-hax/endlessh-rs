//! Black-box connection limit tests using an isolated endlessh-rs process.
//! Run with: cargo test --locked --no-default-features

use std::io::{BufRead, BufReader, Read};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const BURST: usize = 100;
const PER_IP_LIMIT: usize = 16;
const GLOBAL_LIMIT: usize = 8;
const OBSERVATION: Duration = Duration::from_secs(2);

struct TestServer {
    child: Child,
    messages: Receiver<String>,
    port: u16,
}

impl TestServer {
    fn start(global_limit: usize, per_ip_limit: usize) -> Self {
        // Reserve an unused port briefly; the child binds it on both loopback families.
        let reserved = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = reserved.local_addr().unwrap().port();
        drop(reserved);

        let mut child = Command::new(env!("CARGO_BIN_EXE_endlessh-rs"))
            .arg("--ssh-listen-address")
            .arg(format!("ip:127.0.0.1:{port}"))
            .arg("--ssh-listen-address")
            .arg(format!("ip:[::1]:{port}"))
            .arg("--ssh-max-clients")
            .arg(global_limit.to_string())
            .arg("--ssh-max-clients-per-ip")
            .arg(per_ip_limit.to_string())
            .arg("--ssh-message-delay-ms")
            .arg("10000")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to start endlessh-rs test process");

        let stderr = child.stderr.take().unwrap();
        let (sender, messages) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                match line {
                    Ok(line) => {
                        if sender.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let mut server = Self {
            child,
            messages,
            port,
        };
        server.wait_for_listeners();
        server
    }

    fn wait_for_listeners(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ipv4_ready = false;
        let mut ipv6_ready = false;
        let mut output = Vec::new();

        while !(ipv4_ready && ipv6_ready) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for dual-stack listeners: {output:?}"
            );
            match self.messages.recv_timeout(remaining) {
                Ok(line) => {
                    if line.contains(&format!("ip:127.0.0.1:{}", self.port)) {
                        ipv4_ready = true;
                    }
                    if line.contains(&format!("ip:[::1]:{}", self.port)) {
                        ipv6_ready = true;
                    }
                    output.push(line);
                }
                Err(error) => panic!("server did not start both listeners ({error}): {output:?}"),
            }
        }
        self.assert_running();
    }

    fn assert_running(&mut self) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "endlessh-rs exited during the test"
        );
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Clients {
    streams: Vec<TcpStream>,
    connect_errors: usize,
}

fn connect_many(address: SocketAddr, count: usize) -> Clients {
    let mut streams = Vec::with_capacity(count);
    let mut connect_errors = 0;
    for _ in 0..count {
        match TcpStream::connect_timeout(&address, Duration::from_millis(500)) {
            Ok(stream) => {
                stream.set_nonblocking(true).unwrap();
                streams.push(stream);
            }
            Err(_) => connect_errors += 1,
        }
    }
    Clients {
        streams,
        connect_errors,
    }
}

#[derive(Default, Debug)]
struct Results {
    greeted: usize,
    closed: usize,
    pending: usize,
}

fn classify(clients: &mut Clients) -> Results {
    // A completed TCP handshake does not imply that the server accepted the
    // connection. Only an actual banner byte counts as a trapped client.
    let mut states = vec![0_u8; clients.streams.len()];
    let deadline = Instant::now() + OBSERVATION;
    loop {
        for (state, stream) in states.iter_mut().zip(&mut clients.streams) {
            if *state != 0 {
                continue;
            }
            let mut byte = [0_u8; 1];
            match stream.read(&mut byte) {
                Ok(0) => *state = 2, // Closed by server.
                Ok(_) => *state = 1, // Received an SSH pre-banner byte.
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => *state = 2,
            }
        }
        if states.iter().all(|state| *state != 0) || Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    Results {
        greeted: states.iter().filter(|&&state| state == 1).count(),
        closed: states.iter().filter(|&&state| state == 2).count(),
        pending: states.iter().filter(|&&state| state == 0).count(),
    }
}

fn ipv4(port: u16) -> SocketAddr {
    format!("127.0.0.1:{port}").parse().unwrap()
}

fn ipv6(port: u16) -> SocketAddr {
    format!("[::1]:{port}").parse().unwrap()
}

#[test]
fn per_ip_limit_applies_independently_to_ipv4_and_ipv6() {
    let mut server = TestServer::start(64, PER_IP_LIMIT);
    let mut v4 = connect_many(ipv4(server.port), BURST);
    let mut v6 = connect_many(ipv6(server.port), BURST);
    let result4 = classify(&mut v4);
    let result6 = classify(&mut v6);

    println!(
        "IPv4: {result4:?}, connection errors: {}",
        v4.connect_errors
    );
    println!(
        "IPv6: {result6:?}, connection errors: {}",
        v6.connect_errors
    );
    assert_eq!(result4.greeted, PER_IP_LIMIT, "IPv4 per-IP quota failed");
    assert_eq!(result6.greeted, PER_IP_LIMIT, "IPv6 per-IP quota failed");
    server.assert_running();
}

#[test]
fn ipv4_and_ipv6_share_the_global_limit() {
    let mut server = TestServer::start(GLOBAL_LIMIT, PER_IP_LIMIT);
    // Keep an existing connection alive while adding more IPv4 clients.
    // This catches starvation when the first client is waiting for its next line.
    let mut first_v4 = connect_many(ipv4(server.port), 1);
    assert_eq!(first_v4.connect_errors, 0);
    assert_eq!(classify(&mut first_v4).greeted, 1);

    let mut other_v4 = connect_many(ipv4(server.port), 5);
    let result4 = classify(&mut other_v4);
    assert_eq!(other_v4.connect_errors, 0);
    assert_eq!(
        result4.greeted, 5,
        "new IPv4 clients must be served promptly"
    );

    let mut v6 = connect_many(ipv6(server.port), 6);
    let result6 = classify(&mut v6);
    println!(
        "IPv4: greeted={}, closed={}, pending={}; IPv6: greeted={}, closed={}, pending={}",
        result4.greeted,
        result4.closed,
        result4.pending,
        result6.greeted,
        result6.closed,
        result6.pending
    );
    assert_eq!(v6.connect_errors, 0, "could not connect six IPv6 clients");
    assert_eq!(result6.greeted, 2, "only two global slots should remain");
    assert_eq!(1 + result4.greeted + result6.greeted, GLOBAL_LIMIT);
    server.assert_running();
}
