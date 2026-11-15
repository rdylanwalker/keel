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

## How Provider Blueprints Work

Enabling a provider on the **Topology Map** updates `providers/registry.yaml`. That registry acts as an allowlist:

- `GET /api/v1/catalog` scans `catalog/infra/*/catalog.yaml` and returns only blueprints whose `metadata.provider` is `enabled: true` in `providers/registry.yaml`.
- Enabling a provider does **not** auto-generate blueprints; you add the exact `CatalogInfraBlueprint` compositions (`catalog.yaml` + `Pulumi.yaml`) your organization wants to offer.

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
        x-keel-secret: true # encrypted with SOPS + age before committing
  uiSchema:
    apiToken:
      ui:widget: password
```

### Secret Partitioning (`x-keel-secret: true`)

Any schema property marked with `"x-keel-secret": true` (or `format: password`) is automatically partitioned out of plaintext `spec.config` and encrypted into a SOPS-encrypted Kubernetes `Secret` using the tenant's `age` public key before staging to Git.

## `CatalogApplication` Schema (`keel.io/v1alpha1`)

Upstream Helm charts and Kustomize bases work **without structural modification**. A thin `catalog.yaml` sidecar defines the upstream source and a JSON Schema that renders into standard `values.yaml` overrides:

```yaml
apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: podinfo
  name: Podinfo
  category: web
  runtime: helm
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
   - In **Catalog & Form Wizard**, click **+ Add Catalog Item**, choose **Application (Helm / Kustomize)**, **Infrastructure Blueprint (PKO v2)**, or **Multi-Item Bundle (Infra + Apps)**, fill in the fields (optionally authoring `Pulumi.yaml` directly in the **Pulumi.yaml Program** editor or inserting a starter template for infrastructure blueprints), and merge the generated PR.
   - Select any existing catalog item and click **Edit** in the item header to update its metadata, upstream/provider settings, bundle steps, optional JSON Schema, or—for infrastructure blueprints—its `Pulumi.yaml` program via a Git PR (`PUT /api/v1/catalog/{kind}/{id}`). Leave **Pulumi.yaml Program** blank when editing to preserve the existing `Pulumi.yaml`.
2. **Direct Git PR:** Commit `catalog/infra/<id>/catalog.yaml` (+ `Pulumi.yaml`), `catalog/apps/<id>/catalog.yaml`, or `catalog/bundles/<id>/catalog.yaml` (see [Provider Blueprint Examples](provider-examples.md)) on a feature branch and open a PR.
