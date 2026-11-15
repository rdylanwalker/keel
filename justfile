# Keel Developer Task Runner
# Run `just --list` to see all available recipes.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Default recipe: list available tasks
default:
    @just --list

# Install frontend and E2E dependencies
install:
    pnpm --dir ui install
    pnpm --dir e2e install

# Build the React SPA into ui/dist (embedded into keel-server)
build-ui:
    pnpm --dir ui build

# Build the Rust workspace
build-rust:
    cargo build --workspace

# Build UI assets and the keel binary
build: build-ui build-rust

# Run the Vite frontend dev server
dev-ui:
    pnpm --dir ui dev

# Start the keel control-plane server locally (real GitOps state, empty by default)
serve *ARGS:
    cargo run --bin keel -- serve --bind 127.0.0.1:8080 {{ARGS}}

# Start keel with a temporary Git repo seeded from deploy/catalog/ and demo UI data
serve-demo:
    #!/usr/bin/env bash
    set -euo pipefail
    tmp_dir=$(mktemp -d)
    trap 'rm -rf "$tmp_dir"' EXIT
    git init --bare "$tmp_dir/bare.git" >/dev/null
    git clone "$tmp_dir/bare.git" "$tmp_dir/work" >/dev/null 2>&1
    git -C "$tmp_dir/work" checkout -b main >/dev/null 2>&1
    mkdir -p "$tmp_dir/work/.keel" "$tmp_dir/work/providers" "$tmp_dir/work/catalog"
    cp -r deploy/catalog/* "$tmp_dir/work/catalog/"
    cp deploy/seed/providers/registry.yaml "$tmp_dir/work/providers/registry.yaml"
    printf 'version: v1\nauth_phase: bootstrap\n' > "$tmp_dir/work/.keel/config.yaml"
    git -C "$tmp_dir/work" add -A
    git -C "$tmp_dir/work" -c user.name="Keel Demo" -c user.email="demo@keel.io" commit -m "chore(demo): seed catalog and providers" >/dev/null
    git -C "$tmp_dir/work" push origin main >/dev/null 2>&1
    echo "Demo URL (unauthenticated UI preview): http://127.0.0.1:8080/?demo=true"
    echo "Live API URL (use printed Bootstrap Token): http://127.0.0.1:8080/"
    cargo run --bin keel -- serve --bind 127.0.0.1:8080 --repo-dir "$tmp_dir/work" --bare-repo "$tmp_dir/bare.git"

# Format Rust, UI, and E2E code
fmt:
    cargo fmt --all
    pnpm --dir ui format
    pnpm --dir e2e format

# Check formatting across Rust, UI, and E2E without modifying files
fmt-check:
    cargo fmt --all -- --check
    pnpm --dir ui format:check
    pnpm --dir e2e format:check

# Run linters and TypeScript typechecking across Rust, UI, and E2E
lint:
    cargo clippy --workspace --all-targets -- -D warnings
    pnpm --dir ui lint
    pnpm --dir ui typecheck
    pnpm --dir e2e lint
    pnpm --dir e2e typecheck

# Run Rust workspace unit & integration tests
test-rust:
    cargo test --workspace

# Run Rust coverage check (>= 80% line coverage; requires cargo-llvm-cov)
cov-rust:
    cargo llvm-cov --workspace --fail-under-lines 80

# Run React UI unit tests with coverage (>= 80% line coverage)
test-ui:
    pnpm --dir ui test:unit --coverage

# Build UI + keel binary and run Playwright E2E suite
test-e2e: build
    pnpm --dir e2e test

# Run all unit and E2E tests
test: test-rust test-ui test-e2e

# Run the full local CI quality gate suite
check: fmt-check lint test

# Fast, low-output checks for active agent/local iteration (pass crate name e.g. `just agent-rust keel-core`)
agent-rust crate="":
    cargo fmt --all -- --check
    @if [ -n "{{crate}}" ]; then \
        cargo clippy -p "{{crate}}" --all-targets --message-format=short -- -D warnings && \
        cargo test -p "{{crate}}" -q; \
    else \
        cargo clippy --workspace --all-targets --message-format=short -- -D warnings && \
        cargo test --workspace -q; \
    fi

# Fast, low-output UI checks (skips coverage table)
agent-ui:
    pnpm --dir ui format:check
    pnpm --dir ui lint --quiet
    pnpm --dir ui typecheck
    pnpm --dir ui test:unit --reporter=dot

# Fast E2E static checks (skips browser test execution)
agent-e2e:
    pnpm --dir e2e format:check
    pnpm --dir e2e lint --quiet
    pnpm --dir e2e typecheck

# Clean build, coverage, and test artifacts
clean:
    cargo clean
    rm -rf ui/dist ui/coverage e2e/playwright-report e2e/test-results book

# Serve documentation locally with mdBook
docs:
    mdbook serve --open

# Build static documentation with mdBook
docs-build:
    mdbook build
