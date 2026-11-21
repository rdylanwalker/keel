# Stage 0 Bootstrap

Stage 0 initializes the seed management cluster and GitOps repository layout (`clusters/management/`, `providers/registry.yaml`, `catalog/`, and `tenants/default/`).

All Stage 0 targets produce the same seed control-plane layout (Flux CD, PKO v2, S3 state backend, SOPS/`age`, Kyverno, cert-manager, Envoy Gateway, Prometheus, Flagger, and the in-cluster Rauthy IdP unless you pass `--oidc-issuer` or `--oidc-provider none`) regardless of where the management cluster runs. After Stage 0, Flux adopts these components as protected management add-ons under `clusters/management/core-addons/`, so you upgrade and roll them back through Git.

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

#### Local `k3d` (`k3s`) Example

Disable `k3d`'s bundled Traefik ingress so Envoy Gateway (`keel-ingress`) can bind ports cleanly, and expose `:8888` for Rauthy OIDC:

```bash
k3d cluster create keel-mgmt \
  --k3s-arg "--disable=traefik@server:*" \
  -p "8888:8888@loadbalancer" \
  --wait

keel bootstrap init \
  --target kubeconfig \
  --cluster-name keel-mgmt \
  --context k3d-keel-mgmt \
  --with-forge \
  --repo-dir ./keel-gitops
```

> **Talos Note:** Talos does not ship a default `StorageClass` out of the box—install a provisioner such as `local-path-provisioner` (marked as default `StorageClass`) before running `keel bootstrap init --target kubeconfig` so SeaweedFS and Rauthy PVCs can bind. Also note that `talosctl cluster create docker` requires nested cgroup delegation not available under WSL2 cgroups v2 (use `talosctl cluster create qemu` or a VM/bare-metal Talos node).

### Public Cloud Management Clusters (AWS, Azure, GCP)

Stage 0 can bootstrap directly onto public cloud providers executing provider Pulumi programs (`deploy/bootstrap/<provider>/Pulumi.yaml`). All templates are pre-configured with free-tier-eligible instance types and volume sizes:

#### 1. Amazon Web Services (`--target aws`)
Uses `deploy/bootstrap/aws/Pulumi.yaml` (`pulumi/aws`) with free-tier `t3.micro` / `t2.micro` EC2 instance, 30 GB gp3 root disk, and versioned S3 Pulumi state bucket.

```bash
# Acquire credentials via aws iam (or AWS Console)
export AWS_ACCESS_KEY_ID="AKIAIOSFODNN7EXAMPLE"
export AWS_SECRET_ACCESS_KEY="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
export AWS_REGION="us-east-1"

# Initialize Stage 0 AWS seed management cluster
keel bootstrap init \
  --target aws \
  --cluster-name keel-mgmt \
  --region us-east-1 \
  --repo-dir ./keel-gitops
```

#### 2. Microsoft Azure (`--target azure`)
Uses `deploy/bootstrap/azure/Pulumi.yaml` (`pulumi/azure-native`) with free-tier `Standard_B1s` VM, 30 GB OS disk, protected Resource Group, and Blob Storage state container.

```bash
# Acquire credentials via az ad sp create-for-rbac (or Azure Portal)
export ARM_CLIENT_ID="<client_id>"
export ARM_CLIENT_SECRET="<client_secret>"
export ARM_TENANT_ID="<tenant_id>"
export ARM_SUBSCRIPTION_ID="<subscription_id>"
export AZURE_LOCATION="eastus"

# Initialize Stage 0 Azure seed management cluster
keel bootstrap init \
  --target azure \
  --cluster-name keel-mgmt \
  --region eastus \
  --repo-dir ./keel-gitops
```

#### 3. Google Cloud Platform (`--target gcp`)
Uses `deploy/bootstrap/gcp/Pulumi.yaml` (`pulumi/gcp`) with Always-Free `e2-micro` VM in `us-central1-a`, 10 GB `pd-standard` boot disk, and versioned GCS state bucket.

