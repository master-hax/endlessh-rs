# endlessh-rs

An implementation of [endlessh](https://nullprogram.com/blog/2019/03/22/) 
in Rust, inspired by [endlessh](https://github.com/skeeto/endlessh) 
and [endlessh-go](https://github.com/shizunge/endlessh-go).

Features:

- Works on platforms supported by [mio](https://github.com/tokio-rs/mio), 
including Windows
- Single-threaded, with minimal allocation
- Multiple TCP and/or Unix socket listeners, with IPv4 and IPv6 in one event loop
- One shared SSH connection limit across all listeners and a per-source-IP limit
- Normalization of IPv4-mapped IPv6 addresses for per-IP accounting
- Optional OpenMetrics/Prometheus HTTP server (in the same thread)

## Usage

```text
Usage: endlessh-rs.exe [OPTIONS]

Options:
      --ssh-listen-address <SSH_LISTEN_ADDRESS>
          repeatable address (ip:<socketaddr> or unix:<socketpath>)
          [default: ip:0.0.0.0:2222 ip:[::]:2222]
      --ssh-banner-line-length <SSH_BANNER_LINE_LENGTH>
          length, in bytes, of each randomly generated banner line sent to ssh clients [default: 32]
      --ssh-max-clients <SSH_MAX_CLIENTS>
          maximum number of ssh clients across all listeners [default: 4096]
      --ssh-max-clients-per-ip <SSH_MAX_CLIENTS_PER_IP>
          maximum number of concurrent clients from a single source IP [default: 16]
      --ssh-message-delay-ms <SSH_MESSAGE_DELAY_MS>
          delay, in milliseconds, between banner lines sent to each trapped ssh client [default: 10000]
      --metrics-listen-address <METRICS_LISTEN_ADDRESS>
          address to serve openmetrics/prometheus stats on, in the form "disabled", "ip:<socketaddr>", or "unix:<socketpath>" [default: disabled]
      --metrics-max-clients <METRICS_MAX_CLIENTS>
          maximum number of concurrent metrics http clients to serve at once [default: 3]
  -h, --help
          Print help
  -V, --version
          Print version
```

## IPv4/IPv6 and per-IP limit

By default, SSH listens on both `0.0.0.0:2222` and `[::]:2222` using 
separate sockets; the IPv6 socket is explicitly `IPV6_V6ONLY`.
To specify other ports, pass `--ssh-listen-address` once per address:

```sh
endlessh-rs --ssh-listen-address ip:0.0.0.0:22 \
  --ssh-listen-address 'ip:[::]:22' \
  --ssh-max-clients 512 --ssh-max-clients-per-ip 16
```

The global limit applies across all listeners. The per-IP limit counts 
**simultaneously open** TCP connections, instead of historical connections or 
request rates. IPv4-mapped IPv6 addresses share the corresponding IPv4 quota. 
Unix socket connections count only towards the global limit.

Connections rejected by the per-IP quota are closed immediately; 
rejection is logged only at debug verbosity (`-vv`) to avoid log flooding.

## Building

### Standard release build

To build a release binary without optional metrics:

```sh
cargo build --release --locked --no-default-features
```

### Static Linux build (x86_64)

For deployment on compatible x86-64 Linux servers, build a statically 
linked binary using the musl target.

Install the target once:

```sh
rustup target add x86_64-unknown-linux-musl
```

A compatible musl linker/toolchain must also be available on the build machine.

Build the release binary without optional metrics:

```sh
cargo build --release --locked --no-default-features \
  --target x86_64-unknown-linux-musl
```

The binary is written to:

```text
target/x86_64-unknown-linux-musl/release/endlessh-rs
```

Check its format:

```sh
file target/x86_64-unknown-linux-musl/release/endlessh-rs
```

On x86-64 Linux, the expected result is a **statically linked ELF executable** 
(typically `static-pie`). Release builds are automatically stripped of 
symbol information through `strip = "symbols"` in `Cargo.toml`.

## Testing

Run all unit and integration tests, including the dual-stack connection
limit load tests:

```sh
cargo test --locked --no-default-features
```

The tests in `tests/load_test.rs` start an isolated local instance on
a temporary port. They check IPv4 and IPv6 per-IP limits and the global
limit shared across both address families. 
The tests require IPv4 and IPv6 loopback support.

## Systemd

The example [`systemd/endlessh-rs.service`](systemd/endlessh-rs.service) 
runs under a dynamic, unprivileged user with a restricted systemd sandbox. 
It listens on TCP port 22 for IPv4 and IPv6 and uses a shared limit of 512 
connections, with at most 16 connections per source IP.

The service expects the binary at `/usr/local/bin/endlessh-rs`. 
Install the executable and the service file on the target server before
enabling the service.

**Before starting the tarpit on port 22**, move the actual SSH service 
to its new port, allow that port in the firewall, and verify a new SSH 
login through the new port. If OpenSSH is managed through systemd socket 
activation, update its socket listener as well. Keep an existing SSH 
session open until the new login has been confirmed.

This project does not modify OpenSSH or firewall configuration.

## Signals (Unix)

- `SIGTERM`: Gracefully shut down and print statistics.
- `SIGINT`: Gracefully shut down and print statistics.
- `SIGUSR1`: Print statistics without stopping the server.

## TODO

- [ ] Add logging?
- [ ] Support detailed client metrics
- [ ] Add a comparison between implementations
