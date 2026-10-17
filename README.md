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
Usage: endlessh-rs.exe [OPTIONS]

Options:
      --ssh-listen-address <SSH_LISTEN_ADDRESS>
          address to accept ssh connections on, in the form "ip:<socketaddr>" or "unix:<socketpath>" [default: ip:0.0.0.0:2222]
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
  -h, --help
          Print help
  -V, --version
          Print version
```

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

- [ ] add logging?
- [ ] support detailed client metrics
- [ ] add signals support
- [ ] add comparison between implementations
