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
      --ssh-max-clients-per-ip <SSH_MAX_CLIENTS_PER_IP>
          maximum concurrent SSH connections per source IP [default: 16]
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

## Per-IP connection limit

`--ssh-max-clients-per-ip` (default: 16) limits concurrent TCP clients
from the same IP address on the existing SSH listener. IPv4-mapped IPv6
addresses share their IPv4 quota. Unix socket clients count only towards
the global `--ssh-max-clients` limit. Rejected connections are immediately
closed and logged only at debug verbosity (`-vv`).

## TODO

- [ ] add logging?
- [ ] support detailed client metrics
- [ ] add signals support
- [ ] add comparison between implementations
