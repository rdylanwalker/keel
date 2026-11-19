# Things to Try

These hands-on walkthroughs assume you have completed the [Stage 0 Bootstrap](bootstrap.md) quick-start and have:

1. A local `kind` management cluster running (bootstrapped with `--target kind --local-dev-docker-socket-exception`).
2. `keel serve` running on `http://127.0.0.1:8080`.
3. An authenticated Web UI or CLI session (`keel auth login`).

> **New to Keel?** Follow the [Stage 0 Bootstrap](bootstrap.md) guide first to initialize your local `kind` management cluster and sign in via OIDC.

---

## 1. Provision a Second `kind` Workload Cluster

Because the `kind` provider is enabled by default in `providers/registry.yaml`, you can spin up a second isolated Kubernetes cluster on Docker right away.

### Step 1: Create a Tenant

1. Open `http://127.0.0.1:8080` and go to **Catalog & Form Wizard**.
2. Click **+ New Tenant**, enter `dev-team`, and submit.
3. Open **PR & Preview Inspector** and merge the generated PR (`tenants/dev-team/rbac.yaml`).

### Step 2: Submit the `kind-cluster` Blueprint

In **Catalog & Form Wizard**:

1. Set **Target Tenant** to `dev-team`.
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
  --tenant dev-team \
  --values-file /tmp/workload-dev.yaml \
  --server http://127.0.0.1:8080
```

### Step 3: Inspect the Preview & Merge

1. Open **PR & Preview Inspector** (or run `keel preview status --pr <PR_NUMBER> --watch --server http://127.0.0.1:8080`).
2. Review the **Git Diff** (`tenants/dev-team/infra/workload-dev.yaml`) and the **PKO v2 Diff** (`+ Create` for `kind:index:Cluster`).
3. Click **Merge PR Proposal**.
4. Once Flux CD and PKO v2 reconcile, verify the new cluster is running alongside your management cluster:

```bash
kind get clusters
```

---

## 2. Deploy an Application with Canary Rollouts (`podinfo`)

With a tenant namespace ready (such as `default` or `dev-team`), deploy **Podinfo** (`podinfo`) using an upstream Helm chart with Flagger + Envoy Gateway progressive delivery enabled.

### Step 1: Initial Canary Deployment

1. In **Catalog & Form Wizard**, select your target tenant (e.g., `default`) and choose **Podinfo Microservice** (`podinfo`).
2. Check **Enable Flagger Canary**.
3. Fill in the form (`Replica Count`: `2`, `Ingress Hostname`: `podinfo.keel.local`, `API Secret Token`: any 8+ character secret, and keep the pre-filled `restricted` PSS `Container Security Context` defaults).
4. Click **Submit & Open Git PR Proposal** (or via CLI):

```bash
cat <<'EOF' > podinfo-values.yaml
replicaCount: 2
ingressHost: podinfo.keel.local
apiToken: demo-podinfo-token
EOF

keel app install \
  --app podinfo \
  --tenant default \
  --values-file ./podinfo-values.yaml \
  --canary \
  --server http://127.0.0.1:8080
```

5. Inspect the PR diff in **PR & Preview Inspector**—which synthesizes `helmrelease-canary.yaml`, `canary.yaml`, `networkpolicy-canary.yaml`, and SOPS-encrypted `secrets.enc.yaml`—and click **Merge PR**.
6. In **Topology Map**, wait for `podinfo` to reach `Ready` / `Initialized` in the **Flagger Canary Traffic Rollouts (Envoy Gateway)** panel.

### Step 2: Demonstrate a Live Canary Traffic Rollout

1. Ensure `127.0.0.1 podinfo.keel.local` is in your `hosts` file and Envoy Gateway (`keel-ingress`) is port-forwarded on `127.0.0.1:8888`:

```bash
kubectl --context kind-keel-mgmt -n keel-system port-forward svc/envoy-keel-system-keel-ingress-34e23306 8888:8888
```

2. Start a traffic loop in a terminal so Prometheus records live Envoy Gateway success-rate and latency metrics:

```bash
while true; do
  curl -s -H "Host: podinfo.keel.local" http://127.0.0.1:8888/version
  echo ""
  sleep 1
done
```

