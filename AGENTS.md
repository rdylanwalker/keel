# AGENTS.md — AI Coding Agent Instructions for Keel

> **Authoritative Specification:** Read `SDD.md` in the repository root before making architectural or feature changes. `SDD.md` is the source of truth for system design, functional requirements, and non-functional requirements (`NFR-SEC`, `NFR-REL`, `NFR-PERF`, `NFR-OBS`, `NFR-EXT`, `NFR-QA`, `NFR-DEVOPS`).

## 1. Project Identity & Naming Conventions

* **Project Name:** Keel
* **CLI & Server Binary:** `keel` (e.g., `keel bootstrap init`, `keel serve`, `keel preview status`)
* **Kubernetes / Catalog Schema API Group:** `keel.io` (e.g., `keel.io/v1alpha1` for Git-stored `CatalogApplication` schemas — not a custom cluster CRD)
* **Rust Workspace Crates:** Use the `keel-*` prefix across all workspace members (`keel-core`, `keel-git`, `keel-pko`, `keel-kube`, `keel-server`, `keel-cli`), mapping directly to the module responsibilities defined in `SDD.md` §7.1.
* **System Namespaces:** `flux-system`, `keel-system`, `keel-previews`, and `tenant-<tenant-id>`.

## 2. Technology Stack & Repository Layout

```text
keel/
├── AGENTS.md                   # Agent instructions & architectural guardrails (this file)
├── SDD.md                      # Spec-Driven Development specification (source of truth)
├── README.md                   # Project overview, quick start & developer guide
├── Cargo.toml                  # Root Rust workspace (latest compatible crates)
├── .releaserc.json             # semantic-release configuration (Conventional Commits)
├── renovate.json               # Weekly Renovate dependency automation with auto-merge
├── .github/workflows/          # ci.yml, e2e.yml, release.yml
├── crates/
│   ├── keel-core/              # Domain models, BootstrapTarget, catalog parser, JSONSchema, SOPS/age
│   ├── keel-git/               # Direct git CLI runner (SSH/HTTPS) + ForgeProvider trait
│   ├── keel-pko/               # PKO v2 Stack synthesis, ephemeral PR previews, diff parser
│   ├── keel-kube/              # kube-rs watchers, SSE telemetry broadcaster, tenant RBAC, K8s Secret state
│   ├── keel-server/            # Stateless axum REST/SSE API, OIDC PKCE auth, embedded React SPA
│   └── keel-cli/               # clap CLI entrypoint
├── deploy/                     # Stage 0 target templates & seed Flux, PKO v2, Kyverno, Envoy, Flagger manifests
├── ui/                         # React + TypeScript + Shadcn UI + Tailwind CSS + @rjsf/core
└── e2e/                        # Playwright end-to-end test suite
```

## 3. Non-Negotiable Architectural Guardrails

AI agents **MUST NOT** violate the following constraints from `SDD.md`:

1. **Provider-Agnostic Stage 0 Bootstrap Targets:**
   * Never hardcode Proxmox VE as a mandatory dependency in `keel-cli` or `keel-core`.
   * All Stage 0 bootstrapping must flow through the `BootstrapTarget` trait (`kind`, `kubeconfig`, `proxmox`, `aws`, `gcp`, `azure`, `hetzner`), producing an identical seed control-plane layout (Flux CD, PKO v2, S3, SOPS/`age`, Kyverno, Envoy Gateway, Flagger) regardless of the underlying host.
   * Ensure Flux `GitRepository` manifests support outbound pull-polling (`interval: 1m`) so `kind` and local `k3s` clusters behind NAT/firewalls work without inbound webhooks.

2. **No Crossplane or Custom Infrastructure CRDs:**
   * Do **not** introduce Crossplane, XRDs, Compositions, or custom Kubernetes controllers to wrap cloud/Proxmox resources.
   * All infrastructure provisioning post-Stage 0 must use **Pulumi Kubernetes Operator (PKO v2)** executing Pulumi YAML/HCL or OpenTofu/Terraform providers via `pulumi package add terraform-provider`.

