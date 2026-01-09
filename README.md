# talos-pilot

A terminal UI for managing and monitoring Talos Linux Kubernetes clusters.

## Requirements

- Rust 1.85+
- A running Talos cluster
- `~/.talos/config` with valid credentials

## Building

```bash
cargo build --release
```

## Running

```bash
# Run with default talosconfig
cargo run

# Or run the release binary
./target/release/talos-pilot
```

### CLI Options

```
Usage: talos-pilot [OPTIONS]

Options:
  -c, --context <CONTEXT>    Talos context to use (from talosconfig)
      --config <CONFIG>      Path to talosconfig file
  -d, --debug                Enable debug logging
      --log-file <LOG_FILE>  Log file path [default: /tmp/talos-pilot.log]
  -h, --help                 Print help
  -V, --version              Print version
```

## Watching Logs

Logs are written to a file to avoid corrupting the TUI. To watch logs in real-time, open a second terminal:

```bash
# Watch logs with color
tail -f /tmp/talos-pilot.log

# Or specify a custom log file
cargo run -- --log-file ~/talos-pilot.log
# Then in another terminal:
tail -f ~/talos-pilot.log
```

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `q` / `Esc` | Quit |
| `r` | Refresh data |
| `j` / `Down` | Navigate down |
| `k` / `Up` | Navigate up |

## Local Development with Docker

See [docs/local-talos-setup.md](docs/local-talos-setup.md) for instructions on setting up a local Talos cluster using Docker.

## License

MIT