3. In **Topology Map**, click **Edit** on the `podinfo` release, change **Chart Version** (for example, `6.7.1` → `6.7.0`) or **Replica Count** (`2` → `3`), re-enter **API Secret Token**, and verify **Enable Flagger Canary** remains checked.
4. Submit and merge the PR.
5. Watch the **Flagger Canary Traffic Rollouts (Envoy Gateway)** panel and your `curl` loop:
   - Flagger scales up the new canary pods alongside `podinfo-primary`.
   - Every `30s`, Flagger shifts weight (`10% → 20% → 30% → 40% → 50%`) while querying Prometheus (`envoy-gateway-request-success-rate` and `envoy-gateway-request-duration`).
   - Your `curl` output will alternate between the old and new versions in proportion to the canary weight until Flagger promotes the new version to `podinfo-primary` (`Succeeded`).

---

## 3. Provision a Minimal AWS EKS Cluster (Free-Tier-Eligible Nodes)

You can use the same management cluster to provision cloud infrastructure. This walkthrough provisions a minimal **AWS EKS** cluster using AWS Free-Tier-eligible `t3.micro` / `t2.micro` EC2 nodes.

> **Cost Note:** While `t3.micro` / `t2.micro` EC2 instances are eligible for the AWS Free Tier, the EKS control plane itself incurs standard AWS hourly charges (~$0.10/hour). Remember to delete the stack via PR when you finish testing.

### Step 1: Enable the `aws` Provider

1. Open **Control Plane & Providers** in the web portal.
2. Locate the **Amazon Web Services (`aws`)** provider card and click **Enable Provider**.
3. Merge the generated PR updating `providers/registry.yaml`.

### Step 2: Add the `aws-eks` Blueprint to Your Catalog