3. **Flux CD Only (No Argo CD):**
   * **Flux CD** (`GitRepository`, `Kustomization`, `HelmRelease`) is the sole GitOps reconciliation engine. Do not add Argo CD manifests or dependencies.

4. **Zero Structural Modification of Upstream Apps:**
   * Upstream Helm charts and Kustomize bases must work out of the box without forks or patches.
   * Dynamic GUI forms must render `catalog.yaml` JSON Schemas using **`@rjsf/core`** + **Shadcn UI** widgets and serialize directly into standard Helm `values.yaml` overrides.

5. **Direct `git` CLI Execution & Thin `ForgeProvider` Trait:**
   * Execute all Git operations (`clone`, `checkout`, `add`, `commit`, `push`) via `tokio::process::Command::new("git")` with explicit argument vectors. **Never** invoke `sh -c` or interpolate user strings into shell commands (`NFR-SEC-06`).
   * Keep Git forge REST interactions strictly isolated behind the `ForgeProvider` trait (`ForgejoGiteaProvider`, `GitHubProvider`, `GitLabProvider`, `LocalGitProvider`).

6. **Mandatory PR + Ephemeral PKO Preview Workflow (Stage 1+):**
   * Outside of Stage 0 (`keel bootstrap init`), the control plane must **never** commit directly to `main`.
   * Every infrastructure or application mutation must create a branch, open a PR via `ForgeProvider`, spawn an ephemeral PKO v2 `Stack` (`spec.preview: true`) in `keel-previews`, stream the diff report to CLI/GUI, and garbage-collect ephemeral resources on PR merge/close (`NFR-REL-02`).

7. **Control-Plane Anti-Lockout:**
   * Management cluster core resources must carry `protect: true`. Maintain the break-glass `keel bootstrap recover` path against versioned S3 state (`NFR-REL-01`).

8. **Progressive Delivery & Stateless Control Plane:**
   * Use **Flagger + Envoy Gateway + Prometheus** for opt-in Kubernetes HTTP/gRPC canary rollouts.
   * Keep `keel-server` **stateless** with no embedded or external database (no SQLite/Postgres). Store OIDC/session keys and user Git credentials natively in Kubernetes `Secret`s and read live state from Git, Forge APIs, and `kube-rs` CRD caches.

9. **Strict Multi-Tenancy & Secret Hygiene:**
   * Enforce namespace-per-tenant isolation (`tenant-<id>`), Flux `--no-cross-namespace-refs=true`, Kyverno policies, and `restricted` Pod Security Standards (`NFR-SEC-02`, `NFR-SEC-03`, `NFR-SEC-04`).
   * Encrypt Git secrets with **SOPS + `age`** per-tenant keys before staging. Encrypt user SSH keys/PATs at rest with `AES-256-GCM`. **Never** log or return secrets in API responses (`NFR-SEC-05`).

## 4. Dependency & Release Hygiene (`NFR-DEVOPS`)

* **Latest Compatible Versions:** When adding or initializing crates, npm packages, container images, or GitHub Actions, always resolve to the **latest compatible stable versions**. Do not pin outdated major versions unless explicitly documented due to an upstream incompatibility.
* **Conventional Commits Required:** `semantic-release` governs all versioning and GitHub Releases (`SDD.md` §9.3). Every commit message **must** follow Conventional Commits:
  * `feat(<scope>): ...` (minor version bump)
  * `fix(<scope>): ...` (patch version bump)
  * `feat(<scope>)!: ...` or `BREAKING CHANGE:` footer (major version bump)
  * `chore:`, `docs:`, `test:`, `ci:`, `refactor:`
* **Renovate Compatibility:** Keep `Cargo.lock` and `ui/pnpm-lock.yaml` synchronized whenever modifying manifests so weekly Renovate PRs cleanly auto-merge.

## 5. Quality Gates & Verification Commands

To avoid exhausting API token/TPM limits during active development, **default to scoped, quiet verification** for the areas you actually modified. Reserve full coverage and E2E suites for CI, pre-PR checks (`just check`), or when explicitly requested by the user.