```bash
# Acquire credentials via gcloud iam service-accounts (or GCP Console)
export GOOGLE_CREDENTIALS="$(cat ./gcp-key.json)" # or GOOGLE_APPLICATION_CREDENTIALS
export GCP_PROJECT="my-project-id"
export GCP_ZONE="us-central1-a"

# Initialize Stage 0 GCP seed management cluster
keel bootstrap init \
  --target gcp \
  --cluster-name keel-mgmt \
  --region us-central1 \
  --repo-dir ./keel-gitops
```

> **Testing Without In-Cluster Controllers (`--skip-core-install`):** To validate cloud infrastructure provisioning without deploying Kubernetes controllers onto the newly booted VM, append `--skip-core-install`.

#### k3s Packaged Components

The AWS, Azure, and GCP programs install single-node `k3s` on first boot with `INSTALL_K3S_EXEC="server --disable=traefik ..."`:

| k3s packaged component | Status | Owner |
| --- | --- | --- |
| Traefik ingress | **Disabled** (`--disable=traefik`) | — Envoy Gateway (`keel-ingress`) is the only ingress/Gateway controller, so the two never compete for ports `80`/`443` or `LoadBalancer` addresses |
| ServiceLB (`klipper-lb`) | Enabled | k3s — exposes the Envoy Gateway `LoadBalancer` Service on the node IP |
| `local-path` provisioner | Enabled | k3s — default `StorageClass` for SeaweedFS, Rauthy, and Forgejo PVCs |
| `metrics-server` | Enabled | k3s — resource metrics API |
| CoreDNS | Enabled | k3s — Stage 0 adds Rauthy/Forgejo host rewrites |

These k3s-owned components are reconciled by k3s from `/var/lib/rancher/k3s/server/manifests`, not by Flux, and are not Keel catalog add-ons. Do not delete them, and do not install a second ingress controller on the management cluster.

`--target hetzner` and `--target proxmox` only provision the protected VM; they do not install `k3s`. Install your distribution of choice on the node yourself (for `k3s`, pass `--disable=traefik`, or `--k3s-arg "--disable=traefik@server:*"` for `k3d`). Then attach it with `--target kubeconfig`.

> **Existing clusters:** the install script runs only on first boot, and the node resources ignore later changes to it (`ignoreChanges`), so `keel bootstrap recover` never restarts or replaces a protected node because of a script edit. Nodes created before Traefik was disabled keep it until you migrate them by hand. Add `disable: [traefik]` to `/etc/rancher/k3s/config.yaml` and run `systemctl restart k3s`. If `kubectl -n kube-system get helmchart traefik traefik-crd` still returns either chart, delete it.

### Core Controllers Installed Out of the Box

By default, `keel bootstrap init` installs the core control-plane components with protected Stage 0 Pulumi programs. The Helm charts come from `deploy/bootstrap/control-plane/Pulumi.yaml` (stack `stage0-control-plane-<cluster>`), and in-cluster Rauthy comes from `deploy/bootstrap/control-plane-rauthy/Pulumi.yaml`. Raw seed manifests are applied with `kubectl`. After Stage 0, the protected HelmReleases and Kustomizations in `clusters/management/core-addons/` adopt each release in place, reconciled by the `flux-system/core-addons` Kustomization. From then on Flux owns these components, and you upgrade or roll back each one as a management add-on PR (`catalog/core/<id>`):

