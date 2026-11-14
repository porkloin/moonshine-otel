# moonshine-otel: details

The parts of moonshine-otel that README.md leaves out: the spool and relay, comparing builds and how verdicts are decided,
the CLI, what each number means, and how the repository fits together. README.md is the short version for people.

## Layout

- `exporter/`: the `moonshine-otel-exporter` crate. Turns frame samples into histograms, OTLP metrics and spans, result
  JSON and per-frame CSV, and holds the comparison and `moonshine-otel-relay`.
- `cli/`: the `moonshine-otel` binary: `watch` (reads Moonshine's statistics socket in `cli/src/watch.rs`), `compare`,
  `env`, `plot` (`cli/src/plot.rs`), `summary` (`cli/src/summary.rs`), and run identity and resource sampling
  in `cli/src/env.rs`.
- Nothing links Moonshine: the socket is the only interface, so one build works with any Moonshine that has it.
- `deploy/`: the user unit that runs `watch`, the stack's target and Quadlet units (`deploy/quadlet/`), a hardened
  relay unit, and `stage.sh`, which lays out an install for the release tarball and `just install`.
- `observability/`: the configuration of the local collector, Prometheus, Tempo and Grafana, with generated
  dashboards.

## watch

`moonshine-otel watch` records a running Moonshine from its statistics socket (Moonshine's `[stats] socket`
setting, default path `$XDG_RUNTIME_DIR/moonshine/stats.sock`). The socket carries one JSON object per line:
`stream_started` (the format's `stats_version`, then resolution, fps, bitrate, codec, dynamic range, chroma), a `frame`
per encoded frame with its stage timings, `stream_stopped`, and `lagged` when the reader fell behind. Every duration
in a `frame` line is a whole number of microseconds, and `gpu_encode` and `bitstream_ready` are left out entirely when
Moonshine's GPU video queue reports no timestamps. The format is documented in Moonshine's
`src/stats_socket.rs`, and `cli/src/watch.rs` parses it. Each stream is one run (`run_id`). With `--results-dir`,
every stream leaves `<run_id>.json` and `<run_id>.frames.csv`; `compare` takes the directory. When Moonshine isn't listening,
`watch` retries every 2 s, so it can start before Moonshine and survives restarts.

The connection's peer credentials give Moonshine's pid, which the resource sampler measures (and the app processes
it starts). Several readers can connect at once, so `watch --results-dir` works while the service runs.

## Frame traces, host to client

Moonshine's `frame` lines carry `frame_number` (the frame index it sends, which the client receives) and
`captured_at` (microseconds since the Unix epoch); `watch` uses them as the frame's `seq` and capture time (`frames.csv`
writes the capture time as `captured_unix_us`). `content` uses Moonshine's color space names (`srgb`, `bt2020_pq`,
`scrgb_linear`), which `watch` maps to the workloads' mode names (`sdr`, `hdr10`, `scrgb`).
With `--stream-key-file PATH`, `watch` reads a key from PATH when each stream starts, and every frame with a number
becomes a trace of its own: the trace id and the root `frame` span's id come from the key and the frame number
(`exporter/src/frame_ids.rs`: FNV-1a 128 of `<key>/<number>`, FNV-1a 64 of `<key>/<number>/frame`), linked to the
run's `bench.run` span (the relay drops links; `run_id` and `stream.key` attributes join them too). The tracer
provider's `FrameIdGenerator` applies those ids, since the SDK can't set a span's ids.

`moonshine-otel client STATS --stream-key KEY` follows a test client's statistics (gamestream-test-client `--stats`,
JSON lines with wall clock times) and adds each frame's `client frame` span (first packet to decoded picture, with
`client: reassemble`, `client: wait for decoder` and `client: decode` inside) under that `frame` span, as
`service.name=gamestream-test-client`; `--once` reads to the end and exits, otherwise it follows the file until
SIGTERM. Host and client meet in one trace without passing context. The gap between Moonshine's span and the client's
is the network, as precise as the two machines' clocks agree (exact on one machine).

