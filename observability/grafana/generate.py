#!/usr/bin/env python3
"""Generate the Grafana dashboards in ./dashboards/Moonshine (a folder of that name in Grafana).

    python3 observability/grafana/generate.py

Edit this file rather than the JSON; the JSON is checked in so the stack works without Python.
"""

import json
from pathlib import Path

PROM = {"type": "prometheus", "uid": "prometheus"}
TEMPO = {"type": "tempo", "uid": "tempo"}
CAPTURED = 'frame_kind=~"delta|key"'
STAGES = ["channel_wait", "import", "convert", "submit", "encode_wait", "packetize", "send", "unaccounted"]
ALL_STAGES = STAGES[:4] + ["consumer_queue"] + STAGES[4:]


class Layout:
    """Places panels left to right, wrapping at 24 columns."""

    def __init__(self):
        self.x = 0
        self.y = 0
        self.row_h = 0
        self.next_id = 1

    def place(self, panel, w, h):
        if self.x + w > 24:
            self.x = 0
            self.y += self.row_h
            self.row_h = 0
        panel["gridPos"] = {"x": self.x, "y": self.y, "w": w, "h": h}
        panel["id"] = self.next_id
        self.next_id += 1
        self.x += w
        self.row_h = max(self.row_h, h)
        return panel

    def wrap(self):
        """Start the next panel on a new line."""
        if self.x:
            self.x = 0
            self.y += self.row_h
            self.row_h = 0

    def row(self, title):
        if self.x:
            self.x = 0
            self.y += self.row_h
            self.row_h = 0
        panel = {"type": "row", "title": title, "collapsed": False, "panels": []}
        self.place(panel, 24, 1)
        self.x = 0
        self.y += 1
        self.row_h = 0
        return panel


def target(expr, legend="", ref="A", instant=False, fmt="time_series"):
    t = {"datasource": PROM, "expr": expr, "legendFormat": legend, "refId": ref, "format": fmt}
    if instant:
        t["instant"] = True
        t["range"] = False
    return t


def timeseries(title, targets, unit="s", stack=False, description="", overrides=None, min_=None):
    custom = {"lineWidth": 1, "fillOpacity": 10, "showPoints": "never", "spanNulls": True}
    if stack:
        custom.update({"stacking": {"mode": "normal", "group": "A"}, "fillOpacity": 60})
    defaults = {"unit": unit, "custom": custom}
    if min_ is not None:
        defaults["min"] = min_
    return {
        "type": "timeseries",
        "title": title,
        "description": description,
        "datasource": PROM,
        "targets": targets,
        "fieldConfig": {"defaults": defaults, "overrides": overrides or []},
        "options": {
            "legend": {"displayMode": "table", "placement": "right", "calcs": ["lastNotNull", "max"]},
            "tooltip": {"mode": "multi", "sort": "desc"},
        },
    }


def stat(title, expr, unit, description="", thresholds=None, decimals=None):
    defaults = {"unit": unit, "color": {"mode": "thresholds"}}
    if decimals is not None:
        defaults["decimals"] = decimals
    defaults["thresholds"] = {
        "mode": "absolute",
        "steps": thresholds or [{"color": "text", "value": None}],
    }
    return {
        "type": "stat",
        "title": title,
        "description": description,
        "datasource": PROM,
        "targets": [target(expr, instant=True)],
        "fieldConfig": {"defaults": defaults, "overrides": []},
        "options": {"reduceOptions": {"calcs": ["lastNotNull"]}, "colorMode": "value", "graphMode": "none"},
    }


def rate(metric, sel, window="$__rate_interval"):
    return f"rate({metric}{{{sel}}}[{window}])"


def dashboard(uid, title, description, panels, variables, annotations=None, time="now-30m"):
    return {
        "uid": uid,
        "title": title,
        "description": description,
        "tags": ["moonshine", "moonshine-otel"],
        "timezone": "browser",
        "schemaVersion": 41,
        "version": 1,
        "editable": True,
        "graphTooltip": 1,
        "refresh": "10s",
        "time": {"from": time, "to": "now"},
        "templating": {"list": variables},
        "annotations": {"list": annotations or []},
        "panels": panels,
        "links": [
            {"title": "Moonshine", "type": "dashboards", "tags": ["moonshine-otel"], "asDropdown": True}
        ],
    }


