# Catalog & Blueprint Overview

Keel's self-service catalog is stored directly in your GitOps repository under `catalog/`:

```text
catalog/
├── infra/
│   └── <blueprint-id>/
│       ├── catalog.yaml    # CatalogInfraBlueprint (keel.io/v1alpha1)
│       └── Pulumi.yaml     # Pulumi YAML / OpenTofu program
├── apps/
│   └── <app-id>/
│       └── catalog.yaml    # CatalogApplication (keel.io/v1alpha1)
└── bundles/
    └── <bundle-id>/
        └── catalog.yaml    # CatalogBundle (keel.io/v1alpha1)
```

## Built-in Starter Applications

On a fresh repository, `keel bootstrap init` seeds these self-service apps into `catalog/apps/`. Any tenant can install them from **Catalog & Form Wizard**:

| ID | Name | Runtime | Notes |
| :--- | :--- | :--- | :--- |
| `podinfo` | Podinfo Microservice | `helm` | Canary-capable demo app (Flagger + Envoy Gateway) |
| `cert-manager` | cert-manager | `helm` | Workload clusters only (`scope: cluster`) |
| `dex` | Dex OIDC Provider | `helm` | Tenant-owned Dex. Workload clusters only (`scope: cluster`): the chart renders a `ClusterRole`/`ClusterRoleBinding` |
| `rauthy` | Rauthy OIDC Provider | `manifest` (Flux) | Tenant-owned single-node Rauthy. Four required secret fields are SOPS-encrypted per tenant. Exposed through an `HTTPRoute` on the shared `keel-ingress` Gateway ([walkthrough](../getting-started/things-to-try.md#8-deploy-a-self-service-rauthy-oidc-provider-rauthy)) |

The tenant `rauthy` app is separate from Keel's own IdP. `catalog/core/rauthy` describes the protected management-plane Rauthy in `keel-system`. Admins manage it from the Topology Map, and it never appears in the tenant catalog. Both entries pin the same `ghcr.io/sebadob/rauthy` image tag and are bumped together.

## How Provider Blueprints Work

Enabling a provider on the **Control Plane & Providers** tab updates `providers/registry.yaml`. That registry acts as an allowlist:

- `GET /api/v1/catalog` scans `catalog/infra/*/catalog.yaml` and returns only blueprints whose `metadata.provider` is `enabled: true` in `providers/registry.yaml`.
- Enabling a provider does **not** auto-generate blueprints; you add the exact `CatalogInfraBlueprint` compositions (`catalog.yaml` + `Pulumi.yaml`) your organization wants to offer.

![Catalog & Form Wizard](../assets/screenshots/catalog.png)

## `CatalogInfraBlueprint` Schema (`keel.io/v1alpha1`)

```yaml
apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: my-blueprint
  name: Human-Readable Name
  description: Short description shown in the catalog
  category: kubernetes-cluster # or database, networking, compute, edge
  provider: aws # must match a provider id in providers/registry.yaml
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: false
  outputs:
    - endpoint
  schema:
    type: object
    required:
      - name
    properties:
      name:
        type: string
        title: Resource Name
        default: prod-cluster
      apiToken:
        type: string
        title: API Token
        format: password
        writeOnly: true # encrypted with SOPS + age before committing
  uiSchema:
    apiToken:
      ui:widget: password
```

### Secret Partitioning (`writeOnly: true` / `format: password`)

Any schema property marked with `writeOnly: true` or `format: password` is automatically partitioned out of plaintext `values.yaml` / `Stack.spec.config` and encrypted into a SOPS-encrypted Kubernetes `Secret` (`secret.enc.yaml`, named `<instance-name>-secrets`) using the tenant's `age` public key before staging to Git.

For infrastructure blueprints (`CatalogInfraBlueprint`), Keel also binds each partitioned secret key to `Stack.spec.envRefs` using the **exact schema property name** as the environment variable name inside the PKO v2 workspace pod. Name credential properties after the environment variable expected by the Pulumi/OpenTofu provider (for example, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `GOOGLE_CREDENTIALS`, `HCLOUD_TOKEN`, or `CLOUDFLARE_API_TOKEN`).

## `CatalogApplication` Schema (`keel.io/v1alpha1`)

Upstream Helm charts, Kustomize bases, and raw Kubernetes manifest templates work **without structural modification**. A thin `catalog.yaml` sidecar defines the source and a JSON Schema that renders into standard `values.yaml` overrides or manifest template variables:

```yaml
apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: podinfo
  name: Podinfo
  category: web
  runtime: helm # helm | kustomize | manifest
spec:
  upstream:
    repoUrl: https://stefanprodan.github.io/podinfo
    chart: podinfo
    version: 6.7.1
  progressiveDelivery:
    supported: true
    defaultPort: 9898
  schema:
    type: object
    properties:
      replicaCount:
        type: integer
        title: Replicas
        default: 2
```

### Placement (`spec.placement.scope`)

`spec.placement.scope` declares where an application may be installed:

| Scope | Meaning | Allowed targets |
| :--- | :--- | :--- |
| `namespaced` (default when `placement` is omitted) | The app creates only namespaced objects. | The tenant namespace on the management cluster, or a remote workload cluster. |
| `cluster` | The app installs cluster-scoped resources: CRDs, `ClusterRole`/`ClusterRoleBinding`s or admission webhooks. | Remote workload clusters only (`targetCluster`). |

```yaml
spec:
  placement:
    scope: cluster # namespaced | cluster
```

Cluster-scoped apps never target the management cluster. Its tenant namespaces run under namespace-scoped RBAC, and the management plane already runs its own copy of these controllers from `catalog/core/` (two cert-manager or Envoy Gateway installs would fight over the same CRDs and webhooks).

Placement is a declarative guard declared by the catalog author. It gives early, clear `400`s and UX hints, but it is not the security boundary: a tenant admin can author their own app without `placement`. What actually blocks cluster-scoped objects on the management cluster is that tenant Flux and PKO objects impersonate `tenant-<id>-sa`, which only holds a namespace-scoped `ClusterRole/admin` RoleBinding (see [Flux multitenancy lockdown](../operations/security-and-tenancy.md#multi-tenant-isolation)).

- **Proposals** (`POST /api/v1/proposals`) for a `scope: cluster` app without a remote `targetCluster` (absent, empty or `local`) are rejected with `400`.
- **Manifest apps:** a `scope: namespaced` `runtime: manifest` app whose `manifests.yaml` declares well-known cluster-scoped kinds (`Namespace`, `CustomResourceDefinition`, `ClusterRole`/`ClusterRoleBinding`, admission webhooks and policies, `PriorityClass`, `StorageClass`, `IngressClass`, `GatewayClass`, ...) is rejected with `400` when registered or edited (set `placementScope: cluster`). A Git-authored one is rejected at proposal time, after rendering, when it targets the management cluster.
- **Bundles:** a `scope: cluster` app item needs `targetClusterFrom` or a remote request `targetCluster`. Otherwise the proposal is rejected with `400`. Registering or editing a bundle through the catalog API requires `targetClusterFrom` on such items (`400` otherwise), because the GUI deploys bundles without a request `targetCluster`. The seeded `canary-core-addons` bundle (Envoy Gateway, Prometheus, Flagger without a cluster item) can therefore only be proposed through the API with a remote `targetCluster`. Use `kind-canary-platform` from the GUI.
- **Deploy dialog:** a cluster-scoped app does not offer *Management Cluster (local)*. Only the target tenant's workload clusters are listed, since a remote `kubeConfig.secretRef` resolves in the release's own `tenant-<id>` namespace. Switching tenant clears a cluster that belongs to another tenant. New deployments preselect the tenant's first workload cluster. Edits never preselect one, so a release without a recorded target is never silently moved. The dialog explains why and blocks submission until a remote cluster or custom kubeconfig Secret is chosen.
- **Catalog Wizard:** tick *Installs cluster-scoped resources* to author `scope: cluster`.

The built-in `cert-manager`, `envoy-gateway`, `flagger`, `prometheus`, `dex` and `authentik` apps are `scope: cluster`. `helm template` of their pinned charts with default values shows they install CRDs, webhooks or `ClusterRole`s. Prometheus has no CRDs, but its server and kube-state-metrics both create `ClusterRole`/`ClusterRoleBinding`s for cluster-wide scraping. Dex (`rbac.createClusterScoped: true`, used by its Kubernetes CRD storage) and authentik (`serviceAccount.create: true`, used by managed outposts) each render a `ClusterRole`/`ClusterRoleBinding`; turning those off would break those features.

## Raw Kubernetes Manifests & Templates (`runtime: manifest`)

For teams deploying raw Kubernetes manifests or parameterized YAML templates instead of packaging a Helm chart, Keel provides native `runtime: manifest` catalog entries.

### Directory Layout & Schema

A raw manifest catalog entry is stored under `catalog/apps/<id>/` with two files:

```text
catalog/apps/http-echo-manifest/
├── catalog.yaml       # CatalogApplication metadata, defaultEngine & JSON Schema
└── manifests.yaml     # Multi-document Kubernetes YAML with ${var} placeholders
```

#### `catalog.yaml`
```yaml
apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: http-echo-manifest
  name: HTTP Echo Server
  description: Parameterized HTTP echo deployment and service template
  category: web
  runtime: manifest
spec:
  defaultEngine: flux # flux | pulumi
  schema:
    type: object
    required: [app_name, port, message]
    properties:
      app_name:
        type: string
        title: Application Name
        default: http-echo
      port:
        type: integer
        title: Service Port
        default: 8080
      message:
        type: string
        title: Echo Message
        default: "Hello from Keel Manifest!"
      secret_api_key:
        type: string
        title: Secret API Key
        format: password
        writeOnly: true # Encrypted via SOPS + age before staging
```

#### `manifests.yaml`
```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ${app_name:=http-echo}
spec:
  replicas: 1
  selector:
    matchLabels:
      app: ${app_name:=http-echo}
  template:
    metadata:
      labels:
        app: ${app_name:=http-echo}
    spec:
      containers:
        - name: echo
          image: hashicorp/http-echo:0.2.3
          args:
            - "-listen=:${port:=8080}"
            - "-text=${message:=hello}"
          ports:
            - containerPort: ${port:=8080}
---
apiVersion: v1
kind: Service
metadata:
  name: ${app_name:=http-echo}
spec:
  ports:
    - port: ${port:=8080}
      targetPort: ${port:=8080}
  selector:
    app: ${app_name:=http-echo}
```

### Template Placeholder Syntax (`${var_name}` / `${var_name:=default}`)
- `${var_name}`: Replaced by the variable value provided in the dynamic form or CLI.
- `${var_name:=default_value}`: Replaced by the variable value if provided, or falls back to `default_value`.
- This placeholder syntax matches standard Kubernetes `envsubst` and Flux CD post-build variable substitution, ensuring portability across execution engines.

### Auto-Extracting Schema Fields in the Catalog Wizard
In the Web UI (**Catalog & Form Wizard → + Add Catalog Item → Application → Manifest**), you can paste or type your raw Kubernetes YAML directly into the manifest editor and click **"Auto-Extract Variables to Schema"**. Keel scans the YAML tokens, finds all `${var_name}` and `${var_name:=default}` patterns, and automatically populates `spec.schema.properties` with appropriate types, titles, and defaults—instantly generating a Shadcn UI form without writing JSON Schema by hand.

### SOPS Secret Partitioning
Properties marked `"writeOnly": true` or `"format": "password"` are automatically partitioned from non-secret variables:
- In the **Flux engine path**, non-secret variables are written to `config/values-configmap.yaml` (`kind: ConfigMap`, `<instance>-values`), while secret variables are encrypted with the tenant's `age` key into `config/secrets.enc.yaml` (`kind: Secret`, `<instance>-secrets`). A separate `<instance>-config` Flux `Kustomization` (`config-kustomization.yaml`) applies both in the tenant namespace of the management cluster, decrypting the Secret with the tenant `sops-age` Secret (`spec.decryption`, `provider: sops`). The app `Kustomization` (`flux-kustomization.yaml`) `dependsOn` `<instance>-config` and substitutes from both with `optional: false`, so the values exist before the first reconcile. Leave secret placeholders (`${secretField}`) in the template: Flux substitutes them at reconcile time, so they never appear in Git. Secret fields in manifest templates are supported only by the Flux engine (Keel rejects `engine: pulumi`). Remote `targetCluster`s work: the Secret stays on the management cluster, but the substituted values reach the workload cluster inside the rendered manifests, as Helm values would. Remote apps also get a `namespace.yaml` that creates the target namespace first. Plaintext secret values must not contain `${`, since Flux also substitutes the decrypted Secret. Apps created with the older single-`Kustomization` layout are moved to this layout on their next **Edit**. On **Edit**, a secret field left blank keeps its stored value (Keel keeps or re-encrypts the committed Secret); fill in only the secrets you want to change. Bundle items that reference a manifest app cannot set `canary: true` or `bindValues` (Helm apps only).
- **Instance names:** Keel derives Flux `Kustomization` names from instance names (`<name>`, `<name>-config`, `<name>-canary`, and `<bundle>-<item>` for bundle items). A proposal is rejected with `409` when one of them is already used by another instance of the tenant, or when a new instance is named `<other>-config`/`<other>-canary` of an existing instance (or the other way round).
- **Route hostnames:** a manifest app's `HTTPRoute`/`GRPCRoute` hostnames must be concrete at proposal time, must not be (or wildcard-cover) a platform host (`idp.`/`keel.`/`forgejo.` under the Keel domain or the configured IdP host), and must not already be routed by another committed app. The Kyverno policy `restrict-tenant-route-hostnames` enforces the platform-host rule at admission. Tenant ServiceAccounts get `httproutes`/`grpcroutes` through `ClusterRole/keel-tenant-gateway-routes` (aggregated into `admin`).
- In the **Pulumi engine path**, secret variables are encrypted into `secrets.enc.yaml` and bound via `Stack.spec.envRefs`.

### Choosing Between Execution Engines (`Flux` vs. `Pulumi`)
Operators can define `spec.defaultEngine: flux | pulumi` in `catalog.yaml` and override the engine at deploy time in the UI Deploy Modal or CLI (`keel app install --engine flux|pulumi`):

| Feature | Flux CD Engine (`flux`) | Pulumi PKO v2 Engine (`pulumi`) |
| :--- | :--- | :--- |
| **Generated Manifests** | `manifests.yaml`, `config/values-configmap.yaml`, `config/secrets.enc.yaml`, `config-kustomization.yaml`, `flux-kustomization.yaml` | `Pulumi.yaml` (`ConfigGroup`), `stack.yaml`, `secrets.enc.yaml` |
| **Reconciler** | `kustomize-controller` via `postBuild.substituteFrom` | `pulumi-kubernetes-operator` (PKO v2) |
| **Pre-Merge Previews** | Git unified diff in PR | Ephemeral PKO `Stack` (`spec.preview: true`) with exact resource-level diffs (`+`/`~`/`-`) |
| **Best For** | Lightweight, direct Kubernetes reconciliation with zero IaC runner overhead | Applications requiring pre-merge Kubernetes resource diff previews or unified multi-cloud coordination |

## `CatalogBundle` Schema (`keel.io/v1alpha1`)

A `CatalogBundle` composes multiple infrastructure blueprints and applications into an ordered dependency graph deployed atomically in a single PR. Flux `Kustomization` `dependsOn` + `healthChecks` ensure downstream items wait until upstream infrastructure is `Ready`, and `targetClusterFrom` wires an application's `kubeConfig` directly to a provisioned workload cluster `Secret`:

```yaml
apiVersion: keel.io/v1alpha1
kind: CatalogBundle
metadata:
  id: kind-with-podinfo
  name: Kind Cluster + Podinfo
  description: Provision a local kind workload cluster and deploy Podinfo onto it
  category: Platform
spec:
  items:
    - id: cluster
      kind: infra
      catalogRef: kind-cluster
    - id: app
      kind: app
      catalogRef: podinfo
      dependsOn:
        - cluster
      targetClusterFrom: cluster
      targetNamespace: apps
```

## Adding & Editing Catalog Items

You can register or update blueprints, applications, and bundles in two ways:

1. **Web UI (+ Add Catalog Item / Edit):**
   - In **Catalog & Form Wizard**, click **+ Add Catalog Item**, choose **Application (Helm / Kustomize)**, **Infrastructure Blueprint (PKO v2)**, or **Multi-Item Bundle (Infra + Apps)**, and fill in the fields. For infrastructure blueprints, you can paste or generate `Pulumi.yaml`, click **Derive from Pulumi.yaml** to auto-populate form fields from `config:`, click **+ Add \<provider\> Credentials** to add `writeOnly: true` / `format: password` credential fields, and customize fields in the **Form Fields** builder or **JSON Schema** editor before opening the PR.
   - Select any existing catalog item and click **Edit** in the item header to update its metadata, upstream/provider settings, bundle steps, optional JSON Schema, or—for infrastructure blueprints—its `Pulumi.yaml` program via a Git PR (`PUT /api/v1/catalog/{kind}/{id}`). Leave **Pulumi.yaml Program** blank when editing to preserve the existing `Pulumi.yaml`.
2. **Direct Git PR:** Commit `catalog/infra/<id>/catalog.yaml` (+ `Pulumi.yaml`), `catalog/apps/<id>/catalog.yaml`, or `catalog/bundles/<id>/catalog.yaml` (see [Provider Blueprint Examples](provider-examples.md)) on a feature branch and open a PR.