The client decides what happened and says so in its statistics; `client` only carries it to OTel. A `drop` (its loss
model dropped a frame on purpose) becomes a `client: dropped on purpose` span in that frame's trace, a `freeze`
(frames never reached the decoder) a `client: freeze` span in the trace of the frame that ended it, and every line
feeds the `moonshine.client.*` metrics: `frames` (by `frame_kind` and `result`: ok, decode_error, corrupt),
`frame.interval`, `decode.duration`, `frames.missing`, `freeze.duration` (by `recovered_by`: recovery_frame, keyframe,
neither), `frames.dropped` and `click.duration` (click sent to flash decoded). They carry `stream_key` and the
`--label`s; with a key, `watch`'s `moonshine.*` metrics carry `stream_key` too. Moonshine's metrics leave out the
frames of `watch --warmup`; the client's count every frame.

The client's `clock` lines say how its times line up with the host's: with `--clock` they're on the host's clock,
measured by NTP from the host's machine, and every client span carries `clock.alignment`, `clock.offset_us` and
`clock.uncertainty_us`, so the gap between Moonshine's span and the client's is the network to within that
uncertainty; without it, `clock.alignment` says the times are the client's own. `moonshine.client.clock.offset` and
`moonshine.client.clock.uncertainty` (by `when`: start, end) record both measurements; their difference is the drift
during the stream. Each `second` line (a second of the stream's bytes, and the busiest sliding one-second window so
far) feeds `moonshine.client.bitrate` (bit/s per second), `moonshine.client.bitrate.peak` and
`moonshine.client.bitrate.requested`. `moonshine.client.stream.start` and `.end` are when the stream's first and
latest frames arrived (Unix seconds), so a dashboard can fit its time range to a run.

Once the stream ends the client replays its decoded frames through its model of moonlight-qt's presenting (V-Sync,
and the frame pacer) and writes a `display` line per frame and a `display_summary`. Each `display` line becomes a span
in the frame's trace, `client: decoded to on screen` from decoding to the refresh that showed it, or a zero-length
`client: dropped by the display`, both V-Sync's (moonlight-qt's default), with `display.judder` and the pacer's fate
(`display.paced_shown`, `display.paced_wait_us`, `display.paced_judder`) on it; and feeds
`moonshine.client.display.latency` (decoded to on screen), `moonshine.client.display.frames` (by `shown`: true,
false) and `moonshine.client.display.judder` (frames that went up a refresh or more later than the stream's rate
needs), each by `mode` (vsync, paced), counted after `--warmup` by when the frame decoded.
`moonshine.client.display.refresh_rate` is the modelled display's rate. The `client` line (the client's build) puts
`client.version` on its frame spans.