1. In **Catalog & Form Wizard**, click **+ Add Catalog Item** → **Infrastructure Blueprint (PKO v2)** (or commit directly on a branch).
2. Use the `catalog.yaml` and `Pulumi.yaml` from [Provider Blueprint Examples — AWS EKS](../catalog/provider-examples.md#aws-eks-cluster-aws) under `catalog/infra/aws-eks/`.
3. Merge the catalog PR. **AWS EKS Workload Cluster** now appears in **Catalog & Form Wizard**.

### Step 3: Provision with Minimal Sizing

1. Select tenant `dev-team` and choose **AWS EKS Workload Cluster** (`aws-eks`).
2. Configure minimal sizing to keep costs low and provide your IAM credentials (`AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, which are encrypted with SOPS + `age` into `secret.enc.yaml`):
   - **Cluster Name:** `eks-sandbox`
   - **AWS Region:** `us-east-1`
   - **Node Instance Type:** `t3.micro`
   - **Desired Node Count:** `1`
   - **AWS Access Key ID / Secret Access Key:** Your IAM credentials
3. Submit the PR and inspect the **PKO v2 Diff** in **PR & Preview Inspector** to verify the exact VPC, subnets, and EKS resources before any cloud resources are created.
4. Merge the PR to start provisioning.

### Step 4: Clean Up Cloud Resources

When you are done testing, open a PR that removes `tenants/dev-team/infra/eks-sandbox.yaml`. Merging that PR triggers PKO v2 to destroy the EKS cluster and VPC cleanly.

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

You can copy `catalog.yaml` and `Pulumi.yaml` from [Provider Blueprint Examples — Always-Free GCP Compute Engine VM](../catalog/provider-examples.md#always-free-gcp-compute-engine-vm-gcp), or add it directly in the GUI:

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
  --tenant dev-team \
  --values-file /tmp/gcp-vm-values.yaml \
  --server http://127.0.0.1:8080

keel preview status --pr <PR_NUMBER> --watch --server http://127.0.0.1:8080
```

`GOOGLE_CREDENTIALS` (marked `"writeOnly": true` and `"format": "password"`) is encrypted with SOPS + `age` into `secret.enc.yaml` and bound to `Stack.spec.envRefs`, while `vmName`, `project`, and `zone` populate `Stack.spec.config`. Merge the PR to create the VM, and remove `tenants/dev-team/infra/<instance>/` via PR when done so PKO v2 runs `pulumi destroy`.

---

## 5. Add Your Own Blueprint (`cloudflare` or `hetzner`)

Try extending the self-service catalog with another provider from [Provider Blueprint Examples](../catalog/provider-examples.md):

1. Enable **Cloudflare (`cloudflare`)** or **Hetzner Cloud (`hetzner`)** on **Control Plane & Providers** and merge the PR.
2. Add the corresponding `catalog.yaml` + `Pulumi.yaml` under `catalog/infra/<blueprint-id>/` via **+ Add Catalog Item** and merge.
3. Open **Catalog & Form Wizard** to see the JSON Schema automatically rendered as a validated form, with any `writeOnly: true` / `format: password` fields (such as `CLOUDFLARE_API_TOKEN` or `HCLOUD_TOKEN`) encrypted with SOPS + `age` into `secret.enc.yaml` and bound to `Stack.spec.envRefs` before touching Git.

---

## 6. Deploy a Parameterized Kubernetes Manifest Template (`http-echo-manifest`)

Deploy raw Kubernetes YAML templates without packaging a Helm chart, choosing between Flux CD and Pulumi execution engines.

### Step 1: Deploy via the Web Portal
1. In **Catalog & Form Wizard**, select target tenant `dev-team`.
2. Locate the **HTTP Echo Server** (`http-echo-manifest`) application blueprint.
3. In the deploy modal, configure the parameters:
   - **Application Name:** `echo-demo`
   - **Service Port:** `8080`
   - **Echo Message:** `"Hello from Keel Manifest Template!"`
4. Notice the **Action Engine** selector:
   - **Flux CD Kustomization (`flux`):** Generates `manifests.yaml`, non-secret substitutions in `values-configmap.yaml`, and a Flux `Kustomization` using `postBuild.substituteFrom`.
   - **Pulumi PKO Stack (`pulumi`):** Generates `Pulumi.yaml` wrapping the rendered manifests in `kubernetes:yaml/v2:ConfigGroup` with a PKO v2 `Stack` and ephemeral PR preview diffing.
5. Click **Submit & Open Git PR Proposal**.
6. Inspect the PR in **PR & Preview Inspector** and click **Merge PR Proposal**.

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
  --tenant dev-team \
  --values-file /tmp/echo-values.yaml \
  --engine flux \
  --server http://127.0.0.1:8080

# Option B: Deploy using Pulumi PKO v2 engine (provides pre-merge diff previews)
keel app install \
  --app http-echo-manifest \
  --tenant dev-team \
  --values-file /tmp/echo-values.yaml \
  --engine pulumi \
  --server http://127.0.0.1:8080
```

Verify in **Topology Map** that `echo-demo` transitions from `Pending` → `Synced` / `Ready`.

---

## 7. Connect to a Provisioned VM via SSH (Web Portal & CLI)

Once an infrastructure blueprint containing virtual machines has been provisioned (such as `aws-free-tier-vm`, `azure-free-tier-vm`, or `gcp-free-tier-vm`):

### Step 1: Connect via the Web Portal
1. In **Topology Map**, locate your provisioned VM stack card and click the **SSH** button (or open the **Stack Inspector** and click **SSH**).
2. The **Connection Information** modal opens with resolved metadata:
   - `Host`: Public IPv4 address or DNS name
   - `User`: Configured username (`ubuntu`, `azureuser`, or `debian`)
   - `Port`: `22`
3. Click the copy button next to `ssh -p 22 <user>@<host>` to paste into your local terminal.
4. Or click **"Launch Browser Terminal"**:
   - Select your saved profile SSH key (or paste an ephemeral private key).
   - An in-browser terminal connects over a secure WebSocket bridge (`GET /api/v1/stacks/{ns}/{name}/ssh/ws`), streaming interactive PTY input and output directly in your browser.

### Step 2: Connect via the Keel CLI (`keel infra ssh`)

```bash
# Print connection table and copyable ssh command
keel infra ssh aws-free-tier-vm --tenant dev-team --server http://127.0.0.1:8080

# Output raw command string
keel infra ssh aws-free-tier-vm --tenant dev-team --output command

# Launch an interactive terminal session directly in your shell
keel infra ssh aws-free-tier-vm --tenant dev-team --exec

# Launch with custom SSH identity file and port
keel infra ssh aws-free-tier-vm --tenant dev-team -i ~/.ssh/id_ed25519 --port 22 --exec
```
