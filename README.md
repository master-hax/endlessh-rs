# endlessh-rs

an implementation of [endlessh](https://nullprogram.com/blog/2019/03/22/) in Rust

inspired by [endlessh](https://github.com/skeeto/endlessh) & [endlessh-go](https://github.com/shizunge/endlessh-go)

features:
* works on all platforms supported by [mio](https://github.com/tokio-rs/mio), including Windows
* completely single threaded
* minimal allocation
* listens on multiple TCP or Unix sockets, including IPv4 and IPv6
* optional openmetrics/prometheus http server (in the same thread)

```
Usage: endlessh-rs.exe [OPTIONS]

Options:
      --ssh-listen-address <SSH_LISTEN_ADDRESS>
          repeatable SSH listen address (ip:<socketaddr> or unix:<socketpath>)
          [default: one dual-stack socket on [::]:2222; IPv4 fallback]
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

## IPv4 and IPv6 listeners

By default, the SSH tarpit uses one dual-stack IPv6 listener on `[::]:2222`
(with `IPV6_V6ONLY` disabled), accepting IPv4 and IPv6 connections. If IPv6
cannot be bound, it tries an IPv4-only listener on `0.0.0.0:2222`.
Explicitly configured IPv6 listeners use `IPV6_V6ONLY`, allowing separate
IPv4 listeners on the same port. Configure multiple listeners by repeating
`--ssh-listen-address`:

```sh
endlessh-rs --ssh-listen-address ip:0.0.0.0:2222 \
  --ssh-listen-address 'ip:[::]:2222'
```

All SSH listeners share the existing `--ssh-max-clients` connection limit.

## TODO

- [ ] add logging?
- [ ] support detailed client metrics
- [ ] add signals support
- [ ] add comparison between implementations