`moonshine-otel app LOG --stream-key KEY [--label k=v]...` does the same for a test workload's log (clickflash's and
hdrneon's `--log`, JSON lines with wall clock times): `moonshine.app.inputs` (by `kind`), `moonshine.app.flashes`,
`moonshine.app.flash.duration` (input reaching the app to its flash's first frame going to the GPU) and
`moonshine.app.content.switches` (by `mode`). `cli/src/follow.rs` follows both kinds of file.

`moonshine-otel info --label k=v...` exports one `moonshine.test.info` sample (value 1) whose labels are facts no
measurement carries; moonshine-test-headless sends one per run for the host, the client and the run (code, baseline
and stream settings), and its test runs board shows them as tables.

Every click is a trace too, from the stream key and the click's id (`FrameIds::click`). The client puts each click's
id in the pointer's position (a 64 × 32 grid over the stream) and clickflash reads it back, so the id survives
scaling and lost inputs. `client` writes the root `click` span (sent to flash decoded, with `click.flash_frame` and a
link to that frame's trace, and `client: flash arrives to decoded`); `app` adds `app: input to flash submitted` under
it. The gap before the app's span is the input's trip through the network, Moonshine and the compositor; the flash
frame's trace has the rest.

Moonshine's `frame` lines carry `content`, the captured content's color space (`sdr`, `hdr10`, `scrgb`; absent on a
keyframe re-encoded without a capture). `watch` counts a change between frames as `moonshine.content.switches` (by
`to`), after warmup like the frames, and puts `frame.content` on frame spans; keyframes per switch is Moonshine's
keyframes over Moonshine's switches, both inside the stream.

`deploy/moonshine-otel.service` runs `watch` as a user service (no containers); the release tarball installs it into
`~/.local/share/systemd/user/`. It reads `~/.config/moonshine-otel/settings.env`: `OTEL_EXPORTER_OTLP_ENDPOINT` there
names a collector elsewhere (unreachable after 30 s is then an error, and systemd retries every 10 s); otherwise it
sends to `localhost` on `MOONSHINE_OTEL_OTLP_GRPC_PORT` (4317) and records without export if nothing answers.

## The spool (servers that can't reach a collector)

If moonshine-otel runs somewhere without a network path to your collector (a container with its own network namespace,
a VM, or anywhere you'd rather not give one), it can write the same OTLP data to a directory, and a relay outside
forwards it:

```sh
# inside
moonshine-otel watch --otlp-spool ~/.local/state/moonshine-otel/spool --results-dir results/live
# outside, with read access to that directory (a bind mount, a shared volume, the VM's disk)
moonshine-otel-relay /path/to/spool --otlp-endpoint http://127.0.0.1:4317
```

The user services take the same from `~/.config/moonshine-otel/settings.env`: `MOONSHINE_OTEL_SPOOL` for `watch`
inside, and `MOONSHINE_OTEL_RELAY_SPOOL` (plus `OTEL_EXPORTER_OTLP_ENDPOINT`, default `localhost` on
`MOONSHINE_OTEL_OTLP_GRPC_PORT`) for `moonshine-otel-relay.service` outside.

With `--otlp-spool` (or `MOONSHINE_OTEL_SPOOL`), `watch` opens no network connections. It writes one OTLP/protobuf request per file, and deletes its own files
after 10 minutes or past 64 MiB, so a stopped relay can't fill the disk. The relay only reads.

The relay treats the spool as untrusted, since whatever runs inside can write any file there:
- It reads only regular files with the writer's naming pattern, never follows a symlink, and skips files over 4 MiB.
- It decodes every batch and passes on only the `moonshine-otel` scope's `moonshine.*` metrics and spans, with scalar
  attribute values. Counts and lengths are capped. Exemplars and links are dropped.
- It tags everything with `moonshine_otel.relay.source` (the spool path, or `--source`), so the data can't pass itself
  off as another source.
- It forwards at most 64 MiB a minute (`--max-mib-per-minute`).
- Its logs report counts, never file contents.

Run it as an account the inside can't touch. The relay won't follow a symlink at the spool itself, but no directory
above the spool's parent may be writable from inside. `deploy/moonshine-otel-relay.service` is a hardened user unit for
it. The relay has no GPU or desktop dependencies.

## Comparing builds

Record the same streams on both builds into separate directories, then compare them:

```sh
moonshine-otel env                                       # check identity and noise sources
moonshine-otel watch --results-dir main --label branch=main
moonshine-otel watch --results-dir mine --label branch=mine
moonshine-otel compare main mine                               # exit 0 no regression, 1 regression, 2 invalid
moonshine-otel compare main mine --format markdown             # paste into a PR
```

`compare` reads every `*.json` in a directory and merges streams with the same resolution, fps, codec, bitrate, HDR
and chroma into one run, one iteration per stream. Five streams of at least 20 s each per build is a good start; the client
must ask for the same settings each time. Each stream's first `--warmup` seconds (2) are left out. Streams that
don't match a stream on the other side are listed and skipped.

If nothing listens on `localhost:4317`, `watch` warns and records without export. An `--otlp-endpoint` that can't be
reached is an error.

### How the verdict works

Each metric compares medians across iterations (streams). A difference is flagged only when

    |Δ| > max(rel_threshold × baseline, k × 1.4826 × √(MAD_baseline² + MAD_candidate²), floor)

The defaults are `--rel-threshold 5` (%) and `--k 3`. The floor is a per-metric minimum: 20 µs for latencies, 100 µs
for the interval, 0.1 points for percentages, and 0.5 fps. With one iteration the spread is unknown, so the verdict
falls back to the relative threshold and is labeled low-confidence. Record five streams per build.

A tail quantile only decides the verdict when at least 10 frames per iteration lie beyond it: p99 needs 1,000
frames (17 s at 60 fps), p99.9 needs 10,000. Below that it's the slowest few frames of each stream and moves with
single frames, so its row reads "changed" at most, and a warning says to record longer streams.

Metrics compared: total and per-stage p50/p95/p99/p99.9, interval p99 (stutter), over-budget %, pacing-miss %,
effective fps, and Mbps (reported as "changed", never as a regression). `--all` shows every row.

`compare` returns **invalid** (exit 2) when either run is marked invalid, when no workloads match, or when the
identities differ in a way that makes the numbers incomparable (a different GPU). Driver, Mesa,
kernel, CPU governor/EPP, platform profile, AC power, GPU power level and host differences only produce warnings,
unless you pass `--strict-identity`. `--allow-identity-mismatch` compares anyway.

An iteration is **invalid**, and left out of the aggregate, when more than 0.1% of its frames were lost, when it
recorded no frames, or when it was cut short (Moonshine or `watch` stopped mid-stream). A run with any invalid iteration is
marked invalid.

## Charts

`moonshine-otel plot` writes NAME.svg and NAME.png (`--out NAME`). The PNG is the same SVG rendered with resvg and
DejaVu Sans built in (`cli/fonts/`, with its license), so it looks the same anywhere; GitHub takes PNG attachments.

```sh
moonshine-otel plot clicks main=main.json mine=mine.json --out click-latency
moonshine-otel plot timeline hevc-loss.csv --out timeline
moonshine-otel plot pacing "hevc=hevc-clean.csv" "hevc, loss=hevc-loss.csv" --out pacing
moonshine-otel plot stages "hevc=hevc-clean.frames.csv" --out stages
```

- `clicks NAME=FILE...`: click-to-flash latency from gamestream-test-client JSON (`click_ms`, written with
  `--click-every`), one violin per NAME with every sample drawn, its median as a line, and median and p95 under it.
  Files with the same NAME are one group (several streams of one build). The violin is a Gaussian kernel density
  (Silverman's bandwidth) from the group's lowest to highest sample.
- `timeline CSV`: one stream from gamestream-test-client's `--csv`, on one time axis: a row of marks for keyframes,
  recovery frames (the CSV's `recovery` column, when it has one), frames that never arrived (a jump in frame numbers)
  and frames that failed to decode; the gap before each frame; and each frame's size. `--title` names it.
- `pacing NAME=CSV...`: a panel per group of gamestream-test-client CSVs: each frame's gap after the one before
  against its arrival time, a sideways histogram of the gaps, and p50/p99/max and stutters (over 3× p50) above it.
  In reports the target interval is dashed. The axis stops at four usual intervals; gaps past it are markers on the
  top edge, and the subtitle gives the longest.
- `stages NAME=CSV...`: per group of `watch` frames CSVs (warmup frames left out), each pipeline stage's median
  stacked into a bar, with the total's p50 and p99 marked.

## summary

`moonshine-otel summary BUNDLE --queries FILE --targets FILE` turns a test run's telemetry bundle
(`results.tar.gz`, its results folder, or the `logs/otlp` spool) into Markdown for a pull request. It starts a
throwaway Prometheus (`--prometheus`, on a free loopback port, with an out-of-order window wide enough for old
samples), POSTs each `*.metrics.pb` to its OTLP receiver, and runs the queries against it. The test harness owns
both files (moonshine-test-headless's `grafana/test-runs-queries.json`, written by the generator of its board, so
the two can't disagree, and its `targets.json`):

- queries: `{"metrics": [{"key", "expr", "empty", "decimals"}]}` in the report card's order. `expr` is PromQL with
  `{sel}` where the run's selector goes (`run="…"`, plus `side="…"` for a run's baseline streams), and may use
  `$__range` (the run's span) and `$fps`; `empty` is shown when it has no value.
- targets: `{"metrics": {KEY: {"label", "unit", "better", "target", "acceptable"}}}`, a target per query key.
  `better` is `higher` or `lower`; units `frame intervals` (also shown in ms at the run's frame rate), `ms`, `%`
  and counts. A null `target` is a metric shown without a verdict (no thresholds yet).

The output: a Hardware table from the run's `moonshine.test.info` samples (`role=host` and `role=client`, labels
`gpu`, `driver`, `other_gpus`, `cpu`, `memory`, `os`, `kernel`, `network`, `machine`), the stream from `role=run`
(`code`, `ref`, `commit`, `resolution`, `fps`, `bitrate`, `seconds`, `route`) and `moonshine_client_frames_total`
by `workload`, the tools it was made with (`harness_version`, `client_version`, `otel_version`, and this summary's own
version when it's another), the report card, then `--notes`. Two runs made with different tool versions get a
warning above the report card, since a metric's definition or the client's model can differ between them. Runs are told apart by the `run` label: `--run` picks one of
several (a run per mode); `--baseline BUNDLE` sets another run beside it as A and B, and a run with `side="base"`
streams gets test and baseline columns. A retained baseline (moonshine-test-headless keeps a run's baseline streams
and reuses them) is `--baseline BUNDLE --baseline-run RUN --baseline-side base`: that run's `side="base"` streams as
this run's baseline, named by its `baseline` fact. A run whose `role=run` facts name a `baseline_run` that's in the
same bundle (the harness copies a reused baseline's telemetry into the run's) gets that run's baseline streams as its
baseline without any option, and a `noise.json` beside the bundle's `logs/otlp` is read as `--noise` when that's not
given. Without `--run`, the run is the latest by when its first stream started. `--side NAME` (repeatable) sets sides of the one run beside each
other instead (a calibration's repeats: `base`, `base-2`, ...), and `--noise-out FILE` writes, per report card metric,
each side's value, their mean and their spread (largest less smallest) as JSON. `--noise FILE` reads one: with two
sides, a Change column gives test less baseline and calls it within noise, better or worse (by the target's `better`).
Within noise is within the larger of 1.5 × the spread (a few repeats see less than the whole range), 2% of the
baseline, and half the last digit shown (so a metric that didn't vary across the repeats doesn't call a rounding a
change). Blocking HTTP, so main runs it in `spawn_blocking`.

## CLI

```sh
moonshine-otel watch [--socket PATH] [--gpu <node>] [--warmup 2] [--results-dir DIR] [--label k=v]... \
    [--otlp-endpoint URL | --otlp-spool DIR] [--otlp-protocol grpc|http/protobuf] [--trace-mode outliers] \
    [--metric-interval 5] [--histogram-buckets exponential|explicit] [--no-export] [--stream-key-file PATH]
moonshine-otel client <STATS> --stream-key KEY [--warmup SECONDS] [--label k=v]... [--once] [export options as for watch]
moonshine-otel app <LOG> --stream-key KEY [--label k=v]... [--once] [export options as for watch]
moonshine-otel compare <baseline> <candidate> [--format text|markdown|json] [--all] \
    [--rel-threshold 5] [--k 3] [--strict-identity] [--allow-identity-mismatch]
moonshine-otel env [--gpu <node>] [--json]
moonshine-otel plot clicks <NAME=FILE>... [--out NAME] [--title TEXT]
moonshine-otel plot timeline <CSV> [--out NAME] [--title TEXT]
moonshine-otel plot pacing|stages <NAME=FILE>... --out NAME [--title TEXT]
moonshine-otel summary <BUNDLE> --queries FILE --targets FILE [--baseline BUNDLE] [--run NAME] [--notes FILE] [--out FILE] [--prometheus PATH]
moonshine-otel-relay <spool> [--otlp-endpoint URL] [--source NAME] [--max-mib-per-minute 64]
```

`<baseline>` and `<candidate>` are result files or directories of them. `--help` on any subcommand has the details. Exit codes: 0 ok, 1 regression, 2 invalid
comparison, 3 `watch` or `env` failed.

`env` and `watch` read the GPU's Vulkan device and driver version from `vulkaninfo --summary` (vulkan-tools), when
it's installed.

## What gets measured

Moonshine writes a `frame` line per encoded frame to its statistics socket. Each one becomes a `FrameSample`, from
which these are derived:

- **Frame kind**: `delta`, `key` or `idr_reencode`. An IDR re-encode is sent when no new frame arrives in time. It never
  went through capture, import or convert, so it's kept separate: a key frame with `channel_wait`, `import` and
  `convert` all exactly zero. `captured` means `delta` + `key`, the default group for headline numbers.
- **unaccounted** = `total − (channel_wait + import + convert + submit + encode_wait + packetize + send)`.
  `consumer_queue` is a sub-interval of `encode_wait` and isn't added. A negative value means the reported stage
  timings overlap. Such frames are counted, and histograms clamp the value at zero.
- **gpu_encode**, **bitstream_ready**: times measured inside the GPU's video queue, sub-intervals of `encode_wait`,
  so they are never added to the sequential sum either. Moonshine leaves both out when that queue reports no
  timestamps. `watch` carries them to `frames.csv` (the `gpu_encode_us` and `bitstream_ready_us` columns, empty when
  absent) and to the frame spans, and leaves them out of the stage histograms.
- **interval**: time between `watch` receiving consecutive captured frames, so delivery pacing plus receiver
  jitter.
- **over budget**: `total > 1/target_fps`.
- **pacing miss**: `interval > 1.5 × expected`. The expected interval is the source's own cadence (the median interval
  over the last 64 captured frames), or `1/target_fps` when that's longer or there are fewer than 8 intervals yet. A
  game that steadily renders 66 fps into a 120 fps stream isn't missing its pacing; a hitch in its rhythm is. The fps
  panel shows the rate itself, and outlier frame spans carry the cadence they were judged against (`frame.cadence_us`).
- **Resources** (averaged over the window in the result JSON, live as metrics): GPU engine busy share and memory from
  DRM fdinfo, for Moonshine's process (`bench`: encoder and compositor) and for the app (the
  `moonshine-session.service` unit plus Xwayland), and CPU time per Moonshine thread. For the proprietary Nvidia driver, which has no fdinfo, build with
  `--features nvml`. When a driver has no data, `watch` says so and leaves the values out.

### Metrics

Durations are in seconds, sizes in bytes. Histograms are base-2 exponential by default, which Prometheus stores as
native histograms (Prometheus ≥ 3.8). `--histogram-buckets explicit` uses fixed buckets that are dense from 0.25 ms to
50 ms. Every series carries `codec`, `resolution`, `target_fps`, `hdr` and `run_id`.

| Metric | Type | Extra attributes |
|---|---|---|
| `moonshine.frame.duration` | histogram (s) | `frame.kind` |
| `moonshine.frame.stage.duration` | histogram (s) | `frame.kind`, `stage` |
| `moonshine.frame.interval` | histogram (s) | |
| `moonshine.frame.size` | histogram (By) | `frame.kind` |
| `moonshine.frames` | counter | `frame.kind` |
| `moonshine.encoded.bytes` | counter (By) | |
| `moonshine.frames.over_budget`, `moonshine.frames.pacing_miss` | counter | |
| `moonshine.stats.lagged` | counter | |
| `moonshine.gpu.engine.utilization` | gauge (0–1) | `process`, `engine` |
| `moonshine.gpu.memory.used` | gauge (By) | `process`, `region` |
| `moonshine.process.cpu.time` | counter (s) | `thread` |
| `moonshine.exporter.record.duration` | histogram (s) | (self-overhead per frame) |

Prometheus's quantiles are for trends; the result JSON is the source of truth.

### Traces

- `bench.run` spans one run (one stream). Its attributes are the run identity, the workload, and the
  exact aggregate as `result.<metric>` (medians, µs), e.g. `result.latency.captured.total.p99`.
- `bench.iteration` is a child with the same `result.*` for a single iteration.
- `frame` spans have the stage durations as attributes (`frame.<stage>_us`, `frame.total_us`, `frame.interval_us`, and
  so on), plus `frame.gpu_encode_us` and `frame.bitstream_ready_us` on the frames whose GPU video queue reported
  them. The stages overlap, so they're attributes, not child spans. A span runs from capture to send; the capture
  time is receive time minus `total`, and the span is marked `timing=reconstructed`. Each outlier carries a `lead_up`
  event listing the 8 preceding frames.

`--trace-mode` sets which frames get a span:
- `outliers` (the default): frames over budget or with a pacing miss
- `sampled:<p>`: every ⌈1/p⌉-th frame by frame counter, plus all outliers
- `all`
- `none`

Useful TraceQL. Float attributes need a float literal:

```text
{ name = "bench.run" && span.result.latency.captured.total.p99 > 8000.0 }
{ name = "frame" && span.run_id = "<id>" && span.frame.convert_us >= 5000 }
```

### Overhead

`watch` is a separate process. Moonshine queues lines for each reader and drops them when a reader falls behind,
writing a `lagged` line instead, so a slow `watch` never delays the pipeline; lost lines are counted as `lagged`. The
recording cost per frame is exported as `moonshine.exporter.record.duration`.

## Observability stack

```sh
systemctl --user enable --now moonshine-otel-stack.target    # or `just stack-up`
```

Four Quadlet containers (`~/.config/containers/systemd/moonshine-otel-*`) on one podman network,
`moonshine-otel`, where they reach each other by container name (`moonshine-otel-prometheus`, `moonshine-otel-tempo`).
Each is wanted by `moonshine-otel-stack.target` and stops with it. Their configuration is bind-mounted from
`~/.local/share/moonshine-otel/observability/`, and their data lives in the volumes `moonshine-otel-prometheus`,
`moonshine-otel-tempo` and `moonshine-otel-grafana`. Ports are published on 127.0.0.1 only.

The published ports come from `~/.config/moonshine-otel/settings.env` when it sets them (one `NAME=value` per line;
the units read it at start, so restart `moonshine-otel-stack.target` after changing it):

| Variable | Default |
|---|---|
| `MOONSHINE_OTEL_GRAFANA_PORT` | 3000 |
| `MOONSHINE_OTEL_PROMETHEUS_PORT` | 9090 |
| `MOONSHINE_OTEL_TEMPO_PORT` | 3200 |
| `MOONSHINE_OTEL_OTLP_GRPC_PORT` | 4317 |
| `MOONSHINE_OTEL_OTLP_HTTP_PORT` | 4318 |

`watch` sends to `localhost` on the OTLP port variables when no `--otlp-endpoint` is given, and the user service reads
the same file, so a moved collector is followed. Inside the network the containers keep their standard ports. `watch` waits up to
30 s for the collector at startup, so both can start at login; `systemctl --user status 'moonshine-otel*'` shows
everything.

| | |
|---|---|
| OTLP | `localhost:4317` (gRPC), `localhost:4318` (HTTP) |
| Grafana | <http://localhost:3000>: anonymous viewer, `admin`/`admin` to edit |
| Prometheus | <http://localhost:9090> (OTLP receiver on, native histograms) |
| Tempo | <http://localhost:3200> |

Dashboards (folder "Moonshine", `observability/grafana/dashboards/Moonshine/`; the provider makes a folder per
directory, so another tool's boards dropped in a directory of their own get a folder of their own):
- **Live pipeline**: headline stats; total latency and stage p50/p99 by frame kind; a stacked mean stage breakdown; an
  interval heatmap (stutter shows up as a second band); fps against target; over-budget, pacing and loss rates;
  bitrate and frame size; GPU engines, GPU memory and CPU by thread; exporter overhead. Each run is drawn as an
  annotation region.
- **Run comparison**: pick runs A and B for whole-run quantile curves, a per-stage p50/p99 table with Δ, headline
  deltas, and recent `bench.run` spans with their exact summaries.
- **Outlier explorer**: `frame` spans where a chosen stage exceeds N µs, and outliers per second.

To use your own Prometheus, enable its OTLP receiver and native histograms (see `observability/prometheus.yaml`) and
import the JSON from `observability/grafana/dashboards/Moonshine/` (installed in
`~/.local/share/moonshine-otel/observability/grafana/dashboards/Moonshine/`).

## Developing

```sh
just test                  # exporter unit and integration tests
just check                 # fmt + clippy + all tests
just check-container       # same, inside a Rust container
just synthetic --duration 30 --out a.json                        # fake frames → exporter → stack
just synthetic --duration 30 --convert-shift-us 800 --out b.json # simulated regression
just compare a.json b.json
just dashboards            # regenerate the dashboard JSON from observability/grafana/generate.py
just update-snapshots      # after an intentional change to the result schema or compare output
```

Everything that decides what the numbers mean lives in the exporter crate. `cli/src/watch.rs` turns socket lines
into `FrameSample`s in one function (`to_sample`). The dashboards are generated: edit `generate.py`, not the JSON. Every `moonshine-otel`
example in README.md and AGENTS.md is parsed by a test (`readme_examples_parse` in `cli/src/main.rs`).


## Releasing

Commits on `main` follow [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `feat!:`).
release-please (`.github/workflows/release.yaml`, `release-please-config.json`) keeps a release pull request open
with the next version and its changelog; merging it tags `vX.Y.Z`, bumps the workspace version in `Cargo.toml`, and
the same workflow builds and attaches `moonshine-otel-linux-amd64.tar.zst`, which the README's install command
fetches. Before 1.0, `feat:` bumps the minor version and `fix:` the patch. Running the workflow by hand with a tag
rebuilds that tag's tarball.