def query_var(name, label, query, multi=True, include_all=True, all_value=".*"):
    v = {
        "name": name,
        "label": label,
        "type": "query",
        "datasource": PROM,
        "query": {"query": query, "refId": name},
        "definition": query,
        "refresh": 2,
        "sort": 2,
        "multi": multi,
        "includeAll": include_all,
    }
    if include_all:
        v["allValue"] = all_value
        v["current"] = {"text": "All", "value": "$__all"}
    return v


def custom_var(name, label, values, default, include_all=False):
    v = {
        "name": name,
        "label": label,
        "type": "custom",
        "query": ",".join(values),
        "current": {"text": default, "value": default},
        "options": [{"text": x, "value": x, "selected": x == default} for x in values],
        "multi": False,
        "includeAll": include_all,
    }
    if include_all:
        v["allValue"] = ".*"
    return v


def textbox(name, label, default):
    return {"name": name, "label": label, "type": "textbox", "query": default, "current": {"text": default, "value": default}}


# --------------------------------------------------------------------------------------------
# Live pipeline
# --------------------------------------------------------------------------------------------


def live():
    L = Layout()
    run = 'run_id=~"$run_id", codec=~"$codec"'
    cap = f"{run}, {CAPTURED}"
    kind = f'{run}, frame_kind=~"$frame_kind"'
    p = []

    p.append(L.row("Headline (captured frames, over the selected range)"))
    whole = "$__range"
    p.append(L.place(stat(
        "Total p99",
        f"histogram_quantile(0.99, sum({rate('moonshine_frame_duration_seconds', cap, whole)}))",
        "s", "Exact per-run numbers are in the result JSON; this is from Prometheus native histograms."), 4, 4))
    p.append(L.place(stat(
        "Total p50",
        f"histogram_quantile(0.5, sum({rate('moonshine_frame_duration_seconds', cap, whole)}))", "s"), 4, 4))
    # Rates over the whole dashboard range would divide by the range, not the run's length, so
    # these two show the last minute.
    p.append(L.place(stat(
        "fps (last 1m)",
        f"sum({rate('moonshine_frames_total', cap, '1m')})", "none", decimals=1), 4, 4))
    p.append(L.place(stat(
        "Mbps (last 1m)",
        f"sum({rate('moonshine_encoded_bytes_total', run, '1m')}) * 8 / 1e6", "none", decimals=2), 4, 4))
    red = [{"color": "green", "value": None}, {"color": "orange", "value": 0.1}, {"color": "red", "value": 1}]
    p.append(L.place(stat(
        "Over budget",
        f"100 * sum(increase(moonshine_frames_over_budget_total{{{run}}}[{whole}])) / sum(increase(moonshine_frames_total{{{cap}}}[{whole}]))",
        "percent", "Captured frames with total latency above 1/target_fps.", red, 2), 4, 4))
    p.append(L.place(stat(
        "Pacing miss",
        f"100 * sum(increase(moonshine_frames_pacing_miss_total{{{run}}}[{whole}])) / sum(increase(moonshine_frames_total{{{cap}}}[{whole}]))",
        "percent", "Captured frames arriving more than 1.5x the expected interval after the previous one: the game's own recent cadence, or the frame budget if longer. Stutter, not a game rendering below the target fps.", red, 2), 4, 4))

    p.append(L.row("Latency"))
    p.append(L.place(timeseries(
        "Total latency by frame kind",
        [target(f"histogram_quantile({q}, sum by (frame_kind) ({rate('moonshine_frame_duration_seconds', kind)}))",
                f"{{{{frame_kind}}}} p{int(q*100) if q < 0.999 else 99.9}", ref)
         for q, ref in [(0.5, "A"), (0.95, "B"), (0.99, "C")]],
        description="idr_reencode frames skip capture, import and convert; compare them separately."), 12, 9))
    p.append(L.place(timeseries(
        "Stage p99",
        [target(f"histogram_quantile(0.99, sum by (stage) ({rate('moonshine_frame_stage_duration_seconds', kind)}))",
                "{{stage}}")],
        description="consumer_queue is a sub-interval of encode_wait. unaccounted = total minus the sequential stages."), 12, 9))
    p.append(L.place(timeseries(
        "Mean stage breakdown (stacked)",
        [target(f"histogram_avg(sum by (stage) ({rate('moonshine_frame_stage_duration_seconds', cap + ', stage!=\"consumer_queue\"')}))",
                "{{stage}}")],
        stack=True,
        description="Sequential stages plus unaccounted; the stack height is the mean total latency."), 12, 9))
    p.append(L.place(timeseries(
        "Stage p50",
        [target(f"histogram_quantile(0.5, sum by (stage) ({rate('moonshine_frame_stage_duration_seconds', kind)}))",
                "{{stage}}")]), 12, 9))

    p.append(L.row("Pacing"))
    heat = {
        "type": "heatmap",
        "title": "Frame interval heatmap",
        "description": "Time between captured frames. A second band above the budget is stutter.",
        "datasource": PROM,
        "targets": [target(f"sum({rate('moonshine_frame_interval_seconds', run)})", "interval")],
        "options": {
            "calculate": False,
            "yAxis": {"unit": "s"},
            "color": {"mode": "scheme", "scheme": "Oranges", "scale": "exponential", "exponent": 0.5},
            "cellGap": 1,
            "tooltip": {"mode": "single", "yHistogram": True},
        },
        "fieldConfig": {"defaults": {}, "overrides": []},
    }
    p.append(L.place(heat, 12, 9))
    p.append(L.place(timeseries(
        "Frame interval",
        [target(f"histogram_quantile({q}, sum({rate('moonshine_frame_interval_seconds', run)}))", n, r)
         for q, n, r in [(0.5, "p50", "A"), (0.99, "p99", "B"), (1.0, "max", "C")]]), 12, 9))
    p.append(L.place(timeseries(
        "fps vs target",
        [target(f"sum by (run_id, target_fps) ({rate('moonshine_frames_total', cap)})", "{{run_id}} (target {{target_fps}})")],
        unit="none", min_=0), 8, 8))
    p.append(L.place(timeseries(
        "Over budget / pacing miss",
        [target(f"100 * sum({rate('moonshine_frames_over_budget_total', run)}) / sum({rate('moonshine_frames_total', cap)})", "over budget", "A"),
         target(f"100 * sum({rate('moonshine_frames_pacing_miss_total', run)}) / sum({rate('moonshine_frames_total', cap)})", "pacing miss", "B")],
        unit="percent", min_=0), 8, 8))
    p.append(L.place(timeseries(
        "Lost frames",
        [target(f"sum({rate('moonshine_stats_lagged_total', run)})", "stats lagged", "A"),
         target(f"sum by (reason) ({rate('moonshine_frames_dropped_total', run)})", "dropped: {{reason}}", "B")],
        unit="none", min_=0,
        description="Lagged: the harness fell behind the stats channel. Dropped: frames the recorder was told were dropped."), 8, 8))

    p.append(L.row("Bitrate and frame size"))
    p.append(L.place(timeseries(
        "Mbps",
        [target(f"sum({rate('moonshine_encoded_bytes_total', run)}) * 8 / 1e6", "Mbps")], unit="none"), 12, 8))
    p.append(L.place(timeseries(
        "Frame size",
        [target(f"histogram_quantile({q}, sum by (frame_kind) ({rate('moonshine_frame_size_bytes', kind)}))",
                f"{{{{frame_kind}}}} {n}", r) for q, n, r in [(0.5, "p50", "A"), (0.99, "p99", "B")]],
        unit="bytes"), 12, 8))

    p.append(L.row("GPU, CPU and instrumentation overhead"))
    p.append(L.place(timeseries(
        "GPU engine utilization",
        [target(f"max by (process, engine) ({{__name__=~\"moonshine_gpu_engine_utilization.*\", {run}}})", "{{process}} {{engine}}")],
        unit="percentunit", min_=0,
        description="From DRM fdinfo (or NVML). Absent when the driver does not expose it."), 8, 8))
    p.append(L.place(timeseries(
        "GPU memory",
        [target(f"max by (process, region) ({{__name__=~\"moonshine_gpu_memory_used.*\", {run}}})", "{{process}} {{region}}")],
        unit="bytes"), 8, 8))
    p.append(L.place(timeseries(
        "CPU by thread",
        [target(f"sum by (thread) ({rate('moonshine_process_cpu_time_seconds_total', run)})", "{{thread}}")],
        unit="percentunit", stack=True), 8, 8))
    p.append(L.place(timeseries(
        "Exporter record() cost",
        [target(f"histogram_quantile({q}, sum({rate('moonshine_exporter_record_duration_seconds', run)}))", n, r)
         for q, n, r in [(0.5, "p50", "A"), (0.99, "p99", "B")]],
        description="Time spent recording one frame on the harness task. Never on the pipeline thread."), 12, 8))

    variables = [
        query_var("run_id", "Run", "label_values(moonshine_frames_total, run_id)"),
        query_var("codec", "Codec", "label_values(moonshine_frames_total{run_id=~\"$run_id\"}, codec)"),
        custom_var("frame_kind", "Frame kind", ["delta", "key", "idr_reencode"], "All", include_all=True),
    ]
    variables[2]["multi"] = True
    variables[2]["current"] = {"text": "All", "value": "$__all"}
    annotations = [{
        "name": "Runs",
        "datasource": PROM,
        "enable": True,
        "iconColor": "rgba(80, 140, 255, 0.25)",
        "expr": 'group by (run_id) (increase(moonshine_frames_total{run_id=~"$run_id"}[30s]) > 0)',
        "step": "15s",
        "titleFormat": "run {{run_id}}",
        "tagKeys": "run_id",
        "useValueForTime": False,
    }]
    return dashboard(
        "moonshine-live",
        "Moonshine: live pipeline",
        "Where frame time goes, live. Exact per-run percentiles are in the result JSON and on bench.run spans.",
        p, variables, annotations)


