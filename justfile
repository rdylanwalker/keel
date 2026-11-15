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

# Build UI + keel binary, kill any previous serve, start serve in background, and report credentials
build-serve *ARGS: build
    #!/usr/bin/env bash
    set -euo pipefail
    pid_file="target/keel-serve.pid"
    log_file="target/keel-serve.log"

    if [ -f "$pid_file" ]; then
        old_pid=$(cat "$pid_file" 2>/dev/null || true)
        if [ -n "$old_pid" ] && kill -0 "$old_pid" 2>/dev/null; then
            echo "Stopping previous keel serve (PID $old_pid)..."
            kill -INT "$old_pid" 2>/dev/null || true
            for _ in $(seq 1 30); do
                kill -0 "$old_pid" 2>/dev/null || break
                sleep 0.1
            done
            if kill -0 "$old_pid" 2>/dev/null; then
                kill -9 "$old_pid" 2>/dev/null || true
            fi
        fi
        rm -f "$pid_file"
    fi

    # Also clean up any orphaned `target/debug/keel serve` without a pidfile
    if pgrep -f "target/debug/keel serve" >/dev/null 2>&1; then
        pkill -INT -f "target/debug/keel serve" 2>/dev/null || true
        for _ in $(seq 1 20); do
            pgrep -f "target/debug/keel serve" >/dev/null 2>&1 || break
            sleep 0.1
        done
    fi

    extra_args=({{ARGS}})
    has_bind=false
    has_repo_dir=false
    for arg in "${extra_args[@]+"${extra_args[@]}"}"; do
        case "$arg" in
            --bind|--bind=*) has_bind=true ;;
            --repo-dir|--repo-dir=*) has_repo_dir=true ;;
        esac
    done

    cmd_args=()
    if [ "$has_bind" = false ]; then
        cmd_args+=(--bind 127.0.0.1:8080)
    fi
    if [ "$has_repo_dir" = false ] && [ -f "keel-gitops/.keel/config.yaml" ]; then
        cmd_args+=(--repo-dir ./keel-gitops)
    fi
    if [ "${#extra_args[@]}" -gt 0 ]; then
        cmd_args+=("${extra_args[@]}")
    fi

    mkdir -p target
    nohup ./target/debug/keel serve "${cmd_args[@]}" > "$log_file" 2>&1 < /dev/null &
    new_pid=$!
    echo "$new_pid" > "$pid_file"

    ready=false
    for _ in $(seq 1 150); do
        if ! kill -0 "$new_pid" 2>/dev/null; then
            echo "keel serve exited unexpectedly. Log output:" >&2
            cat "$log_file" >&2 || true
            rm -f "$pid_file"
            exit 1
        fi
        if grep -q "Keel Control Plane listening on " "$log_file" 2>/dev/null; then
            ready=true
            break
        fi
        sleep 0.1
    done

    if [ "$ready" = false ]; then
        echo "Timed out waiting for keel serve to listen. Log output:" >&2
        cat "$log_file" >&2 || true
        exit 1
    fi

    echo "Keel serve running in background (PID: $new_pid, log: $log_file)"
    grep "Keel Control Plane listening on " "$log_file" | head -n 1
    clean_log=$(sed 's/\x1b\[[0-9;]*m//g' "$log_file")
    forgejo_port=$(printf '%s\n' "$clean_log" | awk -F'local_port=' '/Started automatic port-forward to in-cluster Forgejo/ {print $2; exit}')
    dex_port=$(printf '%s\n' "$clean_log" | awk -F'local_port=' '/Started automatic port-forward to in-cluster Dex OIDC/ {print $2; exit}')
    if [ -n "$forgejo_port" ]; then
        echo "Forgejo Local URL:       http://127.0.0.1:$forgejo_port"
    fi
    if [ -n "$dex_port" ]; then
        echo "Dex OIDC Local URL:      http://127.0.0.1:$dex_port/dex"
    fi
    grep "Bootstrap Token" "$log_file" || true

    if command -v kubectl >/dev/null 2>&1; then
        dex_email=$(kubectl get secret -n keel-system dex-config -o jsonpath='{.data.config\.yaml}' 2>/dev/null | base64 -d 2>/dev/null | awk '/email:/ {print $NF; exit}' || true)
        if [ -n "$dex_email" ]; then
            echo "Dex OIDC Admin Email:    $dex_email"
            dex_pass="${KEEL_ADMIN_PASSWORD:-}"
            if [ -z "$dex_pass" ] && [ -f "target/keel-dex-password" ]; then
                dex_pass=$(cat "target/keel-dex-password" 2>/dev/null || true)
            fi
            if [ -z "$dex_pass" ]; then
                dex_pass=$(kubectl get secret -n keel-system dex-admin -o jsonpath='{.data.password}' 2>/dev/null | base64 -d 2>/dev/null || true)
            fi
            if [ -n "$dex_pass" ]; then
                echo "Dex OIDC Admin Password: $dex_pass"
            else
                echo "Dex OIDC Admin Password: <not recoverable from bcrypt hash in dex-config; run 'just dex-reset-password' or set KEEL_ADMIN_PASSWORD>"
            fi
        fi
        forgejo_user=$(kubectl get secret -n keel-system forgejo-admin -o jsonpath='{.data.username}' 2>/dev/null | base64 -d 2>/dev/null || true)
        forgejo_pass=$(kubectl get secret -n keel-system forgejo-admin -o jsonpath='{.data.password}' 2>/dev/null | base64 -d 2>/dev/null || true)
        if [ -n "$forgejo_user" ] && [ -n "$forgejo_pass" ]; then
            echo "Forgejo Admin Username:  $forgejo_user"
            echo "Forgejo Admin Password:  $forgejo_pass"
        fi
    fi

# Reset the in-cluster Dex OIDC admin password, restart Dex, and save it for `just build-serve`
dex-reset-password password="":
    #!/usr/bin/env bash
    set -euo pipefail
    new_pass="{{password}}"
    if [ -z "$new_pass" ]; then
        new_pass=$(python3 -c 'import secrets, string; alphabet = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789"; print("".join(secrets.choice(alphabet) for _ in range(20)))')
    fi
    new_hash=$(python3 -c 'import sys, bcrypt; print(bcrypt.hashpw(sys.argv[1].encode(), bcrypt.gensalt(rounds=12)).decode())' "$new_pass")
    current_cfg=$(kubectl get secret -n keel-system dex-config -o jsonpath='{.data.config\.yaml}' | base64 -d)
    updated_cfg=$(python3 -c 'import re, sys; cfg, h = sys.argv[1], sys.argv[2]; print(re.sub(r"(?m)^(\s*hash:\s*).*$", lambda m: f"{m.group(1)}\"{h}\"", cfg, count=1), end="")' "$current_cfg" "$new_hash")
    kubectl create secret generic dex-config -n keel-system --from-literal=config.yaml="$updated_cfg" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
    kubectl rollout restart deployment/dex -n keel-system >/dev/null
    kubectl rollout status deployment/dex -n keel-system --timeout=60s >/dev/null
    mkdir -p target
    printf '%s' "$new_pass" > target/keel-dex-password
    chmod 600 target/keel-dex-password
    echo "Updated Dex admin password in keel-system/dex-config and restarted Dex."
    echo "Dex OIDC Admin Password: $new_pass"

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
