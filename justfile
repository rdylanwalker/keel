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
# (`?demo=true` fixtures are compiled out of production bundles, so the SPA is rebuilt with
# VITE_ENABLE_DEMO=true; run `just build-ui` afterwards to restore a production bundle)
serve-demo:
    #!/usr/bin/env bash
    set -euo pipefail
    VITE_ENABLE_DEMO=true pnpm --dir ui build
    tmp_dir=$(mktemp -d)
    trap 'rm -rf "$tmp_dir"' EXIT
    git init --bare "$tmp_dir/bare.git" >/dev/null
    git clone "$tmp_dir/bare.git" "$tmp_dir/work" >/dev/null 2>&1
    git -C "$tmp_dir/work" checkout -b main >/dev/null 2>&1
    mkdir -p "$tmp_dir/work/.keel" "$tmp_dir/work/catalog"
    cp -r deploy/catalog/* "$tmp_dir/work/catalog/"
    printf 'apiVersion: keel.io/v1alpha1\nkind: KeelConfig\nclusterName: keel-demo\nbootstrapTarget: kind\nforge:\n  provider: local\n  repoUrl: ""\n' > "$tmp_dir/work/.keel/config.yaml"
    git -C "$tmp_dir/work" add -A
    git -C "$tmp_dir/work" -c user.name="Keel Demo" -c user.email="demo@keel.io" commit -m "chore(demo): seed catalog and providers" >/dev/null
    git -C "$tmp_dir/work" push origin main >/dev/null 2>&1
    echo "Demo URL (unauthenticated UI preview): http://127.0.0.1:8080/?demo=true"
    echo "Live API URL (OIDC login): http://127.0.0.1:8080/"
    cargo run --bin keel -- serve --bind 127.0.0.1:8080 --repo-dir "$tmp_dir/work" --bare-repo "$tmp_dir/bare.git"

# Stop any running background or orphaned `keel serve` process
stop-serve:
    #!/usr/bin/env bash
    set -euo pipefail
    pid_file="target/keel-serve.pid"
    stopped=false

    if [ -f "$pid_file" ]; then
        old_pid=$(cat "$pid_file" 2>/dev/null || true)
        if [ -n "$old_pid" ] && kill -0 "$old_pid" 2>/dev/null; then
            echo "Stopping keel serve (PID $old_pid)..."
            kill -INT "$old_pid" 2>/dev/null || true
            for _ in $(seq 1 30); do
                kill -0 "$old_pid" 2>/dev/null || break
                sleep 0.1
            done
            if kill -0 "$old_pid" 2>/dev/null; then
                kill -9 "$old_pid" 2>/dev/null || true
            fi
            stopped=true
        fi
        rm -f "$pid_file"
    fi

    # Also clean up any orphaned `keel serve` binaries without a pidfile
    for pattern in "target/debug/keel serve" "target/release/keel serve"; do
        if pgrep -f "$pattern" >/dev/null 2>&1; then
            echo "Stopping orphaned '$pattern' process..."
            pkill -INT -f "$pattern" 2>/dev/null || true
            for _ in $(seq 1 20); do
                pgrep -f "$pattern" >/dev/null 2>&1 || break
                sleep 0.1
            done
            if pgrep -f "$pattern" >/dev/null 2>&1; then
                pkill -9 -f "$pattern" 2>/dev/null || true
            fi
            stopped=true
        fi
    done

    if [ "$stopped" = false ]; then
        echo "No running keel serve process found."
    fi

# Build UI + keel binary, bootstrap a local kind cluster (--with-forge), and start serve in background
build-init-serve *ARGS: build
    #!/usr/bin/env bash
    set -euo pipefail

    just stop-serve

    extra_args=({{ARGS}})
    repo_dir="./keel-gitops"
    cluster_name="keel-mgmt"
    bind_addr="127.0.0.1:8080"
    init_args=()

    i=0
    while [ "$i" -lt "${#extra_args[@]}" ]; do
        arg="${extra_args[$i]}"
        case "$arg" in
            --repo-dir)
                i=$((i + 1))
                repo_dir="${extra_args[$i]}"
                ;;
            --repo-dir=*)
                repo_dir="${arg#*=}"
                ;;
            --cluster-name)
                i=$((i + 1))
                cluster_name="${extra_args[$i]}"
                ;;
            --cluster-name=*)
                cluster_name="${arg#*=}"
                ;;
            --bind)
                i=$((i + 1))
                bind_addr="${extra_args[$i]}"
                ;;
            --bind=*)
                bind_addr="${arg#*=}"
                ;;
            *)
                init_args+=("$arg")
                ;;
        esac
        i=$((i + 1))
    done

    mkdir -p target
    init_log=$(mktemp)
    trap 'rm -f "$init_log"' EXIT

    ./target/debug/keel bootstrap init \
        --target kind \
        --cluster-name "$cluster_name" \
        --with-forge \
        --local-dev-docker-socket-exception \
        --repo-dir "$repo_dir" \
        "${init_args[@]+"${init_args[@]}"}" | tee "$init_log"

    init_user=$(awk -F'Admin Username:[[:space:]]*' '/^Admin Username:/ {print $2; exit}' "$init_log" || true)
    if [ -n "$init_user" ]; then
        printf '%s' "$init_user" > target/keel-admin-username
    fi
    if [ -n "${KEEL_ADMIN_PASSWORD:-}" ]; then
        printf '%s' "$KEEL_ADMIN_PASSWORD" > target/keel-admin-password
        chmod 600 target/keel-admin-password
    else
        init_pass=$(awk -F'Admin Password:[[:space:]]*' '/^Admin Password:/ {print $2; exit}' "$init_log" || true)
        if [ -n "$init_pass" ] && [ "$init_pass" != "[PROVIDED VIA CLI]" ]; then
            printf '%s' "$init_pass" > target/keel-admin-password
            chmod 600 target/keel-admin-password
        fi
    fi

    just _start-serve --bind "$bind_addr" --repo-dir "$repo_dir"

# Build UI + keel binary, kill any previous serve, start serve in background, and report credentials
build-serve *ARGS: build
    just _start-serve {{ARGS}}

# Internal helper: kill any previous serve, start serve in background, and report credentials (no build)
[private]
_start-serve *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    pid_file="target/keel-serve.pid"
    log_file="target/keel-serve.log"

    just stop-serve

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
    gateway_port=$(printf '%s\n' "$clean_log" | awk -F'local_port=' '/Started automatic port-forward to Envoy Gateway/ {print $2; exit}')
    if [ -n "$forgejo_port" ]; then
        echo "Forgejo Local URL:       http://127.0.0.1:$forgejo_port"
    fi
    if [ -n "$gateway_port" ]; then
        echo "Rauthy OIDC (gateway):   port-forwarded on 127.0.0.1:$gateway_port (https://idp.keel.local:8888/auth/v1/admin)"
    fi

    admin_user="${KEEL_ADMIN_USERNAME:-}"
    if [ -z "$admin_user" ] && [ -f "target/keel-admin-username" ]; then
        admin_user=$(cat "target/keel-admin-username" 2>/dev/null || true)
    fi
    admin_pass="${KEEL_ADMIN_PASSWORD:-}"
    if [ -z "$admin_pass" ] && [ -f "target/keel-admin-password" ]; then
        admin_pass=$(cat "target/keel-admin-password" 2>/dev/null || true)
    fi
    if [ -n "$admin_user" ]; then
        echo "Rauthy Admin Username:   $admin_user"
    fi
    if [ -n "$admin_pass" ]; then
        echo "Rauthy Admin Password:   $admin_pass"
    elif [ -n "$admin_user" ]; then
        echo "Rauthy Admin Password:   <not stored; set KEEL_ADMIN_PASSWORD or use the Rauthy admin UI password reset>"
    fi

    if command -v kubectl >/dev/null 2>&1; then
        forgejo_user=$(kubectl get secret -n keel-system keel-forgejo-admin -o jsonpath='{.data.username}' 2>/dev/null | base64 -d 2>/dev/null || true)
        forgejo_pass=$(kubectl get secret -n keel-system keel-forgejo-admin -o jsonpath='{.data.password}' 2>/dev/null | base64 -d 2>/dev/null || true)
        if [ -n "$forgejo_user" ] && [ -n "$forgejo_pass" ]; then
            echo "Forgejo Admin Username:  $forgejo_user"
            echo "Forgejo Admin Password:  $forgejo_pass"
        fi
    fi

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

# Run the README Quick Start literally against fresh kind clusters (keel-mgmt, workload-dev; ports 8080/8888)
test-e2e-quickstart: build
    pnpm --dir e2e test:live:quickstart

# Run multi-cloud free-tier live E2E suite (AWS/Azure/GCP; auto-loads .env.cloud-e2e if present)
test-e2e-cloud: build
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -f ".env.cloud-e2e" ]; then
        echo "Loading cloud E2E variables from .env.cloud-e2e..."
        set -a
        # shellcheck disable=SC1091
        source .env.cloud-e2e
        set +a
    fi
    pnpm --dir e2e test:live:cloud

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

# ------------------------------------------------------------------------------
# Local DX Pull-Through Registry Cache (Zot) — host/WSL-only, not deployed to clusters
# ------------------------------------------------------------------------------

# Start a long-lived local Zot pull-through cache container (`keel-zot-cache`) on the `kind` network
zot-up:
    #!/usr/bin/env bash
    set -euo pipefail
    cfg_dir="${HOME}/.cache/keel"
    cfg_file="${cfg_dir}/zot-config.json"
    mkdir -p "$cfg_dir"
    cat > "$cfg_file" <<'EOF'
    {
      "distSpecVersion": "1.1.0",
      "storage": {
        "rootDirectory": "/var/lib/registry",
        "dedupe": true,
        "gc": true,
        "gcDelay": "1h",
        "gcInterval": "24h"
      },
      "http": {
        "address": "0.0.0.0",
        "port": "5000"
      },
      "log": {
        "level": "info"
      },
      "extensions": {
        "sync": {
          "enable": true,
          "registries": [
            { "urls": ["https://registry-1.docker.io"], "onDemand": true, "tlsVerify": true },
            { "urls": ["https://ghcr.io"], "onDemand": true, "tlsVerify": true },
            { "urls": ["https://quay.io"], "onDemand": true, "tlsVerify": true },
            { "urls": ["https://registry.k8s.io"], "onDemand": true, "tlsVerify": true }
          ]
        }
      }
    }
    EOF
    docker network inspect kind >/dev/null 2>&1 || docker network create kind >/dev/null
    docker volume inspect keel-zot-cache-data >/dev/null 2>&1 || docker volume create keel-zot-cache-data >/dev/null
    if docker ps -a --format '{{ "{{" }}.Names{{ "}}" }}' | grep -qx 'keel-zot-cache'; then
        if docker ps --format '{{ "{{" }}.Names{{ "}}" }}' | grep -qx 'keel-zot-cache'; then
            echo "keel-zot-cache is already running on 127.0.0.1:5050 (kind network: http://keel-zot-cache:5000)."
        else
            docker start keel-zot-cache >/dev/null
            echo "Started existing keel-zot-cache container on 127.0.0.1:5050."
        fi
    else
        docker run -d \
            --name keel-zot-cache \
            --restart unless-stopped \
            --network kind \
            -p 127.0.0.1:5050:5000 \
            -v keel-zot-cache-data:/var/lib/registry \
            -v "${cfg_file}:/etc/zot/config.json:ro" \
            ghcr.io/project-zot/zot-minimal-linux-amd64:latest \
            serve /etc/zot/config.json >/dev/null
        echo "Started keel-zot-cache on 127.0.0.1:5050 (kind network: http://keel-zot-cache:5000)."
    fi
    echo "Run 'just zot-wire <cluster-name>' to point an existing kind cluster's containerd at the cache."

# Configure containerd on an existing kind cluster's nodes to pull through `keel-zot-cache` (falls back to upstream if stopped)
zot-wire cluster="keel-mgmt":
    #!/usr/bin/env bash
    set -euo pipefail
    nodes=$(kind get nodes --name "{{cluster}}" 2>/dev/null || true)
    if [ -z "$nodes" ]; then
        echo "No kind nodes found for cluster '{{cluster}}'." >&2
        exit 1
    fi
    for node in $nodes; do
        for reg_pair in \
            "docker.io|https://registry-1.docker.io" \
            "ghcr.io|https://ghcr.io" \
            "quay.io|https://quay.io" \
            "registry.k8s.io|https://registry.k8s.io"; do
            reg="${reg_pair%%|*}"
            upstream="${reg_pair##*|}"
            docker exec -i "$node" sh -c "mkdir -p /etc/containerd/certs.d/${reg} && cat > /etc/containerd/certs.d/${reg}/hosts.toml" <<EOF
    server = "${upstream}"

    [host."http://keel-zot-cache:5000"]
      capabilities = ["pull", "resolve"]
    EOF
        done
        echo "Wired containerd certs.d mirrors on node ${node}."
    done

# Show status of the local Zot pull-through cache container and volume
zot-status:
    #!/usr/bin/env bash
    set -euo pipefail
    docker ps -a --filter "name=^/keel-zot-cache$" --format 'table {{ "{{" }}.Names{{ "}}" }}\t{{ "{{" }}.Status{{ "}}" }}\t{{ "{{" }}.Ports{{ "}}" }}'
    if curl -fsS http://127.0.0.1:5050/v2/ >/dev/null 2>&1; then
        echo "Registry /v2/ endpoint: healthy (http://127.0.0.1:5050/v2/)"
    else
        echo "Registry /v2/ endpoint: unreachable"
    fi

# Stop and remove the local Zot cache container (pass `purge` to also delete cached image volume)
zot-down mode="":
    #!/usr/bin/env bash
    set -euo pipefail
    if docker ps -a --format '{{ "{{" }}.Names{{ "}}" }}' | grep -qx 'keel-zot-cache'; then
        docker rm -f keel-zot-cache >/dev/null
        echo "Removed keel-zot-cache container."
    else
        echo "keel-zot-cache container is not present."
    fi
    if [ "{{mode}}" = "purge" ]; then
        if docker volume inspect keel-zot-cache-data >/dev/null 2>&1; then
            docker volume rm keel-zot-cache-data >/dev/null
            echo "Removed keel-zot-cache-data volume."
        fi
    fi

# Stop keel serve, delete the local kind management cluster, and remove local GitOps repos
down cluster_name="keel-mgmt" repo_dir="./keel-gitops":
    #!/usr/bin/env bash
    set -euo pipefail
    just stop-serve

    if command -v kind >/dev/null 2>&1; then
        if kind get clusters 2>/dev/null | grep -qx "{{cluster_name}}"; then
            echo "Deleting kind cluster '{{cluster_name}}'..."
            kind delete cluster --name "{{cluster_name}}"
        else
            echo "Kind cluster '{{cluster_name}}' not found; skipping."
        fi
    else
        echo "kind CLI not found; skipping cluster deletion."
    fi

    repo_dir="{{repo_dir}}"
    repo_dir="${repo_dir%/}"
    bare_dir="${repo_dir}.git"

    for dir in "$repo_dir" "$bare_dir"; do
        if [ -e "$dir" ]; then
            echo "Removing local GitOps directory '$dir'..."
            rm -rf "$dir"
        fi
    done

    rm -f target/keel-admin-password target/keel-admin-username

alias cleanup := down

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

# Capture/update UI screenshots via Playwright against a running keel server (default http://127.0.0.1:8080)
screenshots *ARGS:
    pnpm --dir e2e screenshots {{ARGS}}