### Fast / Scoped Agent Verification (Default)

Run only the checks relevant to your changes, using concise output flags (or the `just agent-*` recipes):

```bash
# Rust (scope to the modified crate, e.g., keel-core)
cargo fmt --all -- --check
cargo clippy -p <crate> --all-targets --message-format=short -- -D warnings
cargo test -p <crate> -q
# Or via just: `just agent-rust <crate>`

# React UI (`ui/`, skip --coverage during routine edits)
pnpm --dir ui format:check
pnpm --dir ui lint --quiet
pnpm --dir ui typecheck
pnpm --dir ui test:unit --reporter=dot
# Or via just: `just agent-ui`

# E2E (`e2e/`, static checks only unless E2E behavior changed)
pnpm --dir e2e format:check
pnpm --dir e2e lint --quiet
pnpm --dir e2e typecheck
# Or via just: `just agent-e2e`
```

Do **not** run `cargo llvm-cov`, `--coverage`, full `--workspace` sweeps, or `pnpm --dir e2e test` after every edit unless the user asks for them.

### Full Quality Gate Suite (CI / Pre-PR / On-Request, `NFR-QA-01`, `NFR-QA-02`)

Run locally via `just check` + `just cov-rust` before opening a PR or when full verification is requested:

#### Rust Backend & CLI

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo llvm-cov --workspace --fail-under-lines 80
```

#### React + Shadcn + Tailwind Frontend (`ui/`)

```bash
pnpm --dir ui format:check   # prettier --check
pnpm --dir ui lint           # eslint
pnpm --dir ui typecheck      # tsc --noEmit
pnpm --dir ui test:unit --coverage # Must maintain >= 80% line coverage
```

#### Playwright End-to-End Tests (`e2e/`)

```bash
pnpm --dir e2e format:check  # prettier --check
pnpm --dir e2e lint          # eslint
pnpm --dir e2e typecheck     # tsc --noEmit
pnpm --dir e2e test
```

## 6. Agent Execution Workflow

When implementing any feature or fix:

1. **Cross-Reference `SDD.md` (Targeted Reads):** Use `grep` or line-range reads on `SDD.md` for the relevant section instead of loading the entire file into context. Verify how the requested change maps to existing sections, API contracts, CRD schemas, and NFRs. If a requirement introduces a new architectural pattern, update `SDD.md` alongside the code.
2. **Design & Isolate:** Keep business logic in `keel-core`, `keel-git`, `keel-pko`, or `keel-kube` so it can be unit-tested independently of HTTP/CLI transport layers.
3. **Write Unit Tests First / Alongside Code:** Every new parser, `BootstrapTarget` driver, `ForgeProvider` method, SOPS wrapper, PKO diff mapper, and `@rjsf/core` widget must include comprehensive unit tests (`cargo test` / `vitest`).
4. **Update Playwright E2E Coverage:** Any change to user-facing flows (bootstrap, catalog forms, PR creation, PKO preview inspector, canary deployment, tenant isolation) must add or update corresponding specs in `e2e/tests/`.
5. **Run Scoped Verification:** Execute the relevant fast/scoped checks in §5 for modified crates/packages before reporting completion, and note when full coverage (`cargo llvm-cov`, `--coverage`) or `e2e` browser tests were skipped.

### Subagent Model & Thinking-Level Delegation

When delegating subtasks via `spawn_agent`, match the model and thinking level to the complexity of the task (call `list_agents_and_models` first to inspect available native `models[].id` options):

* **Routine / Mechanical Tasks** (running scoped test/lint/build commands, summarizing logs, boilerplate edits, simple searches): Use a fast, lower-cost model and lower thinking level.
* **Standard Implementation & Refactoring** (well-scoped feature work, unit tests, UI components following existing patterns): Use a balanced general-purpose model and medium thinking level.
* **Complex Architecture, Debugging & Cross-Cutting Changes** (subtle concurrency/Kubernetes/security bugs, `SDD.md`-level design, multi-crate refactors): Use the highest-capability reasoning model and high thinking level.
