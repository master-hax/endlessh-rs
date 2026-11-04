# endlessh-rs

an implementation of [endlessh](https://nullprogram.com/blog/2019/03/22/) in Rust

inspired by [endlessh](https://github.com/skeeto/endlessh) & [endlessh-go](https://github.com/shizunge/endlessh-go)

features:
* works on all platforms supported by [mio](https://github.com/tokio-rs/mio), including Windows
* completely single threaded
* minimal allocation
* listens on either TCP or unix sockets
* optional openmetrics/prometheus http server (in the same thread)

```
an ssh tarpit inspired by endlessh, with optional prometheus metrics

Usage: endlessh-rs[.exe] [OPTIONS]

Options:
      --ssh-listen-address <SSH_LISTEN_ADDRESS>
          address to accept ssh connections on, in the form "disabled", "ip:<socketaddr>", or "unix:<socketpath>" [default: ip:0.0.0.0:2222]
      --ssh-banner-line-length <SSH_BANNER_LINE_LENGTH>
          length, in bytes, of each randomly generated banner line sent to ssh clients [default: 32]
      --ssh-max-clients <SSH_MAX_CLIENTS>
          maximum number of ssh clients to trap at once [default: 4096]
      --ssh-message-delay-ms <SSH_MESSAGE_DELAY_MS>
          delay, in milliseconds, between banner lines sent to each trapped ssh client [default: 10000]
      --metrics-listen-address <METRICS_LISTEN_ADDRESS>
          address to serve openmetrics/prometheus stats on, in the form "disabled", "ip:<socketaddr>", or "unix:<socketpath>" [default: disabled]
      --metrics-max-clients <METRICS_MAX_CLIENTS>
          maximum number of concurrent metrics http clients to serve at once [default: 3]
  -v, --verbose...
          increase logging verbosity (-v logs each connection, -vv for debug)
  -h, --help
          Print help
  -V, --version
          Print version
```

## Comparison with other implementations

|                   | endlessh-rs | [endlessh](https://github.com/skeeto/endlessh) | [endlessh-go](https://github.com/shizunge/endlessh-go) |
|-------------------|-------------|-------------------------------------------------|------------------------------------------------------|
| Memory safe language | 🟢 yes (Rust) | 🔴 no (C) | 🟢 yes (Go) |
| Windows support | 🟢 yes | 🔴 no | 🟢 yes |
| Unix socket support | 🟢 yes, for ssh and metrics | 🔴 no | 🟡 metrics only, not ssh |
| Multiple listening ports | 🔴 no, one port | 🔴 no, one port | 🟢 yes, repeatable `-port` |
| Static binary support | 🟢 yes (musl target) | 🟢 yes (`-static`) | 🟢 yes (`CGO_ENABLED=0`) |
| Static binary size | 🟡 ~1.0 MB | 🟢 ~78 KB | 🔴 ~11.5 MB |
| Dynamic binary size | 🟡 ~940 KB | 🟢 ~23 KB | 🔴 ~11.6 MB |
| Number of OS threads | 🟢 1 | 🟢 1 | 🟡 `GOMAXPROCS` (usually = number of CPUs) |
| I/O event notification | 🟢 `epoll`/`kqueue`/IOCP | 🟡 `poll()` | 🟢 `epoll`/`kqueue`/IOCP |
| Connection polling | 🟢 none | 🟡 only those due to send | 🔴 all connections |
| Memory usage | 🟢 minimal | 🟢 minimal | 🔴 unbounded |
| `SIGUSR1` stats dump | 🟢 yes | 🟢 yes | 🔴 no |
| fail2ban-compatible logs | 🟢 yes | 🟢 yes | 🟢 yes |
| OpenMetrics/Prometheus support | 🟢 yes | 🔴 no | 🟢 yes |
| License | MIT OR Apache-2.0 | Unlicense | GPLv3 |

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

## TODO

- [ ] support detailed client metrics
