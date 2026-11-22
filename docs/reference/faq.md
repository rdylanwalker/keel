# FAQ

Short, honest answers to the questions people ask first. This page describes v0.12.0.

## Is it production ready?

No. Keel is alpha. Only the local `kind` path with the in-cluster Forgejo is tested end to end, the management cluster is a single node, and a release marked breaking has no upgrade path: you re-bootstrap a fresh cluster and repository. See the [maturity table](https://github.com/rdylanwalker/keel#maturity-what-is-tested) and [Known Limitations](known-limitations.md).

## Who maintains it, and is it AI-generated?

Keel has a single maintainer. It is developed with heavy AI assistance: much of the code is written by AI coding agents (those commits carry a `Co-Authored-By: Claude` trailer), working against the [`SDD.md`](https://github.com/rdylanwalker/keel/blob/main/SDD.md) specification and the rules in [`AGENTS.md`](https://github.com/rdylanwalker/keel/blob/main/AGENTS.md). That is why the codebase grew quickly.

Every change has to pass the same automated gates: `rustfmt`, `clippy -D warnings`, Rust and UI unit tests with at least 80% line coverage, ESLint and TypeScript checks, and the mocked Playwright suite. On `main`, a live end-to-end run also bootstraps a real `kind` cluster, opens and merges PRs with real PKO previews, and checks tenant isolation. Passing tests do not make it mature; treat it as alpha.

## Why Flux and not Argo CD?

- **Pull-only.** Flux polls Git (every minute in Keel), so clusters behind NAT or a home router need no inbound webhooks.
- **Multi-tenancy built in.** Flux's `--no-cross-namespace-refs` lockdown and per-namespace ServiceAccount impersonation map directly onto Keel's namespace-per-tenant model.
- **Plain CRDs, no extra UI.** Keel brings its own UI; Flux's `GitRepository`, `Kustomization` and `HelmRelease` objects are all it needs underneath.

Argo CD is a fine tool; Keel just does not support it, and there are no plans to.

## Why Pulumi and not Crossplane or plain Terraform?

- **No custom controllers or CRDs to write.** With Crossplane, new infrastructure types mean XRDs and Compositions. In Keel, a blueprint is an ordinary Pulumi YAML program that the Pulumi Kubernetes Operator (PKO) runs.
- **Terraform providers still work.** OpenTofu/Terraform providers are used through Pulumi's provider bridge (`pulumi package add terraform-provider`), for example the `tehcyx/kind` provider the E2E tests use.
- **A real plan on the PR.** Each infrastructure PR gets a real `pulumi preview` before merge, not a guess from the manifests.

If you want Flux plus Terraform without Keel's UI, [Tofu Controller](https://github.com/flux-iac/tofu-controller) is the closer fit.

## How is it different from Backstage, Kratix, Port or Cyclops?

Keel is much narrower than Backstage or Port and is self-hosted only. Kratix is the closest open-source analogue, with a more general model ("Promises") and more for you to build. Cyclops gives you Helm forms without the PR flow or infrastructure provisioning. See [Related projects](https://github.com/rdylanwalker/keel#related-projects).

## What are "Stage 0", "PKO", "Stack" and "blueprint"?

- **Stage 0**: the one-time `keel bootstrap init` that creates or attaches the management cluster and installs the control plane. It is the only time Keel writes to `main` directly.
- **PKO**: the [Pulumi Kubernetes Operator](https://github.com/pulumi/pulumi-kubernetes-operator), which runs Pulumi programs from Git inside the cluster.
- **Stack**: PKO's custom resource for one Pulumi program and its state. A preview `Stack` runs `pulumi preview` for a PR and is deleted afterwards.
- **Blueprint**: a catalog entry (`catalog.yaml` plus a Pulumi program) that the UI renders as a form, for example `kind-cluster`.

## Why is the footprint so large for a homelab?

The local setup needs about 12 GB of free memory; the management node alone peaks at about 7 GiB ([measurements](https://github.com/rdylanwalker/keel#requirements)). Keel installs a full control plane: Flux, PKO, S3 storage for Pulumi state, Kyverno policies, cert-manager, Envoy Gateway, Prometheus and Flagger for canaries, an identity provider and, optionally, Forgejo. In v0.12.0 the only optional parts are the identity provider (`--oidc-provider none`, or bring your own with `--oidc-issuer`) and Forgejo (`--with-forge`). There is no slim mode yet.

## Is it secure? It holds my forge tokens.

The design goals and their limits are documented in [Security, Tenancy & Canary](../operations/security-and-tenancy.md), including a list of [residual exposure](../operations/security-and-tenancy.md#residual-exposure). In short: `keel-server` stores credentials only in Kubernetes Secrets, tenant secrets in Git are SOPS/`age` encrypted, and tenants get read-only Git credentials and scoped S3 identities.

One important caveat: the Quick Start's `--local-dev-docker-socket-exception` flag exposes the host Docker socket to PKO workspace pods without authentication, which is root on the host and breaks tenant isolation. Use it only on throwaway machines. Report vulnerabilities privately as described in [`SECURITY.md`](https://github.com/rdylanwalker/keel/blob/main/SECURITY.md).

## Does Keel phone home?

No. Keel has no telemetry or analytics. It talks to your cluster, your forge and your identity provider, and during installation the cluster pulls charts, images and providers from their upstream registries.

## Does it work on k3s, Talos, my existing cluster or Proxmox?

Experimentally. `--target kubeconfig` attaches to an existing cluster (k3s, k3d, Talos, managed Kubernetes); it is implemented but not covered by E2E. `--target proxmox` and `--target hetzner` only create the VM; install k3s yourself and then attach with `--target kubeconfig`. The cloud targets (`aws`, `gcp`, `azure`) are untested. Reports are welcome through the [Bootstrap Friction issue form](https://github.com/rdylanwalker/keel/issues/new/choose).

## Can I upgrade between releases?

Only between releases that are not marked breaking. A breaking release (a `!` or `BREAKING CHANGE` entry in the changelog) means a fresh cluster and repository. The in-cluster Forgejo is also ephemeral by default, so mirror any repository you want to keep.

## Why one binary with an embedded web UI?

The same `keel` binary is the CLI, the API server and the web UI, so there is one thing to download, version and verify (release archives are checksummed and Sigstore-signed). `keel serve` runs on your laptop against the cluster, or in the cluster as a [Flux-managed add-on](../operations/in-cluster-server.md). It keeps no database: state lives in Git, the forge and Kubernetes.

## What license is it under?

MIT; see [`LICENSE`](https://github.com/rdylanwalker/keel/blob/main/LICENSE).

## How do I give feedback?

Open an issue with one of the [issue forms](https://github.com/rdylanwalker/keel/issues/new/choose): bug report, bootstrap friction, provider feedback or feature request. Reports from experimental targets are especially useful.
