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
- **SeaweedFS (`s3-state-backend`):** In-cluster S3-compatible state backend in `keel-system` with the `pulumi-state` bucket and synced `keel-s3-credentials` Secret
- **Flux CD (`flux2`):** GitOps source, kustomize, and helm controllers in `flux-system`
- **Kyverno:** Admission controller in `kyverno` plus Keel's seed policies (including `local-kind-docker-socket-exception` and `keel-docker-proxy` in `flux-system` when `--local-dev-docker-socket-exception` is passed)
- **Pulumi Kubernetes Operator (PKO v2):** Infrastructure operator in `keel-system` plus the `management-cluster-core` `Stack`
- **Envoy Gateway (`envoy-gateway`):** Gateway API controller in `keel-system` with `GatewayClass` `keel-envoy` and `Gateway` `keel-ingress` (HTTP ports `80` and `8888`)
- **Prometheus (`prometheus`) & Flagger (`flagger`):** Telemetry (`http://prometheus.keel-system:9090`) and progressive canary delivery controller (`meshProvider: gatewayapi:v1`) in `keel-system`
- **Dex (`dex`):** In-cluster OIDC identity provider in `keel-system` seeded with `keel-system/dex-config` and initial admin credentials (configured with `restricted` PSS `securityContext` settings for Kyverno compliance; skipped if `--oidc-issuer` is supplied for an external OIDC provider)

To initialize only the cluster and GitOps repository without installing the core controllers onto the cluster, pass `--skip-core-install`:

```bash
keel bootstrap init \
  --target kind \
  --cluster-name keel-mgmt \
  --skip-core-install \
  --repo-dir ./keel-gitops
```

> **In-Cluster Flux Pull-Polling:** If `--repo-dir` points to a local filesystem path without a reachable HTTP/SSH Git remote (such as `--with-forge` or an external Forgejo/GitHub/GitLab repository), `keel bootstrap init` skips applying the in-cluster `GitRepository` resource because cluster pods cannot clone a host filesystem path.

### Optional Add-ons, OIDC & Git Remotes

- `--with-forge` — Installs an in-cluster Forgejo instance in `keel-system` and points Flux + `.keel/config.yaml` at it.
- `--git-url <URL>` — Connects an external GitHub, GitLab, or Forgejo/Gitea repository (with `KEEL_GIT_TOKEN` / `--git-token` for private repos and PRs). See [Git Repositories & Forges](git-repositories.md).
- `--admin-username` / `--admin-password` — Customizes the initial admin account seeded into in-cluster Dex (`keel-system/dex-config`). When omitted, `keel bootstrap init` generates a random password and prints it in the bootstrap summary.
- `--oidc-issuer <URL>` / `--oidc-client-id <ID>` — Connects an external OIDC provider instead of installing in-cluster Dex.

## 2. Start the Control Plane Server

```bash
keel serve --bind 127.0.0.1:8080 --repo-dir ./keel-gitops
```

When a Kubernetes cluster is reachable via `KUBECONFIG` (or in-cluster config), `keel serve` automatically:
- Queries live **Flux CD** (`Kustomization`, `HelmRelease`) and **PKO v2** (`Stack`) CRD conditions to display `Synced`, `Ready`, `Reconciling`, or `Error` statuses in the UI and CLI (falling back to `Pending` when offline).
- Opens background `kubectl port-forward` tunnels to in-cluster **Forgejo** (`--with-forge`, preferring `127.0.0.1:3000` or falling back to an ephemeral port) and **Dex** (preferring `127.0.0.1:5556`) when running on `localhost`.
- Rewrites Forgejo PR `html_url` links (**Forge Link** / **View PR on Forge**) to the active local port-forwarded URL and rewrites Dex discovery endpoints to `http://127.0.0.1:<port>/dex` so host browsers can complete OIDC login without `/etc/hosts` changes.

When running offline without OIDC configured (e.g. `just serve`), `keel serve` starts in Stage 0 Bootstrap Token mode and prints a **single-use 256-bit bootstrap token** to stdout:

```text
Keel Control Plane listening on http://127.0.0.1:8080
Stage 0 Bootstrap Token: <64-character-hex-token>
```

