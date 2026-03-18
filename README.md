# Mixer

A Rust HTTP proxy that routes traffic either directly to local targets or via an upstream HTTP proxy, based on configured IP ranges.

## Features

- **Smart routing**: Checks if the request's target is an IP address literal and whether it falls within configured CIDR ranges. Traffic to local IP ranges is handled directly; DNS-name targets and all other traffic are forwarded to an upstream HTTP proxy.
- **CONNECT tunneling**: Supports both plain HTTP proxy requests and HTTP CONNECT tunnels (used by HTTPS).
- **Multi-service**: A single process can host multiple independent proxy services (each with its own listen address, IP ranges, and upstream proxy).
- **Async & fast**: Built on [Tokio](https://tokio.rs/) and [Hyper v1](https://hyper.rs/).
- **Structured logging**: Uses [tracing](https://docs.rs/tracing) with `RUST_LOG` environment variable support.

## Usage

```
mixer [OPTIONS]

Options:
  -c, --config <CONFIG>  Path to the JSON configuration file [default: config.json]
  -h, --help             Print help
  -V, --version          Print version
```

### Example

```bash
# Default config file (config.json in current directory)
mixer

# Custom config file path
mixer --config /etc/mixer/config.json

# Adjust log level
RUST_LOG=debug mixer --config config.json
```

## Configuration

Configuration is a JSON file with a `services` array. Each service entry has:

| Field | Type | Description |
|---|---|---|
| `listen` | string | `host:port` to listen on, e.g. `"0.0.0.0:8080"` |
| `local_ranges` | string[] | CIDR ranges handled directly, e.g. `["10.0.0.0/8"]` |
| `upstream_proxy` | string | HTTP proxy URL for non-local traffic, e.g. `"http://proxy.corp:3128"` |

See [`config.example.json`](config.example.json) for a full example with two services.

## Building

Requires Rust 1.75+.

```bash
cargo build --release
# Binary at: target/release/mixer
```

## How it works

1. A client sends a request to Mixer (either a plain `GET http://host/path` or a `CONNECT host:443`).
2. Mixer checks whether the target host is an **IP address literal**.
3. If it is an IP and that IP falls inside one of the `local_ranges`, Mixer connects **directly** to the target.
4. Otherwise (hostname target, or IP outside all ranges), Mixer forwards the request to the configured `upstream_proxy`.
