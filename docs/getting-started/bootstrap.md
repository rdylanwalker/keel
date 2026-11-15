# Stage 0 Bootstrap

Stage 0 initializes the seed management cluster and GitOps repository layout (`clusters/management/`, `providers/registry.yaml`, `catalog/`, and `tenants/default/`).

All Stage 0 targets produce the same seed control-plane layout (Flux CD, PKO v2, S3 state backend, SOPS/`age`, Kyverno, Envoy Gateway, and Flagger) regardless of where the management cluster runs.

## Supported Targets

| Target | Flag | Use Case |
| :--- | :--- | :--- |
| Local `kind` | `--target kind` | Local development on Docker |
| Existing cluster | `--target kubeconfig` | Existing `k3s`, `Talos`, or managed Kubernetes cluster |
| Proxmox VE | `--target proxmox` | On-premises hypervisor cluster |
| AWS / GCP / Azure / Hetzner | `--target aws\|gcp\|azure\|hetzner` | Public cloud management cluster |

## 1. Initialize the Management Cluster

### Local `kind` Management Cluster

Pass `--local-dev-docker-socket-exception` on local `kind` clusters so Stage 0 mounts `/var/run/docker.sock` into the management node, deploys `keel-docker-proxy` in `flux-system`, and injects `DOCKER_HOST` into `kind` PKO v2 workspace pods (via an explicit Kyverno `PolicyException`) while keeping tenant namespaces `restricted` PSS compliant:

```bash
keel bootstrap init \
  --target kind \
  --cluster-name keel-mgmt \
  --local-dev-docker-socket-exception \
  --repo-dir ./keel-gitops
```

### Existing Cluster (`kubeconfig`)

```bash
keel bootstrap init \
  --target kubeconfig \
  --kubeconfig ~/.kube/config \
  --context my-mgmt-cluster \
  --repo-dir ./keel-gitops
```

### Core Controllers Installed Out of the Box

By default, `keel bootstrap init` installs the core control-plane components onto the target cluster using `kubectl` and `helm`:

- **Namespaces:** `flux-system`, `keel-system`, `keel-previews`
- **Flux CD (`flux2`):** GitOps source, kustomize, and helm controllers in `flux-system`
- **Kyverno:** Admission controller in `kyverno` plus Keel's seed policies (including the Docker socket `PolicyException` when `--local-dev-docker-socket-exception` is passed)
- **Pulumi Kubernetes Operator (PKO v2):** Infrastructure operator in `keel-system` plus the `management-cluster-core` `Stack`

To initialize only the cluster and GitOps repository without installing the core controllers onto the cluster, pass `--skip-core-install`:

```bash
keel bootstrap init \
  --target kind \
  --cluster-name keel-mgmt \
  --skip-core-install \
  --repo-dir ./keel-gitops
```

> **In-Cluster Flux Pull-Polling:** If `--repo-dir` points to a local filesystem path without a reachable HTTP/SSH Git remote (such as `--with-forge` or an external Forgejo/GitHub/GitLab repository), `keel bootstrap init` skips applying the in-cluster `GitRepository` resource because cluster pods cannot clone a host filesystem path.

### Optional Stage 1 Add-ons & Git Remotes

- `--with-forge` — Installs an in-cluster Forgejo instance in `keel-system` and points Flux + `.keel/config.yaml` at it.
- `--git-url <URL>` — Connects an external GitHub, GitLab, or Forgejo/Gitea repository (with `KEEL_GIT_TOKEN` / `--git-token` for private repos and PRs). See [Git Repositories & Forges](git-repositories.md).
- `--with-oidc` — Seeds an in-cluster OIDC provider (Pocket ID / Authentik).

## 2. Start the Control Plane Server

```bash
keel serve --bind 127.0.0.1:8080 --repo-dir ./keel-gitops
```

When a Kubernetes cluster is reachable via `KUBECONFIG` (or in-cluster config), `keel serve` automatically queries live **Flux CD** (`Kustomization`, `HelmRelease`) and **PKO v2** (`Stack`) CRD conditions to display `Synced`, `Ready`, `Reconciling`, or `Error` statuses in the UI and CLI. If no cluster is reachable or a resource has not been reconciled yet, it falls back to `Pending`.

On startup in Stage 0 mode, `keel serve` prints a **single-use 256-bit bootstrap token** to stdout:

```text
Keel Control Plane listening on http://127.0.0.1:8080
Stage 0 Bootstrap Token: <64-character-hex-token>
```

> **Note — Single-use token:** Only the SHA-256 hash of the token is stored, and the token is invalidated immediately after its first successful login. Restart `keel serve` if you need a fresh token during local Stage 0 development.

### Development Shortcuts (`just`)

- `just serve` — Starts `keel serve` with a clean temporary local Git repository.
- `just serve-demo` — Starts `keel serve` pre-seeded with `deploy/catalog/` blueprints and providers.

## 3. Authenticate

### Web UI

1. Open `http://127.0.0.1:8080`.
2. Click **Bootstrap Token Login**.
3. Paste the 64-character token from stdout.

### CLI

When OIDC is configured (Stage 1+), authenticate the CLI via OIDC Authorization Code + PKCE:

```bash
keel auth login --server http://127.0.0.1:8080
```

For Stage 0 local development before OIDC is enabled, use **Bootstrap Token Login** in the Web UI (or exchange the token via `POST /api/v1/auth/bootstrap`).

## 4. Break-Glass Recovery

If the management cluster or Git forge becomes unreachable, recover the control-plane stack directly from versioned S3 state:

```bash
keel bootstrap recover \
  --s3-endpoint https://s3.infra.local \
  --bucket pulumi-state \
  --stack management-cluster-core
```
