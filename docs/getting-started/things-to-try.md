# Things to Try

These hands-on walkthroughs assume you have completed the [Stage 0 Bootstrap](bootstrap.md) quick-start and have:

1. A local `kind` management cluster running (bootstrapped with `--target kind --local-dev-docker-socket-exception`).
2. `keel serve` running on `http://127.0.0.1:8080`.
3. An authenticated Web UI or CLI session (`keel auth login`).

> **New to Keel?** Follow the [Stage 0 Bootstrap](bootstrap.md) guide first to initialize your local `kind` management cluster and sign in via OIDC.

---

## 1. Provision a Second `kind` Workload Cluster

Because the `kind` provider is enabled by default in `providers/registry.yaml`, you can spin up a second isolated Kubernetes cluster on Docker right away.

### Step 1 (Optional): Create a Tenant

The walkthroughs on this page use the Stage 0 `default` tenant. Every tenant, `default` included, has its own Git repository and Flux source, so the walkthroughs work the same in a tenant you create.

1. Open `http://127.0.0.1:8080` and go to **Catalog & Form Wizard**.
2. Click **+ New Tenant**, enter `dev-team`, and click **Create Tenant PR**. The dialog shows a provisioning summary with the new repository and two pull requests.
3. Merge them in the listed order from **PR & Preview Inspector** (**Go to PR**): first the skeleton PR in the tenant repository, then the platform stub PR (`tenants/dev-team/`). Click **Done**.
4. In the Rauthy Admin UI (`https://idp.keel.local:8888/auth/v1/admin`), create the groups `tenant-dev-team` and `tenant-dev-team-admin` and add your users to them. Keel honors only `tenant-<id>` and `tenant-<id>-admin` group names. The Stage 0 admin in `keel-admins` already has access to every tenant.

### Step 2: Submit the `kind-cluster` Blueprint

In **Catalog & Form Wizard**:

1. Set **Target Tenant** to `default` (or `dev-team` if you created it in Step 1; use `--tenant dev-team` in the CLI below).
2. Select **Local kind Workload Cluster** (`kind-cluster`).
3. Fill in the form:
   - **Cluster Name:** `workload-dev`
   - **Kubernetes Version:** `v1.35.8`
   - **Worker Node Count:** `1`
4. Click **Submit & Open Git PR Proposal**.

Or submit the same proposal via CLI:

```bash
cat <<'EOF' > /tmp/workload-dev.yaml
clusterName: workload-dev
kubernetesVersion: v1.35.8
workerCount: 1
EOF

keel infra provision \
  --blueprint kind-cluster \
  --tenant default \
  --values-file /tmp/workload-dev.yaml \
  --server http://127.0.0.1:8080
```

### Step 3: Inspect the Preview & Merge

1. Open **PR & Preview Inspector** (or run `keel preview status --pr <PR_NUMBER> --tenant <TENANT> --watch --server http://127.0.0.1:8080`).
2. Review the **Git Diff (vs. main)** (`tenants/default/infra/workload-dev.yaml`) and the **Resource Diff Tree** from the PKO v2 preview (`+ Create` for `kind:index:Cluster`).
3. Click **Merge PR**.
4. Once Flux CD and PKO v2 reconcile, verify the new cluster is running alongside your management cluster:

```bash
kind get clusters
```

---

## 2. Deploy an Application with Canary Rollouts (`podinfo`)

With a tenant namespace ready (such as `default` or `dev-team`), deploy **Podinfo** (`podinfo`) using an upstream Helm chart with Flagger + Envoy Gateway progressive delivery enabled.

