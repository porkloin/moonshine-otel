# moonshine-otel

Telemetry and benchmarks for [Moonshine](https://github.com/hgaiser/moonshine).

- Moonshine's pipeline stats:
  - timing: capture, convert, encode, send
  - pacing, bitrate
  - GPU and CPU util
- OTLP for existing telemetry
- Pre-configured Grafana boards
- Trace support for the frames that run over budget
- Comparison of a proposed change against main, from real streams

> [!WARNING]
> moonshine-otel reads Moonshine's statistics socket, which isn't merged yet:
> [hgaiser/moonshine#257](https://github.com/hgaiser/moonshine/pull/257). Until it's in a Moonshine release, run
> Moonshine built from that branch.

## Prerequisites

- podman 4.4 or newer
- Moonshine, installed any of the [ways its README lists](https://github.com/hgaiser/moonshine#installation), with
  the statistics socket turned on (see below)
- `~/.local/bin` in your `PATH`

## Install

```sh
curl -fL https://github.com/porkloin/moonshine-otel/releases/latest/download/moonshine-otel-linux-amd64.tar.zst \
  | tar --zstd -x -C ~
systemctl --user daemon-reload
```

That puts the `moonshine-otel` tools in `~/.local/bin`, plus their user services. Installing starts nothing; the
quick start below picks what runs. To build from source instead: `just install`.

## Quick start

Turn on Moonshine's statistics socket in its `config.toml` and restart Moonshine:

```toml
[stats]
socket = "$XDG_RUNTIME_DIR/moonshine/stats.sock"
```

Every stream gets recorded to `~/moonshine-otel-results/`.

### With Grafana

```sh
systemctl --user enable --now moonshine-otel-stack.target moonshine-otel
```

|                |                                                  |
| -------------- | ------------------------------------------------ |
| Grafana        | <http://localhost:3000>                          |
| Prometheus     | <http://localhost:9090>                          |
| Tempo          | <http://localhost:3200>                          |
| OTLP collector | `localhost:4317` (gRPC), `localhost:4318` (HTTP) |

### Without Grafana

```sh
systemctl --user enable --now moonshine-otel
```

Sends to a collector on `localhost:4317`, or to `OTEL_EXPORTER_OTLP_ENDPOINT` set in
`~/.config/moonshine-otel/settings.env`.

### Moonshine in a container

Inside, set `MOONSHINE_OTEL_SPOOL` to a directory the host can see, in `~/.config/moonshine-otel/settings.env`:

```sh
systemctl --user enable --now moonshine-otel
```

On the host, set `MOONSHINE_OTEL_RELAY_SPOOL` to that directory:

```sh
systemctl --user enable --now moonshine-otel-stack.target moonshine-otel-relay
```

## Benchmark a change

Record a few streams of the same app, with the same Moonlight settings, on each build, then compare:

```sh
moonshine-otel watch --results-dir main     # with main's Moonshine running: stream 5 times, then Ctrl+C
moonshine-otel watch --results-dir mine     # the same with your branch's Moonshine
moonshine-otel compare main mine
```

Streams with the same settings count as repeats, and only differences beyond their run-to-run noise are flagged.
`compare` exits 1 on a regression; `--format markdown` gives a table for a pull request.

[moonshine-test-headless](https://github.com/porkloin/moonshine-test-headless) does all of this in one command with
`--baseline`.

## Charts

`moonshine-otel plot` draws charts as SVG and PNG, ready to attach to a pull request: click-to-flash latency per
build, a timeline of one stream's frames, frame pacing per stream, or time per pipeline stage. `--help` has the details.

## License

BSD-2-Clause.
