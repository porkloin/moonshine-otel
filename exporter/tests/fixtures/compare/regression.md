### moonshine-otel compare: REGRESSION

- warning: 1920x1080@60 hevc 20000000bps: p99.9 reported but not judged: 1200 frames per iteration leave fewer than 10 beyond them (record longer streams)

#### 1920x1080@60 hevc 20000000bps

baseline `baseline-run` (5 it) vs candidate `regression-run` (5 it)

| metric | baseline | candidate | Δ | Δ% | threshold | verdict |
|---|---:|---:|---:|---:|---:|---|
| latency.captured.total.p50 (us) | 3683 ±8.00 | 5191 ±12.00 | +1508 | +40.9% | 184 | **regressed** |
| latency.captured.total.p95 (us) | 3853 ±6.00 | 5367 ±12.00 | +1514 | +39.3% | 193 | **regressed** |
| latency.captured.total.p99 (us) | 3913 ±8.00 | 5427 ±20.00 | +1514 | +38.7% | 196 | **regressed** |
| latency.captured.total.p999 (us) | 3997 ±8.00 | 5515 ±16.00 | +1518 | +38.0% | 200 | changed |
| latency.captured.convert.p50 (us) | 300 ±0.00 | 1799 ±1.00 | +1499 | +499.7% | 20.00 | **regressed** |
| latency.captured.convert.p95 (us) | 323 ±1.00 | 1823 ±0.00 | +1500 | +464.4% | 20.00 | **regressed** |
| latency.captured.convert.p99 (us) | 332 ±1.00 | 1832 ±1.00 | +1500 | +451.8% | 20.00 | **regressed** |
| latency.captured.convert.p999 (us) | 340 ±1.00 | 1840 ±2.00 | +1500 | +441.2% | 20.00 | changed |
| interval.p99 (us) | 16671 ±0.00 | 16671 ±0.00 | 0.00 | +0.0% | 834 | unchanged |
| over_budget_pct (%) | 0.00 ±0.00 | 0.00 ±0.00 | 0.00 | +0.0% | 0.10 | unchanged |
| pacing_miss_pct (%) | 0.00 ±0.00 | 0.00 ±0.00 | 0.00 | +0.0% | 0.10 | unchanged |
| effective_fps (fps) | 60.00 ±0.00 | 60.00 ±0.00 | 0.00 | +0.0% | 0.60 | unchanged |
| mbps (Mbps) | 20.18 ±0.01 | 20.18 ±0.00 | -0.00 | -0.0% | 1.01 | unchanged |

_32 unchanged per-stage rows hidden (use --all)_

_flagged when |Δ| > max(5% of baseline, 3×1.4826×√(MAD_b²+MAD_c²), floor)_
