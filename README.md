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

## systemd (Linux)

An example service is available at [`systemd/endlessh-rs.service`](systemd/endlessh-rs.service).
It runs as an unprivileged dynamic user with a restricted systemd environment.
The service listens on IPv4 port 22 and uses the existing global limit of
512 concurrent SSH connections. This configuration works without dual-stack
or per-IP-limit support.

Install a previously built binary and the service file:

```sh
sudo install -m 755 target/release/endlessh-rs /usr/local/bin/endlessh-rs
sudo install -Dm 644 systemd/endlessh-rs.service /etc/systemd/system/endlessh-rs.service
sudo systemctl daemon-reload
```

**Before enabling this service on port 22**, move OpenSSH to another port,
open that port in the firewall, and confirm you can log in using a *new* SSH
connection. If OpenSSH uses systemd socket activation, adjust its socket too.
Keep your existing SSH session open during this change.

Then start the tarpit with `sudo systemctl enable --now endlessh-rs`.
To listen on IPv6 instead, change its single `--ssh-listen-address` argument
to `ip:[::]:22` (this version supports one SSH listener at a time).

## TODO

- [ ] add logging?
- [ ] support detailed client metrics
- [ ] add signals support
- [ ] add comparison between implementations
