#!/bin/sh
# Lay out an install under $DESTDIR (default: the home directory), from the binaries in $1:
#   .local/bin/                          moonshine-otel, moonshine-otel-relay
#   .local/share/systemd/user/           moonshine-otel.service, moonshine-otel-stack.target,
#                                        moonshine-otel-relay.service
#   .config/containers/systemd/          the stack's Quadlet units
#   .local/share/moonshine-otel/         the stack's configuration and dashboards
#   .local/share/doc/moonshine-otel/     README.md, LICENSE
# Used by the release workflow and `just install`.
set -eu
bin=$1
dest=${DESTDIR:-$HOME}
repo=$(cd "$(dirname "$0")/.." && pwd)
obs=$dest/.local/share/moonshine-otel/observability

install -Dm755 "$bin/moonshine-otel" "$dest/.local/bin/moonshine-otel"
install -Dm755 "$bin/moonshine-otel-relay" "$dest/.local/bin/moonshine-otel-relay"
install -Dm644 -t "$dest/.local/share/systemd/user" "$repo/deploy/moonshine-otel.service" \
	"$repo/deploy/moonshine-otel-stack.target" "$repo/deploy/moonshine-otel-relay.service"
install -Dm644 -t "$dest/.config/containers/systemd" "$repo"/deploy/quadlet/*
install -Dm644 -t "$obs" "$repo/observability/otelcol.yaml" "$repo/observability/prometheus.yaml" \
	"$repo/observability/tempo.yaml"
install -Dm644 -t "$obs/grafana/dashboards/Moonshine" "$repo"/observability/grafana/dashboards/Moonshine/*.json
# Earlier installs kept them at the top, where Grafana would now list them outside any folder.
rm -f "$obs/grafana/dashboards/live-pipeline.json" "$obs/grafana/dashboards/run-comparison.json" \
	"$obs/grafana/dashboards/outlier-explorer.json" "$obs/grafana/dashboards/test-runs.json"
install -Dm644 -t "$obs/grafana/provisioning/dashboards" "$repo"/observability/grafana/provisioning/dashboards/*
install -Dm644 -t "$obs/grafana/provisioning/datasources" "$repo"/observability/grafana/provisioning/datasources/*
install -Dm644 -t "$dest/.local/share/doc/moonshine-otel" "$repo/README.md" "$repo/LICENSE"
