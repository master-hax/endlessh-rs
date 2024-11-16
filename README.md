# endlessh-rs

an implementation of [endlessh](https://nullprogram.com/blog/2019/03/22/) in Rust

inspired by [endlessh](https://github.com/skeeto/endlessh) & [endlessh-go](https://github.com/shizunge/endlessh-go)

```
Usage: endlessh-rs.exe [OPTIONS]

Options:
      --ssh-listen-address <SSH_LISTEN_ADDRESS>          [default: 0.0.0.0:2222]
      --ssh-banner-line-length <SSH_BANNER_LINE_LENGTH>  [default: 32]
      --ssh-max-clients <SSH_MAX_CLIENTS>                [default: 4096]
      --ssh-message-delay-ms <SSH_MESSAGE_DELAY_MS>      [default: 10000]
      --metrics-listen-address <METRICS_LISTEN_ADDRESS>  [default: disabled]
      --metrics-max-clients <METRICS_MAX_CLIENTS>        [default: 3]
  -h, --help                                             Print help
  -V, --version                                          Print version
```

## TODO

- [ ] add logging?
- [ ] support detailed client metrics
- [ ] add signals support
- [ ] add comparison between implementations