> **Note — Single-use token:** Only the SHA-256 hash of the token is stored, and the token is invalidated immediately after its first successful login. Restart `keel serve` if you need a fresh token during offline Stage 0 development.

### Development Shortcuts (`just`)

- `just serve` — Starts `keel serve` with a clean temporary local Git repository.
- `just serve-demo` — Starts `keel serve` pre-seeded with `deploy/catalog/` blueprints and providers.

## 3. Authenticate

### Web UI

- **With OIDC (default after `keel bootstrap init`):** Open `http://127.0.0.1:8080`, click **Sign in with OIDC**, and log in with the Dex admin username/password printed by `keel bootstrap init` (or your external OIDC account).
- **Without OIDC (offline / `just serve`):** Open `http://127.0.0.1:8080`, click **Bootstrap Token Login**, and paste the 64-character token from stdout.

### CLI

Authenticate the CLI via OIDC Authorization Code + PKCE:

```bash
keel auth login --server http://127.0.0.1:8080
```

For offline local development without OIDC, use **Bootstrap Token Login** in the Web UI (or exchange the token via `POST /api/v1/auth/bootstrap`).

## 4. WSL2 + Windows Browser Notes

When running `kind` and `keel serve` inside **WSL2** while opening the GUI in a **Windows** browser:

- **Keel UI & Forge Links Work Without `hosts` Edits:** Open `http://localhost:8080` or `http://127.0.0.1:8080` in Windows. `keel-server` derives the OIDC `redirect_uri` from the incoming `Host` header and sets `keel_pkce` with `SameSite=Lax` (and omits `Secure` on loopback HTTP) so the PKCE cookie survives the redirect back from Dex. Stick to the same host (`localhost` or `127.0.0.1`) you started on.
- **Direct Forgejo Access:** Clicking **Forge Link** / **View PR on Forge** opens Forgejo on `127.0.0.1:<port>` (forwarded automatically by `keel serve`). You can sign in directly with the Stage 0 Forgejo admin credentials (`keel` / `keel-admin-dev`).
- **Optional — Forgejo OIDC SSO (`forgejo.keel.local:8888`):** Forgejo's `openidConnect` integration (`go-oidc`) requires a single OIDC `issuer` URL reachable by **both** your Windows browser and the Forgejo pod inside `kind`. To enable **Sign in with dex** on Forgejo:
  1. Port-forward the Envoy Gateway service in `keel-system` to `8888`:
     ```bash
     kubectl -n keel-system port-forward svc/<envoy-keel-system-keel-ingress-svc> 8888:8888
     ```
  2. Add CoreDNS `rewrite name` entries inside `kind` (`kube-system/coredns`) pointing `dex.keel.local` and `forgejo.keel.local` to `<envoy-svc>.keel-system.svc.cluster.local`.
  3. Add the hostnames to the **Windows** `hosts` file from an **Administrator PowerShell**:
     ```powershell
     Add-Content -Path C:\Windows\System32\drivers\etc\hosts -Value "`n127.0.0.1 dex.keel.local forgejo.keel.local"
     ```
  4. Configure Dex (`keel-system/dex-config`) with `issuer: http://dex.keel.local:8888/dex` and a `forgejo` client (`redirectURIs: http://forgejo.keel.local:8888/user/oauth2/dex/callback`), set `gitea.config.server.ROOT_URL=http://forgejo.keel.local:8888/` on the `forgejo` Helm release, and register the provider in Forgejo (`forgejo admin auth add-oauth --name dex --provider openidConnect ...`).
  5. Note that the Stage 0 Forgejo Helm release uses `persistence.enabled=false` (ephemeral SQLite), so upgrading or restarting the Forgejo pod resets its database and requires re-registering the OAuth source and `keel/infra-gitops` repository.

## 5. Break-Glass Recovery

If the management cluster or Git forge becomes unreachable, recover the control-plane stack directly from versioned S3 state:

```bash
keel bootstrap recover \
  --s3-endpoint https://s3.infra.local \
  --bucket pulumi-state \
  --stack management-cluster-core
```