# --------------------------------------------------------------------------------------------
# Run comparison
# --------------------------------------------------------------------------------------------


def whole_run(metric, run, extra=""):
    """The run's full cumulative histogram: runs finish, so the last value holds everything."""
    sel = f'run_id="{run}"' + (f", {extra}" if extra else "")
    return f"last_over_time({metric}{{{sel}}}[$__range])"


def compare():
    L = Layout()
    p = []
    quantiles = [("p50", 0.5), ("p75", 0.75), ("p90", 0.9), ("p95", 0.95), ("p99", 0.99), ("p99.9", 0.999)]

    def curve(run):
        parts = [
            f'label_replace(histogram_quantile({q}, sum({whole_run("moonshine_frame_duration_seconds", run, CAPTURED)})), "q", "{n}", "", "")'
            for n, q in quantiles
        ]
        return " or ".join(parts)

    p.append(L.row("Headline: B relative to A (whole runs, captured frames)"))

    def delta_stat(title, a_expr, b_expr, unit="percent", invert=False, description=""):
        good, bad = ("red", "green") if invert else ("green", "red")
        return stat(
            title, f"100 * (({b_expr}) - ({a_expr})) / ({a_expr})", unit, description,
            [{"color": good, "value": None}, {"color": "text", "value": -2}, {"color": bad, "value": 2}], 1)

    def q_expr(q, run, metric="moonshine_frame_duration_seconds", extra=CAPTURED):
        return f"histogram_quantile({q}, sum({whole_run(metric, run, extra)}))"

    p.append(L.place(delta_stat("Δ total p50", q_expr(0.5, "$run_a"), q_expr(0.5, "$run_b")), 6, 4))
    p.append(L.place(delta_stat("Δ total p99", q_expr(0.99, "$run_a"), q_expr(0.99, "$run_b")), 6, 4))
    p.append(L.place(delta_stat("Δ interval p99 (stutter)",
                                q_expr(0.99, "$run_a", "moonshine_frame_interval_seconds", ""),
                                q_expr(0.99, "$run_b", "moonshine_frame_interval_seconds", "")), 6, 4))

    def pacing(run):
        return (f'sum(last_over_time(moonshine_frames_pacing_miss_total{{run_id="{run}"}}[$__range])) / '
                f'sum(last_over_time(moonshine_frames_total{{run_id="{run}", {CAPTURED}}}[$__range]))')

    p.append(L.place(stat(
        "Pacing miss A → B",
        f"100 * ({pacing('$run_b')}) - 100 * ({pacing('$run_a')})", "percent",
        "Difference in percentage points.",
        [{"color": "green", "value": None}, {"color": "text", "value": -0.05}, {"color": "red", "value": 0.05}], 2), 6, 4))

    bar = {
        "type": "barchart",
        "title": "Total latency quantiles: A vs B",
        "description": "Whole-run quantile curve from native histograms. Exact values: `moonshine-otel compare`.",
        "datasource": PROM,
        "targets": [
            target(curve("$run_a"), "A", "A", instant=True, fmt="table"),
            target(curve("$run_b"), "B", "B", instant=True, fmt="table"),
        ],
        "transformations": [
            {"id": "joinByField", "options": {"byField": "q", "mode": "outer"}},
            {"id": "organize", "options": {
                "excludeByName": {"Time": True, "Time 1": True, "Time 2": True},
                "renameByName": {"Value #A": "A ($run_a)", "Value #B": "B ($run_b)"},
                "indexByName": {"q": 0}}},
        ],
        "fieldConfig": {"defaults": {"unit": "s"}, "overrides": []},
        "options": {"xField": "q", "orientation": "vertical", "groupWidth": 0.7, "barWidth": 0.9,
                    "legend": {"displayMode": "list", "placement": "bottom"}},
    }
    p.append(L.place(bar, 12, 10))

    stage_targets = []
    for ref, run, q in [("A", "$run_a", 0.5), ("B", "$run_a", 0.99), ("C", "$run_b", 0.5), ("D", "$run_b", 0.99)]:
        stage_targets.append(target(
            f'histogram_quantile({q}, sum by (stage) ({whole_run("moonshine_frame_stage_duration_seconds", run, CAPTURED)}))',
            "", ref, instant=True, fmt="table"))
    table = {
        "type": "table",
        "title": "Per-stage p50 / p99: A vs B",
        "datasource": PROM,
        "targets": stage_targets,
        "transformations": [
            {"id": "joinByField", "options": {"byField": "stage", "mode": "outer"}},
            {"id": "organize", "options": {
                "excludeByName": {"Time": True, "Time 1": True, "Time 2": True, "Time 3": True, "Time 4": True},
                "renameByName": {"Value #A": "A p50", "Value #B": "A p99", "Value #C": "B p50", "Value #D": "B p99"}}},
            {"id": "calculateField", "options": {
                "mode": "binary", "alias": "Δ p99",
                "binary": {"left": {"matcher": {"id": "byName", "options": "B p99"}}, "operator": "-",
                           "right": {"matcher": {"id": "byName", "options": "A p99"}}}}},
        ],
        "fieldConfig": {
            "defaults": {"unit": "s", "decimals": 5},
            "overrides": [{
                "matcher": {"id": "byName", "options": "Δ p99"},
                "properties": [
                    {"id": "custom.cellOptions", "value": {"type": "color-text"}},
                    {"id": "thresholds", "value": {"mode": "absolute", "steps": [
                        {"color": "green", "value": None}, {"color": "text", "value": -0.00005},
                        {"color": "red", "value": 0.00005}]}},
                ],
            }],
        },
        "options": {"showHeader": True},
    }
    p.append(L.place(table, 12, 10))

    runs = {
        "type": "table",
        "title": "Recent bench runs (from bench.run spans)",
        "description": "Exact per-run summary medians. Click a trace ID to open the run in Tempo.",
        "datasource": TEMPO,
        "targets": [{
            "datasource": TEMPO,
            "refId": "A",
            "queryType": "traceql",
            "tableType": "spans",
            "limit": 50,
            "spss": 1,
            "query": ('{ name = "bench.run" } | select(span.run_id, span.bench.codec, span.bench.resolution, '
                      'span.bench.target_fps, span.result.latency.captured.total.p50, '
                      'span.result.latency.captured.total.p99, span.result.interval.p99, '
                      'span.result.pacing_miss_pct, span.result.effective_fps, span.result.valid, '
                      'resource.moonshine.rev, resource.label.branch)'),
        }],
        "fieldConfig": {"defaults": {}, "overrides": []},
        "options": {"showHeader": True},
    }
    p.append(L.place(runs, 24, 10))

    variables = [
        query_var("run_a", "A (baseline)", "label_values(moonshine_frames_total, run_id)", multi=False, include_all=False),
        query_var("run_b", "B (candidate)", "label_values(moonshine_frames_total, run_id)", multi=False, include_all=False),
    ]
    return dashboard(
        "moonshine-compare",
        "Moonshine: run comparison",
        "Two runs side by side. Pick a time range that covers both. For a verdict with noise thresholds, use `moonshine-otel compare`.",
        p, variables, time="now-24h")