> **v0.12.0 bug:** a canary app in a tenant namespace on the management cluster fails because the tenant ServiceAccount cannot manage Flagger `Canary` objects. Apply the [RBAC workaround](../reference/known-limitations.md#canary-apps-on-the-management-cluster) before this walkthrough. Add `127.0.0.1 podinfo.keel.local` to your `hosts` file as well.

### Step 1: Initial Canary Deployment

If you followed step 7 of the [README Quick Start](https://github.com/rdylanwalker/keel#quick-start-local-kind), `podinfo` already runs in `default` with a canary: skip to [Step 2](#step-2-demonstrate-a-live-canary-traffic-rollout). Installing it a second time opens a proposal for an app that already exists.

1. In **Catalog & Form Wizard**, select your target tenant (e.g., `default`) and choose **Podinfo Microservice** (`podinfo`).
2. Check **Enable Flagger Canary**.
3. Fill in the form (`Replica Count`: `2`, `Ingress Hostname`: `podinfo.keel.local`, `API Secret Token`: any 8+ character secret, and keep the pre-filled `restricted` PSS `Container Security Context` defaults).
4. Click **Submit & Open Git PR Proposal** (or via CLI). In v0.12.0 the CLI does not fill in the form's defaults, so the values file must include the `restricted` `securityContext`; without it, Kyverno rejects the Deployment. Later releases fill in the defaults on the server, and keeping them in the file does no harm:

```bash
cat <<'EOF' > podinfo-values.yaml
replicaCount: 2
ingressHost: podinfo.keel.local
apiToken: demo-podinfo-token
securityContext:
  allowPrivilegeEscalation: false
  runAsNonRoot: true
  runAsUser: 100
  capabilities:
    drop: [ALL]
  seccompProfile:
    type: RuntimeDefault
EOF

keel app install \
  --app podinfo \
  --tenant default \
  --values-file ./podinfo-values.yaml \
  --canary \
  --server http://127.0.0.1:8080

# Merge with the PR number printed above:
keel proposal merge <PR_NUMBER> --tenant default --server http://127.0.0.1:8080
```

5. Inspect the PR diff in **PR & Preview Inspector**—which synthesizes `helmrelease-canary.yaml`, `canary.yaml`, `networkpolicy-canary.yaml`, and, when the form's `apiToken` secret is set, the SOPS-encrypted `secrets.enc.yaml` Secret (listed in the tenant repository's `tenants/<id>/kustomization.yaml` and merged into the release through `HelmRelease.spec.valuesFrom`)—and click **Merge PR**.
6. In **Topology Map**, wait for `podinfo` to reach `Ready` / `Initialized` in the **Flagger Canary Traffic Rollouts (Envoy Gateway)** panel (about a minute and a half after the merge). If it stays `Pending`, see [Known Limitations](../reference/known-limitations.md#other-app-limitations) for how to read the Flux error.
7. Open `https://podinfo.keel.local:8888` (accept the certificate warning) or run `curl -k https://podinfo.keel.local:8888/version`. The app is served over HTTPS only.

### Step 2: Demonstrate a Live Canary Traffic Rollout

1. Ensure `127.0.0.1 podinfo.keel.local` is in your `hosts` file and `keel serve` is running: it port-forwards Envoy Gateway (`keel-ingress`) to `127.0.0.1:8888`.

2. Start a traffic loop in a terminal so Prometheus records live Envoy Gateway success-rate and latency metrics:

```bash
while true; do
  curl -sk https://podinfo.keel.local:8888/version
  echo ""
  sleep 1
done
```

3. In **Topology Map**, click **Edit** on the `podinfo` release, change **Chart Version** (for example, `6.7.1` → `6.7.0`) or **Replica Count** (`2` → `3`), re-enter **API Secret Token**, and verify **Enable Flagger Canary** remains checked.
4. Submit and merge the PR.
5. Watch the **Flagger Canary Traffic Rollouts (Envoy Gateway)** panel and your `curl` loop. This analysis is not covered by automated tests yet, so please report what you see:
   - Flagger scales up the new canary pods alongside `podinfo-primary`.
   - Every `30s`, Flagger shifts weight (`10% → 20% → 30% → 40% → 50%`) while querying Prometheus (`envoy-gateway-request-success-rate` and `envoy-gateway-request-duration`).
   - Your `curl` output will alternate between the old and new versions in proportion to the canary weight until Flagger promotes the new version to `podinfo-primary` (`Succeeded`).

---

## 3. Provision a Small AWS EKS Cluster

You can use the same management cluster to provision cloud infrastructure. This walkthrough provisions a small **AWS EKS** cluster. It is not covered by automated tests.

> **Cost Note:** This is **not** free tier. The EKS control plane costs about $0.10/hour, plus the `t3.medium` nodes (the blueprint default). Smaller instances such as `t3.micro` can run only a handful of pods per node on EKS. Delete the stack when you finish testing.

### Step 1: Enable the `aws` Provider

1. Open **Control Plane & Providers** in the web portal.
2. Locate the **Amazon Web Services (`aws`)** provider card and click **Enable**.
3. Merge the generated PR updating `providers/registry.yaml`.

### Step 2: Add the `aws-eks` Blueprint to Your Catalog

1. In **Catalog & Form Wizard**, click **+ Add Catalog Item** → **Infrastructure Blueprint (PKO v2)** (or commit directly on a branch).
2. Use the `catalog.yaml` and `Pulumi.yaml` from [Provider Blueprint Examples — AWS EKS](../catalog/provider-examples.md#aws-eks-cluster-aws) under `catalog/infra/aws-eks/`.
3. Merge the catalog PR. **AWS EKS Workload Cluster** now appears in **Catalog & Form Wizard**.

### Step 3: Provision with Minimal Sizing

1. Select tenant `default` and choose **AWS EKS Workload Cluster** (`aws-eks`).
2. Configure minimal sizing to keep costs low and provide your IAM credentials (`AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, which are encrypted with SOPS + `age` into `secret.enc.yaml`):
   - **Cluster Name:** `eks-sandbox`
   - **AWS Region:** `us-east-1`
   - **Node Instance Type:** `t3.medium`
   - **Desired Node Count:** `1`
   - **AWS Access Key ID / Secret Access Key:** Your IAM credentials
3. Submit the PR and inspect the **Resource Diff Tree** in **PR & Preview Inspector** to verify the exact VPC, subnets, and EKS resources before any cloud resources are created.
   > The ephemeral preview runs in `keel-previews` without your tenant's cloud credentials, which are deliberately not copied there. A cloud preview may therefore end in `failed`, leaving you with the provisional synthesized diff and the Git diff. See [Pre-Merge PKO Previews](previews.md#limitations).
4. Merge the PR to start provisioning.

### Step 4: Clean Up Cloud Resources

When you are done testing, click **Delete** on the `eks-sandbox` stack in **Topology Map** and merge the removal PR. PKO v2 then destroys the EKS cluster and VPC.

---

## 4. Provision an Always-Free GCP Compute Engine VM (`e2-micro`)

GCP's **Always Free** tier includes 1 `e2-micro` VM per month with up to 30 GB `pd-standard` disk in `us-central1`, `us-east1`, or `us-west1` (omitting a public IPv4 address avoids external IP charges).

### Step 1: Prepare a GCP Service Account Key

1. In the Google Cloud Console, select or create a **Project** and enable the **Compute Engine API** (`compute.googleapis.com`).
2. Under **IAM & Admin → Service Accounts**, create a service account with **Compute Admin** (`roles/compute.admin`) and download a **JSON key**.

### Step 2: Enable the `gcp` Provider

1. Open **Control Plane & Providers** in the web portal and enable **Google Cloud Platform (`gcp`)**.
2. Merge the generated PR updating `providers/registry.yaml`.

### Step 3: Add the `gcp-free-vm` Blueprint in the GUI

You can copy `catalog.yaml` and `Pulumi.yaml` from [Provider Blueprint Examples: GCP Compute Engine VM](../catalog/provider-examples.md#gcp-compute-engine-vm-gcp-vm), or add it directly in the GUI:

1. In **Catalog & Form Wizard**, click **+ Add Catalog Item** and select **Kind:** `infra`.
2. Set **ID** to `gcp-free-vm`, **Provider** to `gcp`, **Pulumi Runtime** to `yaml`, **Entrypoint** to `Pulumi.yaml`, and leave **Protect By Default** unchecked.
3. Paste the `Pulumi.yaml`:

```yaml
name: gcp-free-vm
runtime: yaml
description: Always Free GCP e2-micro Compute Engine VM

config:
  vmName:
    type: string
    default: keel-free-vm
  project:
    type: string
  zone:
    type: string
    default: us-east1-b

resources:
  freeVm:
    type: gcp:compute:Instance
    properties:
      name: ${vmName}
      project: ${project}
      machineType: e2-micro
      zone: ${zone}
      bootDisk:
        initializeParams:
          image: debian-cloud/debian-12
          size: 10
          type: pd-standard
      networkInterfaces:
        - network: default
      labels:
        managed-by: keel

outputs:
  instanceName: ${freeVm.name}
  internalIp: ${freeVm.networkInterfaces[0].networkIp}
```

4. Expand **Edit JSON Schema (Optional)** to build the form fields visually (or click **Derive from Pulumi.yaml** + **+ Add gcp Credentials**), or paste the JSON Schema directly:

```json
{
  "type": "object",
  "required": ["vmName", "project", "zone", "GOOGLE_CREDENTIALS"],
  "properties": {
    "vmName": {
      "type": "string",
      "title": "VM Name",
      "default": "keel-free-vm",
      "pattern": "^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$"
    },
    "project": {
      "type": "string",
      "title": "GCP Project ID"
    },
    "zone": {
      "type": "string",
      "title": "GCP Zone (Always Free Eligible)",
      "default": "us-east1-b",
      "enum": [
        "us-central1-a",
        "us-central1-b",
        "us-central1-c",
        "us-east1-b",
        "us-east1-c",
        "us-west1-a",
        "us-west1-b"
      ]
    },
    "GOOGLE_CREDENTIALS": {
      "type": "string",
      "title": "GCP Service Account JSON Key",
      "format": "password",
      "writeOnly": true
    }
  }
}
```

5. Submit and merge the catalog PR.

### Step 4: Provision via GUI or CLI

Submit the form in **Catalog & Form Wizard**, or format your downloaded `.json` key as a single-line string into `/tmp/gcp-vm-values.yaml` and run:

```bash
cat <<'EOF' > /tmp/gcp-vm-values.yaml
vmName: keel-free-vm
project: my-gcp-project-id
zone: us-east1-b
GOOGLE_CREDENTIALS: '{"type":"service_account",...}'
EOF

keel infra provision \
  --blueprint gcp-free-vm \
  --tenant default \
  --values-file /tmp/gcp-vm-values.yaml \
  --server http://127.0.0.1:8080

keel preview status --pr <PR_NUMBER> --tenant <TENANT> --watch --server http://127.0.0.1:8080
```

`GOOGLE_CREDENTIALS` (marked `"writeOnly": true` and `"format": "password"`) is encrypted with SOPS + `age` into `secret.enc.yaml` and bound to `Stack.spec.envRefs`, while `vmName`, `project`, and `zone` populate `Stack.spec.config`. Merge the PR to create the VM. When you are done, click **Delete** on the stack in **Topology Map** and merge the removal PR so PKO v2 runs `pulumi destroy`.

---

## 5. Add Your Own Blueprint (`cloudflare` or `hetzner`)

Try extending the self-service catalog with another provider from [Provider Blueprint Examples](../catalog/provider-examples.md):

These providers have doc examples only and are not tested.

1. Enable **Cloudflare (`cloudflare`)** or **Hetzner Cloud (`hetzner`)** on **Control Plane & Providers** and merge the PR.
2. Add the corresponding `catalog.yaml` + `Pulumi.yaml` under `catalog/infra/<blueprint-id>/` via **+ Add Catalog Item** and merge.
3. Open **Catalog & Form Wizard** to see the JSON Schema automatically rendered as a validated form, with any `writeOnly: true` / `format: password` fields (such as `CLOUDFLARE_API_TOKEN` or `HCLOUD_TOKEN`) encrypted with SOPS + `age` into `secret.enc.yaml` and bound to `Stack.spec.envRefs` before touching Git.

---

## 6. Deploy a Parameterized Kubernetes Manifest Template (`http-echo-manifest`)

Deploy raw Kubernetes YAML templates without packaging a Helm chart, choosing between Flux CD and Pulumi execution engines.

### Step 1: Deploy via the Web Portal
1. In **Catalog & Form Wizard**, select target tenant `default`, or a tenant you created (both engines work in every tenant).
2. Locate the **HTTP Echo Server** (`http-echo-manifest`) application blueprint.
3. In the deploy modal, configure the parameters:
   - **Application Name:** `echo-demo`
   - **Service Port:** `8080`
   - **Echo Message:** `"Hello from Keel Manifest Template!"`
   - **Optional Secret API Token:** any value (`[A-Za-z0-9+/=_.-]`, up to 128 characters). It is committed only SOPS-encrypted (`config/secrets.enc.yaml`); on the Flux engine the `<instance>-config` Kustomization decrypts it with the tenant's `sops-age` key and Flux substitutes it into the `http-echo-api-token` Secret (`ECHO_API_TOKEN` in the pod).
4. Notice the **Action Engine** selector:
   - **Flux CD Kustomization (`flux`):** Generates `manifests.yaml`, non-secret substitutions in `config/values-configmap.yaml` (applied first by an `<instance>-config` Kustomization), and a Flux `Kustomization` that depends on it and uses `postBuild.substituteFrom`.
   - **Pulumi PKO Stack (`pulumi`):** Generates `Pulumi.yaml` wrapping the rendered manifests in `kubernetes:yaml/v2:ConfigGroup` with a PKO v2 `Stack` and ephemeral PR preview diffing.
5. Click **Submit & Open Git PR Proposal**.
6. Inspect the PR in **PR & Preview Inspector** and click **Merge PR**. The PR is opened in the tenant's own repository (`<platform-repo>-tenant-<id>`); the inspector shows which repository it belongs to.

### Step 2: Deploy via Keel CLI (`--engine flux` or `--engine pulumi`)

Submit the proposal directly via the CLI:

```bash
cat <<'EOF' > /tmp/echo-values.yaml
app_name: echo-cli
port: 8080
message: "Hello from CLI Manifest Deployment"
EOF

# Option A: Deploy using Flux CD engine
keel app install \
  --app http-echo-manifest \
  --tenant default \
  --values-file /tmp/echo-values.yaml \
  --engine flux \
  --server http://127.0.0.1:8080

# Option B: Deploy using Pulumi PKO v2 engine (provides pre-merge diff previews)
keel app install \
  --app http-echo-manifest \
  --tenant default \
  --values-file /tmp/echo-values.yaml \
  --engine pulumi \
  --server http://127.0.0.1:8080
```

Verify in **Topology Map** that `echo-demo` transitions from `Pending` → `Synced` / `Ready`.

---

## 7. Connect to a Provisioned VM via SSH (Web Portal & CLI)

Once an infrastructure blueprint containing virtual machines has been provisioned (such as `aws-vm`, `azure-vm`, or `gcp-vm`):

### Step 1: Connect via the Web Portal
1. In **Topology Map**, locate your provisioned VM stack card and click the **SSH** button (or open the **Stack Inspector** and click **SSH**).
2. The **SSH Connect: `<stack>`** modal opens with resolved metadata:
   - `Host`: Public IPv4 address or DNS name
   - `User`: Configured username (`ubuntu`, `azureuser`, or `debian`)
   - `Port`: `22`
3. Click the copy button next to `ssh -p 22 <user>@<host>` to paste into your local terminal.
4. Or click **"Launch Browser Terminal"**:
   - Select your saved profile SSH key (or paste an ephemeral private key).
   - An in-browser terminal (wterm) connects over a secure WebSocket bridge (`GET /api/v1/stacks/{ns}/{name}/ssh/ws`), streaming interactive PTY input and output directly in your browser.

### Step 2: Connect via the Keel CLI (`keel infra ssh`)

```bash
# Print connection table and copyable ssh command
keel infra ssh aws-vm --tenant default --server http://127.0.0.1:8080

# Output raw command string
keel infra ssh aws-vm --tenant default --output command

# Launch an interactive terminal session directly in your shell
keel infra ssh aws-vm --tenant default --exec

# Launch with custom SSH identity file and port
keel infra ssh aws-vm --tenant default -i ~/.ssh/id_ed25519 --port 22 --exec
```

---

## 8. Deploy a Self-Service Rauthy OIDC Provider (`rauthy`)

Tenants can run their own [Rauthy](https://github.com/sebadob/rauthy) identity provider from the catalog, for example to give their applications OIDC login without touching Keel's IdP. Two Rauthy entries exist, and they are different things:

| Entry | Where it appears | Purpose |
| :--- | :--- | :--- |
| `catalog/core/rauthy` | **Topology Map → Management Plane add-ons** (admins only) | Keel's own IdP in `keel-system` (Stage 0); version-only upgrades through reviewed PRs |
| `catalog/apps/rauthy` | **Catalog & Form Wizard** (any tenant) | A tenant-owned Rauthy in `tenant-<id>`, installed and edited like any other catalog app |

Both pin the same `ghcr.io/sebadob/rauthy` image tag. The tenant app is a single-node Rauthy (Hiqlite storage on a PersistentVolumeClaim). Replicas are not configurable because Hiqlite raft membership is configuration-driven.

### Step 1: Generate the Secrets

The form needs four secret values. Keel never generates or defaults them:

```bash
openssl rand -base64 32   # Encryption Key (exactly 32 bytes, base64)
openssl rand -base64 48   # Hiqlite Raft Secret
openssl rand -base64 48   # Hiqlite API Secret (must differ from the raft secret)
```

Choose a **Bootstrap Admin Password** of 14 to 128 characters without whitespace, double quotes, backslashes or `$` (Flux substitution would rewrite `${...}` in it). Rauthy applies the bootstrap admin email and password only on its very first start (empty database). Change the password in the Rauthy Admin UI afterwards. Keep the encryption key: Rauthy encrypts data at rest with it, so it must stay the same on every Edit.

### Step 2: Deploy via the Web Portal or CLI

Rauthy is a Flux manifest app: its Kustomizations read the tenant's own Git source (`tenant-<id>/keel-gitops`, from `tenants/<id>/git-repository.yaml`).

1. In **Catalog & Form Wizard**, select tenant `default` and choose **Rauthy OIDC Provider** (`rauthy`).
2. Fill in a **Public Hostname** unique to your tenant (no default; for example `auth-default.keel.local`), **Public HTTPS Port Suffix** (`:8888` for the local `kind` Gateway, empty when served on port 443), **Bootstrap Admin Email**, and the four secrets from Step 1. Keel rejects platform hosts (`idp.`, `keel.`, `forgejo.` under the Keel domain, and the configured IdP host) and hostnames another app already routes; the Kyverno policy `restrict-tenant-route-hostnames` enforces the platform-host rule at admission too. Keep the **Flux CD** engine (the default): Keel rejects the Pulumi engine for this app, because only Flux can substitute its secret fields.
3. Click **Submit & Open Git PR Proposal**, review the PR, and merge it.

```bash
cat <<'EOF' > rauthy-values.yaml
hostname: auth-default.keel.local
publicPortSuffix: ":8888"
adminEmail: admin@keel.local
encKeyId: keel1
encKey: "<openssl rand -base64 32>"
hiqliteSecretRaft: "<openssl rand -base64 48>"
hiqliteSecretApi: "<openssl rand -base64 48>"
adminPassword: "<14+ character password>"
EOF

keel app install \
  --app rauthy \
  --tenant default \
  --values-file ./rauthy-values.yaml \
  --server http://127.0.0.1:8080
```

### How the Secrets Reach Rauthy

1. Keel partitions the four `format: password` fields out of the form values and encrypts them with SOPS and the tenant's `age` key into `tenants/<tenant>/apps/rauthy/config/secrets.enc.yaml` (the `rauthy-secrets` Secret). Git never holds the plaintext. The committed `manifests.yaml` keeps only `${encKey}`-style placeholders.
2. The `rauthy-config` Flux `Kustomization` decrypts `config/secrets.enc.yaml` with the tenant `sops-age` Secret (`spec.decryption`) and applies it, together with the `rauthy-values` ConfigMap, in the tenant namespace.
3. The `rauthy` Kustomization depends on `rauthy-config`, so it starts only once the Secret exists. It then substitutes the values into the in-cluster `rauthy-env` Secret through `postBuild.substituteFrom`, and Rauthy starts with them on the first reconcile. The container reads them as environment variables (`ENC_KEYS`, `HQL_SECRET_RAFT`, `HQL_SECRET_API`, `BOOTSTRAP_ADMIN_PASSWORD_PLAIN`). On **Edit**, leave a secret field blank to keep its stored value; secrets you do re-enter take effect after the pod restarts (`kubectl -n tenant-<id> rollout restart statefulset/rauthy`). Leave `encKey` blank unless you mean to replace it: the template configures a single `ENC_KEYS` entry, so data encrypted with the previous key becomes unreadable.

Only one `rauthy` instance per tenant namespace is supported, because the template uses fixed resource names (`rauthy`, `rauthy-config`, `rauthy-env`, `rauthy-headless`).

### Step 3: Verify

The app is exposed through an `HTTPRoute` on the shared `keel-ingress` Gateway (HTTPS listeners) and a NetworkPolicy that admits only the Envoy proxies in `keel-system`. Add `127.0.0.1 auth-default.keel.local` to your `hosts` file (the Stage 0 wildcard certificate covers `*.keel.local`), then:

```bash
curl -k https://auth-default.keel.local:8888/auth/v1/.well-known/openid-configuration
```

The `issuer` must be `https://auth-default.keel.local:8888/auth/v1`. Sign in to `https://auth-default.keel.local:8888/auth/v1/admin` with the bootstrap admin. **Require MFA for Admins** (`adminForceMfa`) is off by default, like Keel's own Rauthy, because browsers refuse passkeys on the untrusted local CA. Once you serve Rauthy with a browser-trusted certificate, register a passkey in the Admin UI and then set **Require MFA for Admins** through **Edit**. The pod has egress only to cluster DNS. Allow more egress with your own NetworkPolicy if Rauthy must reach SMTP or upstream identity providers.
