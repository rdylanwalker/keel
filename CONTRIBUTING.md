# Contributing to Keel

Thank you for your interest in contributing to **Keel**! Whether you are fixing a bug, testing an alpha cloud/hypervisor target, improving the React portal, or adding a new feature, this guide covers how to set up your local environment, run the quality gates, and follow project conventions.

> **Before making architectural or feature changes:** Read [`SDD.md`](./SDD.md) (the authoritative Spec-Driven Development specification) and [`AGENTS.md`](./AGENTS.md) (mandatory architectural guardrails, security invariants, and verification rules).

---

## Reporting Issues & Target Feedback

Keel is in active early alpha. The local `kind` + in-cluster Forgejo/Rauthy workflow is continuously tested end-to-end in CI, while cloud and hypervisor targets (`proxmox`, `aws`, `gcp`, `azure`, `hetzner`, `Talos`/managed `kubeconfig`) are ready for real-world validation.

When opening an issue, please choose the appropriate [GitHub Issue Template](https://github.com/rdylanwalker/keel/issues/new/choose):

* **[Feature Request](https://github.com/rdylanwalker/keel/issues/new?template=feature-request.yml):** Propose a new capability, CLI command, UI enhancement, or integration aligned with [`SDD.md`](./SDD.md) and [`AGENTS.md`](./AGENTS.md).
* **[Bug Report](https://github.com/rdylanwalker/keel/issues/new?template=bug-report.yml):** Report unexpected behavior in the CLI, Web Portal, PR previews, Flux reconciliation, or RBAC/security.
* **[Bootstrap Friction / Target Report](https://github.com/rdylanwalker/keel/issues/new?template=bootstrap-friction.yml):** Share results or friction encountered during Stage 0 (`keel bootstrap init`) across `kind`, Proxmox, or cloud targets.
* **[Provider / Catalog / Form UX Feedback](https://github.com/rdylanwalker/keel/issues/new?template=provider-catalog-feedback.yml):** Provide feedback on Day-1 infrastructure providers, `catalog.yaml` JSON schemas, `@rjsf/core` form UX, or request new blueprints.

Please ensure any logs, manifests, or screenshots redact all secrets and sensitive tokens.

---

## Repository Layout

```text
keel/
├── crates/
│   ├── keel-core/          # Domain models, BootstrapTarget, Stage 0 state paths, JSONSchema, catalog, SOPS/age
│   ├── keel-git/           # Direct git CLI runner (SSH/HTTPS) & ForgeProvider trait adapters
│   ├── keel-pko/           # PKO v2 Stack synthesis, ephemeral previews, PKO run mapping, diff parser
│   ├── keel-kube/          # kube-rs watchers, SSE telemetry, tenant RBAC, preview runner, tenant S3 identities
│   ├── keel-server/        # Stateless axum HTTP/SSE server, OIDC PKCE auth, preview lifecycle, embedded SPA
│   └── keel-cli/           # clap CLI (bootstrap {init, recover}, auth, proposal, catalog, app, infra, preview, tenant, serve)
├── ui/                     # React + TypeScript + Shadcn UI (components/ui) + Tailwind CSS + @rjsf/core + wterm
├── e2e/                    # Playwright suite (tests/, scripts/start-test-server.mjs, scripts/capture-screenshots.mjs)
├── deploy/                 # Stage 0 Pulumi programs, bootstrap manifests, in-cluster keel-server bundle, seed add-ons, catalog
├── docs/                   # mdBook user & operations documentation (book.toml at repo root)
├── .github/                # Issue templates, PR template, ci.yml, e2e.yml, release.yml, docs.yml, actionlint.yaml
├── .releaserc.json         # Automated SemVer release & artifact publishing config
├── release/                # Pinned semantic-release toolchain (package.json + package-lock.json)
├── Dockerfile              # Multi-stage image: pnpm UI build -> static musl keel (cargo-zigbuild) -> nonroot runtime
├── renovate.json           # Weekly automated dependency updates with auto-merge
├── justfile                # Common developer build, lint, test, and server tasks
├── SDD.md                  # Full Spec-Driven Development & Non-Functional Requirements
└── AGENTS.md               # Engineering rules, security invariants & verification gates
```

---

## Prerequisites

* **Rust:** Latest stable toolchain with `clippy`, `rustfmt`, and [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov) (`cargo install cargo-llvm-cov`; needs the `llvm-tools-preview` component)
* **Node.js & pnpm:** Node.js `24+` and `pnpm` `12+`
* **Task Runner (recommended):** [`just`](https://github.com/casey/just)
* **For unit tests:** `git`, plus **`sops`** and **`age`** on `PATH`. The real SOPS + `age` round-trip test is skipped (with a loud warning) when they are missing, and fails if you set `KEEL_REQUIRE_SOPS=1`, as CI does with `sops` `v3.13.3` and `age` `v1.3.2`. Rust tests never touch your ambient kubeconfig.
* **For bootstrap and live E2E:** `pulumi`, `helm`, `kubectl`, `kind`, and `docker`
* **For docs:** `mdbook` (`cargo install mdbook`); Mermaid diagrams render client-side from the vendored `docs/assets/js/mermaid.min.js` (lazy-loaded only on pages with diagrams)

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
# (auto-uses ./keel-gitops if present; logs to target/keel-serve.log;
#  admin credentials are saved to target/keel-admin-username / target/keel-admin-password)
just build-serve

# Build UI + binary, bootstrap a local kind cluster (--with-forge), and serve in the background
just build-init-serve

# Start with a pre-seeded demo catalog. This rebuilds the UI with VITE_ENABLE_DEMO=true;
# open /?demo=true. Run `just build-ui` afterwards to restore a production bundle.
just serve-demo

# Stop serve, delete the kind management cluster, and remove local GitOps repos
just down
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

All pull requests and `main` branch pushes are subject to automated verification gates. To provide fast feedback for contributors while ensuring rock-solid release stability, the pipeline is split into fast PR checks and a strict pre-release live cluster gate on `main`:

* **Pull Requests (Fast Quality Gate via `ci.yml`):**
  PRs run formatting, linting, **at least 80% line coverage** (Rust via `cargo llvm-cov`, UI via `vitest --coverage`; currently about 84% and 96%), the SOPS round-trip test with pinned `sops`/`age`, `mdbook build`, the `semantic-release` config check (`release/verify-release-config.mjs`), and the **mocked Playwright end-to-end suite** (`pnpm --dir e2e test`). PR checks complete in under 3 minutes without spinning up full Kubernetes clusters or consuming cloud infrastructure resources.
* **Pushes to `main` (Strict Pre-Release Gate via `release.yml`):**
  Pushes to `main` execute `release.yml`, which orchestrates `ci.yml` and `e2e.yml` via reusable `workflow_call`. The `release` job declares `needs: [ci, e2e-live]`, ensuring `semantic-release` **only** runs if both the fast quality gates and the live `kind` E2E suite (`07-kind-live-e2e.spec.ts`) pass completely.

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
cargo llvm-cov --workspace --fail-under-lines 80   # or: just cov-rust
cargo llvm-cov --workspace --html                  # browse target/llvm-cov/html/index.html
```

#### React + Shadcn + Tailwind Frontend (`ui/`)

```bash
pnpm --dir ui format:check        # prettier --check
pnpm --dir ui lint                # eslint
pnpm --dir ui typecheck           # tsc --noEmit
pnpm --dir ui test:unit --coverage # or: just test-ui (>= 80% lines)
```

#### Playwright End-to-End Tests (`e2e/`)

Requires built `ui/dist` and the `keel` binary:

```bash
pnpm --dir e2e install
pnpm --dir e2e format:check       # prettier --check
pnpm --dir e2e lint               # eslint
pnpm --dir e2e typecheck          # tsc --noEmit
pnpm --dir e2e test               # mocked Playwright suite

# Opt-in live kind E2E (GUI + CLI against real Docker/kind, Flux, PKO, Forgejo, Rauthy).
# Needs docker, kind, pulumi, helm, kubectl, and a built ui/dist + keel binary (just build).
# Add `127.0.0.1 idp.keel.local forgejo.keel.local` to /etc/hosts first.
pnpm --dir e2e test:live                           # reuses existing kind-keel-mgmt if present
KEEL_E2E_REUSE_CLUSTER=0 pnpm --dir e2e test:live  # bootstraps a fresh kind cluster
# Clean up afterwards: just down

# Opt-in live cloud E2E (AWS, Azure, GCP Free-Tier tests)
pnpm --dir e2e test:live:cloud
# Or via just:
just test-e2e-cloud
```

---

## Multi-Cloud Free-Tier Live E2E Testing (AWS, Azure, GCP)

Keel includes a live end-to-end test suite (`e2e/tests/08-cloud-free-tier-live-e2e.spec.ts`) that validates Stage 0 cloud bootstrapping and Stage 1 PKO v2 provider provisioning against real cloud APIs using zero-cost free-tier resources:
* **AWS:** `t3.micro` / `t2.micro` EC2 instances (`aws-vm` blueprint) + S3
* **Azure:** `Standard_B1s` VMs (`azure-vm` blueprint) in `eastus`
* **GCP:** `e2-micro` VMs (`gcp-vm` blueprint) in `us-central1-a` (Always Free tier)

### Dual-Guard Environment Contract

To prevent unexpected billing, credential leaks, or CI failures when credentials are not present, every cloud test is protected by a **Dual-Guard Contract**:
1. **Explicit Provider Toggle:** The provider must be explicitly enabled via `KEEL_E2E_CLOUD_<PROVIDER>=1` (e.g. `KEEL_E2E_CLOUD_AWS=1`) or globally via `KEEL_E2E_CLOUD_ALL=1`.
2. **Valid Credentials Present:** The required cloud provider credential environment variables must be populated in the test environment.

If either guard is missing, the test suite skips that provider cleanly with an informational log message without failing the run.

| Provider | Toggle ENV Var (or `KEEL_E2E_CLOUD_ALL=1`) | Required Credential ENV Vars | Optional Config ENV Vars (with Free-Tier Defaults) |
| :--- | :--- | :--- | :--- |
| **AWS** | `KEEL_E2E_CLOUD_AWS=1` | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | `AWS_SESSION_TOKEN`, `AWS_REGION` (`us-east-1`), `KEEL_E2E_AWS_INSTANCE_TYPE` (`t3.micro`) |
| **Azure** | `KEEL_E2E_CLOUD_AZURE=1` | `ARM_CLIENT_ID`, `ARM_CLIENT_SECRET`, `ARM_TENANT_ID`, `ARM_SUBSCRIPTION_ID` | `AZURE_LOCATION` (`eastus`), `KEEL_E2E_AZURE_VM_SIZE` (`Standard_B1s`) |
| **GCP** | `KEEL_E2E_CLOUD_GCP=1` | `GOOGLE_CREDENTIALS` (or `GOOGLE_APPLICATION_CREDENTIALS`), `GCP_PROJECT` (or `GOOGLE_PROJECT`) | `GCP_REGION` (`us-central1`), `GCP_ZONE` (`us-central1-a`), `KEEL_E2E_GCP_MACHINE_TYPE` (`e2-micro`) |

### Step-by-Step Credential Acquisition & Setup

#### 1. AWS Free Tier Setup
1. Log in to the AWS Console (or configure `aws` CLI) on an account eligible for the AWS Free Tier.
2. Create an IAM user with EC2 and S3 permissions for testing:
   ```bash
   aws iam create-user --user-name keel-e2e-tester
   aws iam attach-user-policy --user-name keel-e2e-tester --policy-arn arn:aws:iam::aws:policy/AmazonEC2FullAccess
   aws iam attach-user-policy --user-name keel-e2e-tester --policy-arn arn:aws:iam::aws:policy/AmazonS3FullAccess
   aws iam create-access-key --user-name keel-e2e-tester
   ```
3. Export environment variables in your shell:
   ```bash
   export KEEL_E2E_CLOUD_AWS=1
   export AWS_ACCESS_KEY_ID="AKIAIOSFODNN7EXAMPLE"
   export AWS_SECRET_ACCESS_KEY="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
   export AWS_REGION="us-east-1"
   ```

#### 2. Azure Free Tier Setup
1. Log in to the Azure CLI and determine your active subscription:
   ```bash
   az login
   SUBSCRIPTION_ID=$(az account show --query id -o tsv)
   ```
2. Create a Service Principal with `Contributor` permissions scoped to your test subscription:
   ```bash
   az ad sp create-for-rbac \
     --name "keel-e2e-sp" \
     --role "Contributor" \
     --scopes "/subscriptions/${SUBSCRIPTION_ID}" \
     --query '{client_id: appId, client_secret: password, tenant_id: tenant}'
   ```
3. Export environment variables in your shell:
   ```bash
   export KEEL_E2E_CLOUD_AZURE=1
   export ARM_CLIENT_ID="<client_id>"
   export ARM_CLIENT_SECRET="<client_secret>"
   export ARM_TENANT_ID="<tenant_id>"
   export ARM_SUBSCRIPTION_ID="${SUBSCRIPTION_ID}"
   export AZURE_LOCATION="eastus"
   ```

#### 3. GCP Free Tier Setup
1. In Google Cloud Console or `gcloud` CLI, select your test project and enable the Compute Engine API:
   ```bash
   PROJECT_ID=$(gcloud config get-value project)
   gcloud services enable compute.googleapis.com
   ```
2. Create a service account with `Compute Admin` and `Storage Admin` roles:
   ```bash
   gcloud iam service-accounts create keel-e2e-sa \
     --description="Keel E2E Test Runner" \
     --display-name="keel-e2e-sa"

   gcloud projects add-iam-policy-binding ${PROJECT_ID} \
     --member="serviceAccount:keel-e2e-sa@${PROJECT_ID}.iam.gserviceaccount.com" \
     --role="roles/compute.admin"

   gcloud projects add-iam-policy-binding ${PROJECT_ID} \
     --member="serviceAccount:keel-e2e-sa@${PROJECT_ID}.iam.gserviceaccount.com" \
     --role="roles/storage.admin"

   gcloud iam service-accounts keys create ./gcp-e2e-key.json \
     --iam-account="keel-e2e-sa@${PROJECT_ID}.iam.gserviceaccount.com"
   ```
3. Export environment variables in your shell:
   ```bash
   export KEEL_E2E_CLOUD_GCP=1
   export GOOGLE_CREDENTIALS="$(cat ./gcp-e2e-key.json)"
   export GCP_PROJECT="${PROJECT_ID}"
   export GCP_ZONE="us-central1-a"
   ```

### Running Locally & Configuring GitHub Actions CI

* **Run Locally:**
  ```bash
  # Run all enabled cloud tests
  pnpm --dir e2e test:live:cloud
  # Or via just:
  just test-e2e-cloud
  ```
* **Configure GitHub Actions CI:**
  Navigate to **Settings → Secrets and variables → Actions** in your GitHub repository:
  * **Repository Secrets:**
    * `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`
    * `ARM_CLIENT_ID`, `ARM_CLIENT_SECRET`, `ARM_TENANT_ID`, `ARM_SUBSCRIPTION_ID`
    * `GOOGLE_CREDENTIALS` (paste raw JSON key string)
  * **Repository Variables:**
    * `KEEL_E2E_CLOUD_AWS`: `1` (or `0`)
    * `KEEL_E2E_CLOUD_AZURE`: `1` (or `0`)
    * `KEEL_E2E_CLOUD_GCP`: `1` (or `0`)
    * (or `KEEL_E2E_CLOUD_ALL`: `1`)

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

* **Pull Request Template:** When opening a pull request, GitHub will automatically populate the [Pull Request Template](.github/pull_request_template.md). Please complete the change summary, link any related issue(s), verify the architectural & spec guardrails checklist (stateless control plane, Flux CD + PKO v2, direct `git` CLI, secret hygiene, `SDD.md` alignment), and indicate which local quality gates passed.
* **Conventional Commits Required:** Versioning and GitHub Releases are governed by [Conventional Commits](https://www.conventionalcommits.org/) and `semantic-release`. Every commit message must follow the specification:
  * `feat(<scope>): ...` — new feature (minor version bump)
  * `fix(<scope>): ...` — bug fix (patch version bump)
  * `feat(<scope>)!: ...` or `BREAKING CHANGE:` footer — breaking change (currently a **minor** bump while Keel is pre-1.0, per `.releaserc.json`)
  * `chore:`, `docs:`, `test:`, `ci:`, `refactor:` — non-release maintenance (hidden from release notes)
  * `perf(<scope>): ...` / `revert: ...` — patch bump, listed under *Performance Improvements* / *Reverts* (both hand-written `revert:` commits and `git revert` commits release a patch)
  * Release notes and `CHANGELOG.md` use the `conventionalcommits` preset and list sections in the order **⚠ BREAKING CHANGES**, **Features**, **Bug Fixes**, **Performance Improvements**, **Reverts** (see `SDD.md` §9.3). After changing `.releaserc.json` or `release/package.json`, run `npm ci --prefix release --ignore-scripts && node release/verify-release-config.mjs .`
* **Lockfile Hygiene:** Keep `Cargo.lock`, `ui/pnpm-lock.yaml`, and `e2e/pnpm-lock.yaml` synchronized whenever modifying dependencies so weekly Renovate PRs auto-merge cleanly.
* **Tests & Docs:** Include unit tests (`cargo test` / `vitest`) for new logic, update Playwright specs in `e2e/tests/` for user-facing changes, and update [`SDD.md`](./SDD.md) if introducing a new architectural pattern.

---

## Releases & Dependency Automation

* **Releases:** Pushes to `main` trigger `release.yml`, which executes `ci.yml` and `e2e.yml` (`e2e-live-kind` + optional cloud E2E) via `workflow_call`. Gated behind `needs: [ci, e2e-live]` (`e2e.yml` runs the live `kind` job because `release.yml` passes `run_live: true`), `semantic-release` (installed with `npm ci` from the pinned `release/package-lock.json`) analyzes Conventional Commits, bumps `Cargo.toml` and `ui/package.json`, refreshes only the workspace entries in `Cargo.lock` (`cargo update --workspace`, then `cargo metadata --locked`), pins the keel-server image tag to the new version in `deploy/bootstrap/keel-server/{kustomization,deployment}.yaml`, `deploy/seed/core-addons/keel-server.yaml`, `deploy/catalog/core/keel-server/catalog.yaml` and the self-service `deploy/catalog/apps/keel-server/{catalog,manifests}.yaml` (committed with the release), generates `CHANGELOG.md`, and creates a **draft** GitHub Release. Follow-up jobs then:
  * build `keel` with `--locked`: **static musl** Linux binaries via `cargo-zigbuild` (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`; linkage verified with `file`) plus `aarch64-apple-darwin` and `x86_64-pc-windows-msvc`, and the UI archive;
  * build the `Dockerfile` for `linux/amd64,linux/arm64` with buildx, push `ghcr.io/<owner>/keel:{<version>,latest}` with SBOM + `mode=max` provenance attestations, and **Cosign keyless**-sign the image digest;
  * build the HTML docs archive and CycloneDX SBOM, write `SHA256SUMS` and `keel-image.txt`, `cosign sign-blob --bundle` every asset (`<asset>.sigstore.json`), upload everything, and un-draft the release. A failure in any job leaves the release as a draft.
* **Verifying a release:**
  ```bash
  ID='^https://github.com/<owner>/keel/.github/workflows/release.yml@refs/heads/main$'
  ISSUER=https://token.actions.githubusercontent.com
  cosign verify-blob --bundle keel-linux-amd64.tar.gz.sigstore.json \
    --certificate-identity-regexp "$ID" --certificate-oidc-issuer "$ISSUER" keel-linux-amd64.tar.gz
  sha256sum -c SHA256SUMS --ignore-missing
  cosign verify ghcr.io/<owner>/keel:<version> \
    --certificate-identity-regexp "$ID" --certificate-oidc-issuer "$ISSUER"
  ```
* **Container image:** `docker build -t keel:dev .` builds the default `runtime` target (Chainguard `wolfi-base`, UID `65532`, with the `git`, `ssh`, `sops`, and `age-keygen` CLIs that `keel serve` shells out to). `ENTRYPOINT` is `keel`; the default `CMD` is `serve --bind 0.0.0.0:8080`, so `docker run keel:dev --version` or `docker run keel:dev bootstrap ...` override it. `--target static` produces a binary-only `distroless/static` image, which cannot run `keel serve`. CI (`ci.yml` `container` job) builds and smoke-tests the `linux/amd64` image on every PR.
* **Supply chain:** Every GitHub Action is pinned to a full commit SHA with a `# vX.Y.Z` comment, every job declares least-privilege `permissions:` (workflow default `{}`), Dockerfile base images are pinned by digest, and release tools (mdbook, cargo-edit, cargo-zigbuild, Zig, Cosign, pnpm) are pinned to exact versions. Lint workflow changes with `actionlint` (`.github/actionlint.yaml` declares the `ubuntu-26.04` runner label). The mdBook site is deployed to **GitHub Pages** by `docs.yml`.
* **Dependencies:** Initialized with latest compatible stable versions and kept current via **Renovate** running weekly before 05:00 UTC on Mondays with auto-merge enabled for passing non-major updates. `helpers:pinGitHubActionDigests` and `docker:pinDigests` keep action SHAs and base-image digests current; a regex manager updates values annotated with `# renovate: datasource=... depName=...` in workflows and the `Dockerfile`; a second one bumps the annotated Helm chart `version:` pins of the Kyverno, Flux and PKO management add-ons together with their `catalog/apps` twins (`catalog/core`, `catalog/apps`, seed `HelmRelease`, Stage 0 Pulumi release) in one manually reviewed PR per chart (keep the `# renovate:` line directly above each pin; `keel-cli` tests check it); `release/package.json` is grouped as `release-tooling`. Zig is bumped manually together with its SHA-256s in the `Dockerfile`.
