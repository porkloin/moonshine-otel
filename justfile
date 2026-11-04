# moonshine-otel developer recipes. `just --list` for an overview.

container := "podman"
rust := "docker.io/library/rust:1-slim"

# Exporter tests.
test:
    cargo test -p moonshine-otel-exporter

# fmt, clippy and tests for everything.
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

# Same as `check`, inside a Rust container (for hosts without a Rust toolchain).
check-container:
    {{container}} run --rm -v "$PWD:/src:Z" -w /src \
        -v moonshine-otel-cargo:/usr/local/cargo/registry \
        -v moonshine-otel-target:/src/target \
        {{rust}} bash -c 'rustup component add rustfmt clippy && cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace'

# Build in a Rust container and install everything the release tarball has (binaries, units, the stack's
# Quadlet units and configuration) into your home directory.
install:
    {{container}} run --rm -v "$PWD:/src:Z" -w /src \
        -v moonshine-otel-cargo:/usr/local/cargo/registry \
        -v moonshine-otel-target:/src/target \
        {{rust}} cargo build --release -p moonshine-otel -p moonshine-otel-exporter --bins
    ./deploy/stage.sh target/release
    systemctl --user daemon-reload

# Record every stream of the running Moonshine from its statistics socket,
# e.g. `just watch --results-dir results/main`.
watch *ARGS:
    cargo run --release -p moonshine-otel -- watch {{ARGS}}

# Compare two result files or directories.
compare baseline candidate *ARGS:
    cargo run --release -p moonshine-otel -- compare {{baseline}} {{candidate}} {{ARGS}}

# Synthetic frames through the real exporter (no GPU needed).
synthetic *ARGS:
    cargo run --release -p moonshine-otel-exporter --example synthetic -- {{ARGS}}

# Start and stop the installed stack (collector, Prometheus, Tempo, Grafana on http://localhost:3000).
stack-up:
    systemctl --user start moonshine-otel-stack.target

stack-down:
    systemctl --user stop moonshine-otel-stack.target

# Regenerate the Grafana dashboards from observability/grafana/generate.py.
dashboards:
    python3 observability/grafana/generate.py

# Regenerate test snapshots and fixtures after an intentional change.
update-snapshots:
    UPDATE_SNAPSHOTS=1 cargo test -p moonshine-otel-exporter
