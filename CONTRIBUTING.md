# Contributing to Keel

Thank you for your interest in contributing to **Keel**! Whether you are fixing a bug, testing an alpha cloud/hypervisor target, improving the React portal, or adding a new feature, this guide covers how to set up your local environment, run the quality gates, and follow project conventions.

> **Before making architectural or feature changes:** Read [`SDD.md`](./SDD.md) (the authoritative Spec-Driven Development specification) and [`AGENTS.md`](./AGENTS.md) (mandatory architectural guardrails, security invariants, and verification rules).

---

## Reporting Issues & Target Feedback

Keel is in active early alpha. The local `kind` + in-cluster Forgejo/Rauthy workflow is continuously tested end-to-end in CI, while cloud and hypervisor targets (`proxmox`, `aws`, `gcp`, `azure`, `hetzner`, `Talos`/managed `kubeconfig`) are ready for real-world validation.

If you test any experimental target or hit a bug, please [open an issue on GitHub](https://github.com/rdylanwalker/keel/issues/new/choose) with your target, CLI flags, and relevant logs (taking care to redact any secrets or tokens).

---

## Repository Layout

```text
keel/
├── crates/
│   ├── keel-core/          # Domain models, BootstrapTarget, JSONSchema validator, catalog, SOPS/age
│   ├── keel-git/           # Direct git CLI runner (SSH/HTTPS) & ForgeProvider trait adapters
│   ├── keel-pko/           # PKO v2 Stack generator, ephemeral PR preview controller, diff parser
│   ├── keel-kube/          # kube-rs CRD watchers, SSE telemetry stream, tenant RBAC, K8s Secret state
│   ├── keel-server/        # Stateless axum HTTP/SSE server, OIDC PKCE auth, embedded SPA assets
│   └── keel-cli/           # clap CLI entrypoint (bootstrap, auth, infra, app, preview, serve)
├── ui/                     # React + TypeScript + Shadcn UI + Tailwind CSS + @rjsf/core
├── e2e/                    # Playwright end-to-end test suite & kind/Forgejo test harness
├── deploy/                 # Stage 0 target templates & seed Flux, PKO v2, Kyverno, Envoy, Flagger manifests
├── docs/                   # mdBook user & operations documentation
├── .github/workflows/      # GitHub Actions CI, Playwright E2E, and semantic-release pipelines
├── .releaserc.json         # Automated SemVer release & artifact publishing config
├── renovate.json           # Weekly automated dependency updates with auto-merge
├── justfile                # Common developer build, lint, test, and server tasks
├── SDD.md                  # Full Spec-Driven Development & Non-Functional Requirements
└── AGENTS.md               # Engineering rules, security invariants & verification gates
```

---

## Prerequisites

* **Rust:** Latest stable toolchain with `clippy`, `rustfmt`, and `cargo-llvm-cov`
* **Node.js & pnpm:** Node.js `24+` and `pnpm` `12+`
* **Task Runner (recommended):** [`just`](https://github.com/casey/just)
* **CLI Tools:** `git`, `sops`, `age`, `pulumi`, `helm`, `kubectl`, `kind`, and `docker`

---

## Build & Run Locally

Using `just`:

```bash
# Install UI and E2E dependencies
just install

# Build frontend assets (embedded via rust-embed) and Rust workspace
just build

# Start the control-plane server locally on 127.0.0.1:8080
just serve

# Or build, restart any previous backgrounded serve, and print URLs/credentials
# (auto-uses ./keel-gitops if present; logs to target/keel-serve.log)
just build-serve

# Start with a pre-seeded demo catalog
just serve-demo
```

Or run the underlying commands directly:

```bash
# Build frontend assets and Rust workspace
pnpm --dir ui install && pnpm --dir ui build
cargo build --workspace

# Start the control-plane server locally
cargo run --bin keel -- serve --bind 127.0.0.1:8080
```

To preview the mdBook documentation locally:

```bash
just docs
```

---

## Quality Gates (Enforced in CI)

All pull requests must pass formatting, linting, **at least 80% unit test coverage** (Rust and UI), and **Playwright end-to-end tests**.

### Full Suite

Run the full verification suite locally before opening a PR:

```bash
just check
```

Or run each ecosystem individually:

#### Rust Backend & CLI

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo llvm-cov --workspace --fail-under-lines 80
```

#### React + Shadcn + Tailwind Frontend (`ui/`)

```bash
pnpm --dir ui format:check        # prettier --check
pnpm --dir ui lint                # eslint
pnpm --dir ui typecheck           # tsc --noEmit
pnpm --dir ui test:unit --coverage
```

#### Playwright End-to-End Tests (`e2e/`)

Requires built `ui/dist` and the `keel` binary:

```bash
pnpm --dir e2e install
pnpm --dir e2e format:check       # prettier --check
pnpm --dir e2e lint               # eslint
pnpm --dir e2e typecheck          # tsc --noEmit
pnpm --dir e2e test               # mocked Playwright suite

# Opt-in live kind E2E (GUI + CLI against real Docker/kind, Flux, PKO, Forgejo, Rauthy)
pnpm --dir e2e test:live                           # reuses existing kind-keel-mgmt if present
KEEL_E2E_REUSE_CLUSTER=0 pnpm --dir e2e test:live  # bootstraps a fresh kind cluster
```

### Fast / Scoped Verification During Development

During active iteration, you can run scoped checks for just the crate or package you changed:

```bash
# Rust (scope to a single crate, e.g., keel-core)
just agent-rust keel-core

# React UI (skips coverage)
just agent-ui

# E2E static checks only
just agent-e2e
```

---

## Pull Request & Commit Conventions

* **Conventional Commits Required:** Versioning and GitHub Releases are governed by [Conventional Commits](https://www.conventionalcommits.org/) and `semantic-release`. Every commit message must follow the specification:
  * `feat(<scope>): ...` — new feature (minor version bump)
  * `fix(<scope>): ...` — bug fix (patch version bump)
  * `feat(<scope>)!: ...` or `BREAKING CHANGE:` footer — breaking change (major version bump)
  * `chore:`, `docs:`, `test:`, `ci:`, `refactor:` — non-release maintenance
* **Lockfile Hygiene:** Keep `Cargo.lock` and `ui/pnpm-lock.yaml` synchronized whenever modifying dependencies so weekly Renovate PRs auto-merge cleanly.
* **Tests & Docs:** Include unit tests (`cargo test` / `vitest`) for new logic, update Playwright specs in `e2e/tests/` for user-facing changes, and update [`SDD.md`](./SDD.md) if introducing a new architectural pattern.

---

## Releases & Dependency Automation

* **Releases:** Merging to `main` triggers `semantic-release` to tag a SemVer release, cross-compile binaries (`linux/amd64`, `linux/arm64`, `darwin/arm64`, `windows/amd64`), attach a CycloneDX SBOM and HTML documentation archive, and publish the mdBook documentation to **GitHub Pages**.
* **Dependencies:** Initialized with latest compatible stable versions and kept current via **Renovate** running weekly before 05:00 UTC on Mondays with auto-merge enabled for passing non-major updates.