- **Namespaces:** `flux-system`, `keel-system`, `keel-previews`
- **SeaweedFS (`s3-state-backend`, chart `4.54.0`):** In-cluster S3-compatible state backend in `keel-system`, with the versioned `pulumi-state` bucket. Tenants get scoped identities limited to `tenants/<id>/*`, never the admin keys.
- **Flux CD (`flux2`, chart `2.20.0`):** GitOps source, kustomize, and helm controllers in `flux-system`, with the multitenancy lockdown enabled (`--no-cross-namespace-refs=true`, default ServiceAccount impersonation). After Stage 0, the protected `flux-system/flux2` HelmRelease in `clusters/management/core-addons/flux2.yaml` adopts this release, so Flux upgrades and rolls back itself from Git
- **Kyverno (chart `3.10.0`):** A single admission controller release in `keel-system`, plus Keel's seed policies (including `local-kind-docker-socket-exception` and `keel-docker-proxy` in `flux-system` when `--local-dev-docker-socket-exception` is passed)
- **Pulumi Kubernetes Operator (PKO v2, chart `2.10.1`):** Infrastructure operator in `keel-system` plus the `management-cluster-core` `Stack`
- **Envoy Gateway (`envoy-gateway`):** Gateway API controller in `keel-system` with `GatewayClass` `keel-envoy` and `Gateway` `keel-ingress` (HTTP on port `80`; HTTPS on `8888` and `443` using `keel-system/keel-wildcard-tls`)
- **Prometheus (`prometheus`) & Flagger (`flagger`):** Telemetry (`http://prometheus.keel-system:9090`) and progressive canary delivery controller (`meshProvider: gatewayapi:v1`) in `keel-system`
- **cert-manager (`cert-manager`, chart `v1.21.3`):** Certificate controller in `keel-system` that issues the self-signed control-plane CA (`keel-ca-issuer`) and the `keel-system/keel-wildcard-tls` certificate used by the `keel-ingress` Gateway
- **Rauthy (`rauthy`):** Lightweight Rust in-cluster OIDC identity provider in `keel-system` with built-in Admin UI (`https://idp.keel.local:8888/auth/v1/admin`) seeded with `keel-system/rauthy-config`, `keel-admins` + `tenant-default` groups, `keel-server` PKCE client, and initial admin credentials (configured with `restricted` PSS `securityContext`; skipped if `--oidc-issuer` or `--oidc-provider none` is supplied). After Stage 0, the protected Flux Kustomization `flux-system/rauthy` (`clusters/management/core-addons/rauthy.yaml`) adopts it from `clusters/management/addons/rauthy/`, with the version pinned by `spec.images[].newTag`; the `keel-system/rauthy-config` Secret stays Stage 0-managed and is never stored in Git

To initialize only the cluster and GitOps repository without installing the core controllers onto the cluster, pass `--skip-core-install`:

```bash
keel bootstrap init \
  --target kind \
  --cluster-name keel-mgmt \
  --skip-core-install \
  --repo-dir ./keel-gitops
```

> **In-Cluster Flux Pull-Polling:** If the GitOps repository is not reachable over the network (no `--with-forge`, and no `--git-url` with an `http(s)://`, `ssh://` or `git://` URL), cluster pods cannot clone it. `keel bootstrap init` then skips the in-cluster Flux sync manifests (`GitRepository` and root `Kustomization`s), and the control-plane add-ons stay Stage 0-only: no Flux reconciliation, and no management API upgrades or rollbacks. Re-running `keel bootstrap init`/`recover` with a newer CLI does not apply newer add-on versions either. Bootstrap prints a note. To hand the add-ons to Flux, push the repository to a network Git remote and re-run with `--git-url`, or use `--with-forge`.

### Optional Add-ons, OIDC & Git Remotes

