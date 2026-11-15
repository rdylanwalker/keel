# Things to Try

These hands-on walkthroughs assume you have completed the [Stage 0 Bootstrap](bootstrap.md) quick-start and have:

1. A local `kind` management cluster running (bootstrapped with `--target kind --local-dev-docker-socket-exception`).
2. `keel serve` running on `http://127.0.0.1:8080`.
3. An authenticated Web UI or CLI session (`keel auth login`).

> **New to Keel?** Follow the [Stage 0 Bootstrap](bootstrap.md) guide first to initialize your local `kind` management cluster and obtain your bootstrap token.

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
   - **Kubernetes Version:** `v1.32.0`
   - **Worker Node Count:** `1`
4. Click **Submit & Open Git PR Proposal**.

Or submit the same proposal via CLI:

```bash
cat <<'EOF' > /tmp/workload-dev.yaml
clusterName: workload-dev
kubernetesVersion: v1.32.0
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

With a tenant namespace ready, deploy **Podinfo** (`podinfo`) using an upstream Helm chart with progressive delivery enabled.

1. In **Catalog & Form Wizard**, select tenant `dev-team` and choose **Podinfo** (`podinfo`).
2. Set **Replicas** to `2` and toggle **Flagger Canary Rollout** on.
3. Click **Submit & Open Git PR Proposal** (or via CLI):

```bash
keel app install \
  --app podinfo \
  --tenant dev-team \
  --values-file ./deploy/catalog/apps/podinfo/upstream/values.yaml \
  --canary \
  --server http://127.0.0.1:8080
```

4. Inspect the PR diff (which synthesizes both the Flux `HelmRelease` and Flagger `Canary` manifests) and merge it.
5. Open **GitOps & Canary Console** to watch Flux reconcile the release and Envoy Gateway + Flagger manage traffic shifting.

---

## 3. Provision a Minimal AWS EKS Cluster (Free-Tier-Eligible Nodes)

You can use the same management cluster to provision cloud infrastructure. This walkthrough provisions a minimal **AWS EKS** cluster using AWS Free-Tier-eligible `t3.micro` / `t2.micro` EC2 nodes.

> **Cost Note:** While `t3.micro` / `t2.micro` EC2 instances are eligible for the AWS Free Tier, the EKS control plane itself incurs standard AWS hourly charges (~$0.10/hour). Remember to delete the stack via PR when you finish testing.

### Step 1: Enable the `aws` Provider

1. Open **Topology Map** in the web portal.
2. Locate the **Amazon Web Services (`aws`)** provider card and click **Enable Provider**.
3. Merge the generated PR updating `providers/registry.yaml`.

### Step 2: Add the `aws-eks` Blueprint to Your Catalog

1. In **Catalog & Form Wizard**, click **+ Add Catalog Item** → **Infrastructure Blueprint (PKO v2)** (or commit directly on a branch).
2. Use the `catalog.yaml` and `Pulumi.yaml` from [Provider Blueprint Examples — AWS EKS](../catalog/provider-examples.md#aws-eks-cluster-aws) under `catalog/infra/aws-eks/`.
3. Merge the catalog PR. **AWS EKS Workload Cluster** now appears in **Catalog & Form Wizard**.

### Step 3: Provision with Minimal Sizing

1. Select tenant `dev-team` and choose **AWS EKS Workload Cluster** (`aws-eks`).
2. Configure minimal sizing to keep costs low:
   - **Cluster Name:** `eks-sandbox`
   - **AWS Region:** `us-east-1`
   - **Node Instance Type:** `t3.micro`
   - **Desired Node Count:** `1`
3. Submit the PR and inspect the **PKO v2 Diff** in **PR & Preview Inspector** to verify the exact VPC, subnets, and EKS resources before any cloud resources are created.
4. Merge the PR to start provisioning.

### Step 4: Clean Up Cloud Resources

When you are done testing, open a PR that removes `tenants/dev-team/infra/eks-sandbox.yaml`. Merging that PR triggers PKO v2 to destroy the EKS cluster and VPC cleanly.

---

## 4. Add Your Own Blueprint (`cloudflare` or `hetzner`)

Try extending the self-service catalog with another provider from [Provider Blueprint Examples](../catalog/provider-examples.md):

1. Enable **Cloudflare (`cloudflare`)** or **Hetzner Cloud (`hetzner`)** on **Topology Map** and merge the PR.
2. Add the corresponding `catalog.yaml` + `Pulumi.yaml` under `catalog/infra/<blueprint-id>/` via **+ Add Catalog Item** and merge.
3. Open **Catalog & Form Wizard** to see the JSON Schema automatically rendered as a validated form, with any `x-keel-secret: true` fields (such as API tokens) encrypted with SOPS + `age` before touching Git.