# --------------------------------------------------------------------------------------------
# Outlier explorer
# --------------------------------------------------------------------------------------------


def outliers():
    L = Layout()
    p = []
    select = ", ".join(
        ["span.frame.seq", "span.frame.kind", "span.frame.total_us", "span.frame.interval_us"]
        + [f"span.frame.{s}_us" for s in ALL_STAGES]
        + ["span.frame.encoded_bytes", "span.frame.over_budget", "span.frame.pacing_miss", "span.frame.cadence_us"])
    # Filter on the frame span's own run_id: a run's spans can sit in different Tempo blocks
    # until compaction, so structural queries against bench.run are unreliable.
    query = ('{ name = "frame" && span.run_id =~ "$run_id" && span.frame.${stage}_us >= $min_us } '
             '| select(' + select + ")")
    p.append(L.place({
        "type": "table",
        "title": "Outlier frames where $stage ≥ $min_us µs",
        "description": "Frame spans (outliers by default; all frames with --trace-mode all). Open a trace to see the lead_up event with the 8 preceding frames.",
        "datasource": TEMPO,
        "targets": [{"datasource": TEMPO, "refId": "A", "queryType": "traceql", "tableType": "spans",
                     "limit": 200, "spss": 200, "query": query}],
        "fieldConfig": {"defaults": {}, "overrides": []},
        "options": {"showHeader": True, "sortBy": [{"displayName": "span.frame.total_us", "desc": True}]},
    }, 24, 14))
    p.append(L.place(timeseries(
        "Outliers per second",
        [target('sum by (run_id) (rate(moonshine_frames_over_budget_total{run_id=~"$run_id"}[$__rate_interval]))', "{{run_id}} over budget", "A"),
         target('sum by (run_id) (rate(moonshine_frames_pacing_miss_total{run_id=~"$run_id"}[$__rate_interval]))', "{{run_id}} pacing miss", "B")],
        unit="none", min_=0), 24, 8))
    stages = ["total"] + ALL_STAGES + ["interval"]
    variables = [
        query_var("run_id", "Run", "label_values(moonshine_frames_total, run_id)"),
        custom_var("stage", "Stage", stages, "total"),
        textbox("min_us", "At least (µs)", "10000"),
    ]
    return dashboard(
        "moonshine-outliers",
        "Moonshine: outlier explorer",
        "Find spike frames by stage in Tempo.",
        p, variables, time="now-6h")


def main():
    out = Path(__file__).parent / "dashboards" / "Moonshine"
    out.mkdir(exist_ok=True)
    for name, d in [("live-pipeline", live()), ("run-comparison", compare()), ("outlier-explorer", outliers())]:
        (out / f"{name}.json").write_text(json.dumps(d, indent=2) + "\n")
        print(f"wrote {out / name}.json")


if __name__ == "__main__":
    main()