- `--with-forge` — Installs an in-cluster Forgejo instance in `keel-system` and points Flux + `.keel/config.yaml` at it. Forgejo then becomes a Flux-managed management add-on (`clusters/management/core-addons/forgejo.yaml`, `catalog/core/forgejo`). Its data is ephemeral by default (persistence disabled): repositories are lost whenever the Forgejo pod is replaced, so the management API refuses Forgejo upgrades and rollbacks until you back up the repository (`forgejo dump` or a pushed mirror) and set `persistence.enabled: true` in `core-addons/forgejo.yaml` directly in Git.
- `--with-keel-server` — Runs `keel-server` inside the cluster as a Flux-managed management add-on (`clusters/management/core-addons/keel-server.yaml`, `clusters/management/addons/keel-server/`, `catalog/core/keel-server`). The image is pinned to the release of the `keel` CLI you bootstrap with. Requires `--repo-dir` plus either `--with-forge` or an HTTP(S) `--git-url` (not both), because the add-on reaches Git through the seed push and the pod clones the GitOps repository on start. See [Running keel-server In-Cluster](../operations/in-cluster-server.md).
- `--git-url <URL>` — Connects an external GitHub, GitLab, or Forgejo/Gitea repository (with `KEEL_GIT_TOKEN` / `--git-token` for private repos and PRs). See [Git Repositories & Forges](git-repositories.md).
- `--admin-username` / `--admin-password` — Customizes the initial admin account seeded into in-cluster Rauthy (`keel-system/rauthy-config`) and added to `keel-admins`. When omitted, `keel bootstrap init` generates a random password and prints it in the bootstrap summary.
- `--oidc-provider <rauthy|none>` — Selects the Stage 0 in-cluster OIDC provider (defaults to `rauthy`; pass `none` to skip in-cluster IdP installation).
- `--oidc-issuer <URL>` / `--oidc-client-id <ID>` — Connects an external OIDC provider instead of installing in-cluster Rauthy.
- `--oidc-public-url <URL>`: the public HTTPS URL of in-cluster Rauthy. The default is `https://idp.keel.local:8888`. The OIDC issuer becomes `<url>/auth/v1`, and the Forgejo and portal hosts are derived from the parent domain (`forgejo.<domain>`, `keel.<domain>`). Ingress uses a self-signed CA (`keel-system/keel-wildcard-tls`), which `keel serve` trusts automatically. Your browser will warn about it unless you import that CA.
- `KEEL_TENANT_GIT_TOKEN` (environment): a read-only token for `tenant-default`'s Flux `GitRepository` when you use an external forge. The operator `--git-token` is never shared with tenants.
- `KEEL_FORGEJO_ADMIN_PASSWORD` (environment): overrides the generated Forgejo admin password, which is stored in `keel-system/keel-forgejo-admin`.


### Stage 0 State, Passphrase & Escrow

Stage 0 is a set of Pulumi stacks named `stage0-<program>-<cluster>`, for example `stage0-kind-keel-mgmt` and `stage0-control-plane-keel-mgmt`. Their state is durable and kept per cluster:

- `~/.config/keel/stage0/<cluster>/state/`: the local Pulumi backend.
- `~/.config/keel/stage0/<cluster>/programs/<program>/`: the materialized programs and encrypted stack config.
- `~/.config/keel/stage0/<cluster>/pulumi-passphrase`: a generated passphrase (`0600`). It is also stored in `keel-system/keel-stage0-pulumi-passphrase`.

The root directory is `$KEEL_CONFIG_DIR`, or `$XDG_CONFIG_HOME/keel`, or `~/.config/keel`. Set `PULUMI_CONFIG_PASSPHRASE` to supply your own passphrase.

When the in-cluster SeaweedFS is used, Stage 0 also escrows every stack to `s3://pulumi-state/stage0/<cluster>/`. It turns on bucket versioning, or falls back to timestamped snapshots if the backend rejects versioning. The first run prints a one-time notice: **copy the passphrase into your vault.** Without it, the S3 escrow can't be decrypted if both this workstation and the cluster are lost. See [Break-Glass Recovery](../operations/break-glass-recovery.md).

Stage 0 core resources carry `protect: true`. To tear the stack down deliberately with Pulumi, you first need `pulumi state unprotect --all`. Deleting the cluster skips this.

## 2. Configure Local DNS

The Stage 0 ingress (Envoy Gateway `keel-ingress`, port `8888`) serves Rauthy and, with `--with-forge`, Forgejo over HTTPS. Add both hosts to `/etc/hosts`, or `C:\Windows\System32\drivers\etc\hosts` on Windows:

```text
127.0.0.1 idp.keel.local forgejo.keel.local
```

The `forgejo.keel.local` entry is needed so PR links printed by the CLI and shown in the GUI resolve. Inside `kind`, Stage 0 adds CoreDNS rewrites so pods reach the same hostnames.

## 3. Start the Control Plane Server

```bash
keel serve --repo-dir ./keel-gitops    # binds 127.0.0.1:8080 by default
```

When a Kubernetes cluster is reachable (through `KUBECONFIG` or in-cluster config), `keel serve`:

- overlays live **Flux CD** (`Kustomization`, `HelmRelease`) and **PKO v2** (`Stack`) conditions as `Synced`, `Ready`, `Reconciling`, or `Error`. It shows `Pending` while it is offline.
- opens background `kubectl port-forward` tunnels to **Forgejo** (`--with-forge`, `127.0.0.1:3000` or an ephemeral port) and **Envoy Gateway** (`keel-ingress`, `127.0.0.1:8888` for `https://idp.keel.local:8888`) when it runs on localhost.
- trusts the Stage 0 ingress CA from `keel-system/keel-wildcard-tls` for its OIDC calls.
- re-applies the Kyverno `restrict-pulumi-providers` policy from `providers/registry.yaml`. Pulumi's built-in providers, including `kubernetes`, are always allowed.
- starts the preview reconciler, which garbage-collects previews of merged or closed PRs every 60 s.

To run the server inside the cluster instead, see [Running keel-server In-Cluster](../operations/in-cluster-server.md).

### Development Shortcuts (`just`)

- `just serve`: starts `keel serve` against a clean temporary local Git repository.
- `just serve-demo`: rebuilds the UI with `VITE_ENABLE_DEMO=true` and starts `keel serve` with `deploy/catalog/` pre-seeded. Open `http://127.0.0.1:8080/?demo=true` for an unauthenticated preview with a **DEMO DATA** banner. Run `just build-ui` afterwards to restore a production bundle. Production bundles ignore `?demo=true`.

## 4. Authenticate

### Web UI

Open `http://127.0.0.1:8080`, click **Sign in with OIDC**, and sign in with the Rauthy admin username and password printed by `keel bootstrap init`, or with your external OIDC account. Manage users and groups in the Rauthy Admin UI (`https://idp.keel.local:8888/auth/v1/admin`), which is linked from the `rauthy` card in **Topology Map → Deployed Application Releases → Management Plane**.

Tenant access comes from IdP groups named `tenant-<id>` (member) and `tenant-<id>-admin` (admin). Cluster admin comes from `keel-admins`. See [Security & Tenancy](../operations/security-and-tenancy.md#identity--authorization).

### CLI

`keel auth login` uses the RFC 8252 loopback flow. The CLI listens on `127.0.0.1:<random>`, opens your browser, and stores a 1-hour Bearer token (`0600`) in the Keel config directory:

```bash
keel auth login --server http://127.0.0.1:8080   # --no-browser prints the URL instead
keel auth status
export KEEL_SERVER=http://127.0.0.1:8080          # then omit --server
```

## 5. WSL2 + Windows Browser Notes

When `kind` and `keel serve` run inside **WSL2** and you open the GUI in a **Windows** browser:

- Open `http://localhost:8080` or `http://127.0.0.1:8080`, and keep using whichever host you started on. `keel-server` derives the OIDC `redirect_uri` from the request `Host` and sets `keel_pkce` with `SameSite=Lax` (without `Secure` on loopback HTTP), so the PKCE cookie survives the redirect back from Rauthy.
- Add `127.0.0.1 idp.keel.local forgejo.keel.local` to `C:\Windows\System32\drivers\etc\hosts` from an Administrator shell.
- **Forgejo access:** **View PR on Forge** opens Forgejo through the port-forward. When both `--with-forge` and in-cluster Rauthy are used, Stage 0 registers a Forgejo OAuth source named `rauthy`, so **Sign in with rauthy** works at `https://forgejo.keel.local:8888`. You can also sign in as the Stage 0 admin user `keel`:
  ```bash
  kubectl -n keel-system get secret keel-forgejo-admin -o jsonpath='{.data.password}' | base64 -d
  ```
- The Stage 0 Forgejo release uses `persistence.enabled=false` (ephemeral SQLite). Restarting the Forgejo pod resets its database.

## 6. Break-Glass Recovery

If the management cluster's add-ons or the Git forge are broken, re-apply the escrowed Stage 0 state directly from S3:

```bash
kubectl -n keel-system port-forward svc/s3-state-backend-seaweedfs-s3 8333:8333 &
export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...   # from keel-system/s3-state-backend-seaweedfs-s3-secret
keel bootstrap recover --s3-endpoint http://127.0.0.1:8333 --cluster-name keel-mgmt --dry-run
keel bootstrap recover --s3-endpoint http://127.0.0.1:8333 --cluster-name keel-mgmt
```

The full runbook is in [Break-Glass Recovery](../operations/break-glass-recovery.md).
