import {
  CatalogItem,
  CrdWatchEvent,
  ManagementAddonStatus,
  PreviewReport,
  Proposal,
  ProviderRegistryEntry,
  ReleaseSummary,
  StackSummary,
  TenantSummary,
} from "../types/api"

export const DEFAULT_STARTER_MANIFEST = `apiVersion: apps/v1
kind: Deployment
metadata:
  name: \${appName}
  labels:
    app: \${appName}
spec:
  replicas: \${replicas:=2}
  selector:
    matchLabels:
      app: \${appName}
  template:
    metadata:
      labels:
        app: \${appName}
    spec:
      containers:
        - name: app
          image: nginx:alpine
          ports:
            - containerPort: 80
---
apiVersion: v1
kind: Service
metadata:
  name: \${appName}
spec:
  selector:
    app: \${appName}
  ports:
    - port: 80
      targetPort: 80`

export const DEMO_TENANTS: TenantSummary[] = [
  { id: "default", namespace: "tenant-default" },
  { id: "acme", namespace: "tenant-acme" },
  { id: "globex", namespace: "tenant-globex" },
  { id: "stark", namespace: "tenant-stark" },
]

export const DEFAULT_CATALOG: CatalogItem[] = [
  {
    id: "app-web-service",
    name: "Web Service Application",
    description:
      "Standard HTTP microservice Helm chart with Envoy ingress and HPA",
    type: "app",
    category: "Applications",
    version: "1.4.0",
    protected: false,
    schema: {
      title: "Web Service Blueprint",
      type: "object",
      required: ["appName", "replicas", "image"],
      properties: {
        appName: {
          type: "string",
          title: "Application Name",
          default: "frontend-portal",
        },
        replicas: {
          type: "number",
          title: "Replica Count",
          default: 3,
          minimum: 1,
        },
        image: {
          type: "string",
          title: "Container Image",
          default: "ghcr.io/acme/portal:v1.2.0",
        },
        ingressHost: {
          type: "string",
          title: "Ingress Hostname",
          default: "portal.acme.internal",
        },
        enableCanary: {
          type: "boolean",
          title: "Enable Flagger Progressive Canary Rollouts",
          default: true,
        },
      },
    },
    defaultValues: {
      appName: "frontend-portal",
      replicas: 3,
      image: "ghcr.io/acme/portal:v1.2.0",
      ingressHost: "portal.acme.internal",
      enableCanary: true,
    },
  },
  {
    id: "infra-proxmox-vm",
    name: "Proxmox VM Cluster Stack",
    description:
      "Proxmox VE virtual machine pool synthesized via PKO v2 OpenTofu/Pulumi provider",
    type: "infrastructure",
    category: "Infrastructure",
    version: "2.1.0",
    provider: "proxmox",
    protected: true,
    resourceKind: "vm",
    schema: {
      title: "Proxmox VM Pool Blueprint",
      type: "object",
      required: ["poolName", "nodeCount", "cores", "memoryMb"],
      properties: {
        poolName: {
          type: "string",
          title: "VM Pool Name",
          default: "k8s-worker-pool",
        },
        nodeCount: {
          type: "number",
          title: "Node Count",
          default: 3,
          minimum: 1,
        },
        cores: {
          type: "number",
          title: "CPU Cores per Node",
          default: 4,
          minimum: 1,
        },
        memoryMb: {
          type: "number",
          title: "RAM (MB) per Node",
          default: 8192,
          minimum: 1024,
        },
        storagePool: {
          type: "string",
          title: "Proxmox Storage Target",
          default: "local-zfs",
        },
      },
    },
    defaultValues: {
      poolName: "k8s-worker-pool",
      nodeCount: 3,
      cores: 4,
      memoryMb: 8192,
      storagePool: "local-zfs",
    },
  },
  {
    id: "app-postgres-db",
    name: "PostgreSQL Database Cluster",
    description:
      "HA PostgreSQL database with SOPS age encrypted credentials and backup schedule",
    type: "app",
    category: "Databases",
    version: "15.4.0",
    protected: false,
    tenant: "acme",
    schema: {
      title: "PostgreSQL Database Blueprint",
      type: "object",
      required: ["dbName", "storageGb", "adminPassword"],
      properties: {
        dbName: {
          type: "string",
          title: "Database Name",
          default: "tenant_db",
        },
        storageGb: {
          type: "number",
          title: "Storage Size (GB)",
          default: 50,
          minimum: 5,
        },
        adminPassword: {
          type: "string",
          title: "Admin Password (Encrypted with SOPS + age)",
          format: "password",
          default: "SecretP@ssw0rd!",
        },
        highAvailability: {
          type: "boolean",
          title: "Enable HA Replication",
          default: true,
        },
      },
    },
    defaultValues: {
      dbName: "tenant_db",
      storageGb: 50,
      adminPassword: "SecretP@ssw0rd!",
      highAvailability: true,
    },
  },
]

export const DEFAULT_EVENTS: CrdWatchEvent[] = [
  {
    kind: "GitRepository",
    namespace: "flux-system",
    name: "flux-system",
    status: "Ready",
    message: "Fetched revision main@sha1:a1b2c3d4",
    revision: "main@sha1:a1b2c3d4",
    timestamp: new Date(Date.now() - 30000).toLocaleTimeString(),
  },
  {
    kind: "Kustomization",
    namespace: "tenant-acme",
    name: "tenant-acme-apps",
    status: "Ready",
    message: "Applied revision main@sha1:a1b2c3d4",
    revision: "main@sha1:a1b2c3d4",
    timestamp: new Date(Date.now() - 24000).toLocaleTimeString(),
  },
  {
    kind: "Stack",
    namespace: "keel-system",
    name: "pve-workload-cluster-prod",
    status: "Ready",
    message: "PKO v2 stack update succeeded cleanly (18 resources)",
    revision: "v2.1.0",
    timestamp: new Date(Date.now() - 18000).toLocaleTimeString(),
  },
  {
    kind: "Canary",
    namespace: "tenant-acme",
    name: "frontend-portal-canary",
    status: "Progressing",
    message:
      "Advance frontend-portal-canary weight 40% (5xx rate 0.02%, latency 18ms)",
    revision: "v1.4.0",
    timestamp: new Date(Date.now() - 5000).toLocaleTimeString(),
  },
]

/** Demo-only management stack fixture (never rendered outside `?demo=true`, OPEN-7). */
const DEMO_MANAGEMENT_STACK: StackSummary = {
  id: "stack-mgmt-01",
  name: "management-cluster-core",
  namespace: "flux-system",
  tenant: "management",
  provider: "kind",
  type: "management",
  status: "synced",
  protect: true,
  resourceCount: 24,
  lastSynced: new Date(Date.now() - 120_000).toISOString(),
  resourceKind: "cluster",
}

export const DEFAULT_STACKS: StackSummary[] = [
  DEMO_MANAGEMENT_STACK,
  {
    id: "stack-pve-01",
    name: "pve-workload-cluster-prod",
    tenant: "tenant-acme",
    provider: "proxmox",
    type: "workload",
    status: "synced",
    resourceCount: 18,
    lastSynced: "5 minutes ago",
    resourceKind: "cluster",
  },
  {
    id: "stack-aws-01",
    name: "aws-us-east-1-cluster",
    tenant: "tenant-globex",
    provider: "aws",
    type: "cloud",
    status: "reconciling",
    resourceCount: 42,
    lastSynced: "1 minute ago",
    resourceKind: "cluster",
  },
  {
    id: "stack-hetzner-01",
    name: "hetzner-fsn1-nodes",
    tenant: "tenant-stark",
    provider: "hetzner",
    type: "workload",
    status: "drift",
    resourceCount: 12,
    lastSynced: "12 minutes ago",
    resourceKind: "vm",
  },
  {
    id: "stack-postgres-managed",
    name: "postgres-managed",
    tenant: "tenant-acme",
    provider: "kubernetes",
    type: "workload",
    status: "synced",
    resourceCount: 5,
    lastSynced: "1 minute ago",
    resourceKind: "service",
  },
]

export const DEFAULT_PROPOSALS: Proposal[] = [
  {
    id: "42",
    title: "Deploy PostgreSQL database for tenant-acme",
    branch: "keel/tenant-acme/db-pg-42",
    author: "dylan@acme.internal",
    tenant: "tenant-acme",
    status: "open",
    createdAt: "10 minutes ago",
    prUrl: "https://git.internal/keel-io/infra/pulls/42",
    previewStatus: "ready",
  },
  {
    id: "43",
    title: "Scale Proxmox worker pool nodes from 3 to 5",
    branch: "keel/tenant-globex/pve-scale-43",
    author: "infra-bot",
    tenant: "tenant-globex",
    status: "open",
    createdAt: "25 minutes ago",
    prUrl: "https://git.internal/keel-io/infra/pulls/43",
    previewStatus: "ready",
  },
]

export const DEFAULT_PREVIEW: PreviewReport = {
  prUrl: "https://git.internal/keel-io/infra/pulls/42",
  policyStatus: "passed",
  source: "pko",
  run: { phase: "succeeded" },
  summary: {
    create: 3,
    update: 2,
    delete: 0,
    replace: 1,
    same: 12,
    drift: 0,
    total: 18,
  },
  resources: [
    {
      urn: "urn:pulumi:prod::infra::kubernetes:core/v1:Namespace::tenant-acme",
      resource_type: "kubernetes:core/v1:Namespace",
      name: "tenant-acme",
      op: "Create",
      changed_fields: ["metadata.labels", "metadata.name"],
    },
    {
      urn: "urn:pulumi:prod::infra::kubernetes:helm.sh/v3:Release::pg-cluster",
      resource_type: "kubernetes:helm.sh/v3:Release",
      name: "pg-cluster",
      op: "Create",
      changed_fields: ["spec.values", "spec.chart"],
    },
    {
      urn: "urn:pulumi:prod::infra::kubernetes:core/v1:Secret::pg-credentials",
      resource_type: "kubernetes:core/v1:Secret",
      name: "pg-credentials",
      op: "Create",
      changed_fields: ["stringData.password"],
    },
    {
      urn: "urn:pulumi:prod::infra::proxmox:ve/vm:VirtualMachine::worker-node-04",
      resource_type: "proxmox:ve/vm:VirtualMachine",
      name: "worker-node-04",
      op: "Replace",
      changed_fields: ["cores", "memory", "storagePool"],
    },
    {
      urn: "urn:pulumi:prod::infra::kubernetes:networking.k8s.io/v1:Ingress::frontend-ingress",
      resource_type: "kubernetes:networking.k8s.io/v1:Ingress",
      name: "frontend-ingress",
      op: "Update",
      changed_fields: ["spec.rules[0].host"],
    },
    {
      urn: "urn:pulumi:prod::infra::kyverno.io/v1:ClusterPolicy::restrict-pulumi-providers",
      resource_type: "kyverno.io/v1:ClusterPolicy",
      name: "restrict-pulumi-providers",
      op: "Update",
      changed_fields: ["spec.rules[0].match"],
    },
  ],
}

/** Demo-only provider registry entries (same shape as `GET /api/v1/providers`). */
export const DEFAULT_PROVIDERS: ProviderRegistryEntry[] = [
  {
    id: "proxmox",
    enabled: true,
    description: "Proxmox VE Cluster",
    package: "bpg/proxmox",
  },
  {
    id: "aws",
    enabled: true,
    description: "Amazon Web Services",
    package: "pulumi/aws",
  },
  {
    id: "gcp",
    enabled: false,
    description: "Google Cloud Platform",
    package: "pulumi/gcp",
  },
  {
    id: "hetzner",
    enabled: true,
    description: "Hetzner Cloud",
    package: "hetznercloud/hcloud",
  },
  {
    id: "kind",
    enabled: true,
    description: "Local kind / k3s",
  },
]

/** Demo-only management add-ons: one protected Flux object per `catalog/core` id. */
const DEMO_MANAGEMENT_ADDONS: Array<{
  id: string
  kind: "HelmRelease" | "Kustomization"
  chart: string
  version: string
  status: string
}> = [
  {
    id: "flux2",
    kind: "HelmRelease",
    chart: "flux2",
    version: "2.20.0",
    status: "Ready",
  },
  {
    id: "s3-state-backend",
    kind: "HelmRelease",
    chart: "seaweedfs",
    version: "4.54.0",
    status: "Ready",
  },
  {
    id: "kyverno",
    kind: "HelmRelease",
    chart: "kyverno",
    version: "3.10.0",
    status: "Ready",
  },
  {
    id: "cert-manager",
    kind: "HelmRelease",
    chart: "cert-manager",
    version: "v1.21.3",
    status: "Ready",
  },
  {
    id: "rauthy",
    kind: "Kustomization",
    chart: "rauthy",
    version: "0.37.2",
    status: "Ready",
  },
  {
    id: "envoy-gateway",
    kind: "HelmRelease",
    chart: "gateway-helm",
    version: "v1.10.0",
    status: "Ready",
  },
  {
    id: "prometheus",
    kind: "HelmRelease",
    chart: "prometheus",
    version: "29.48.0",
    status: "Ready",
  },
  {
    id: "flagger",
    kind: "HelmRelease",
    chart: "flagger",
    version: "1.45.0",
    status: "Ready",
  },
  {
    id: "pulumi-kubernetes-operator",
    kind: "HelmRelease",
    chart: "pulumi-kubernetes-operator",
    version: "2.10.1",
    status: "Ready",
  },
  // Optional add-ons (`--with-forge`, `--with-keel-server`).
  {
    id: "forgejo",
    kind: "HelmRelease",
    chart: "forgejo",
    version: "17.3.3",
    status: "Ready",
  },
  {
    id: "keel-server",
    kind: "Kustomization",
    chart: "keel-server",
    version: "0.7.0",
    status: "Ready",
  },
]

/** Demo-only live add-on statuses (same shape as `GET /api/v1/management/addons`). */
export const DEMO_MANAGEMENT_ADDON_STATUSES: ManagementAddonStatus[] =
  DEMO_MANAGEMENT_ADDONS.map((a) => ({
    id: a.id,
    name: a.id,
    namespace: "flux-system",
    kind: a.kind,
    chart: a.chart,
    desiredVersion: a.version,
    status: a.status,
    tracked: true,
  }))

/** Demo-only management add-on releases (admin view of `GET /api/v1/releases`). */
export const DEMO_MANAGEMENT_ADDON_RELEASES: ReleaseSummary[] =
  DEMO_MANAGEMENT_ADDONS.map((a) => ({
    name: a.id,
    namespace: "flux-system",
    tenant: "management",
    type: a.kind === "HelmRelease" ? "helm" : "kustomize",
    runtime: a.kind === "HelmRelease" ? "helm" : "kustomize",
    chart: a.chart,
    version: a.version,
    status: a.status,
    managementAddon: true,
    protected: true,
    catalogId: a.id,
    addonKind: a.kind,
    desiredVersion: a.version,
  }))

export const DEFAULT_CORE_ADDONS: CatalogItem[] = [
  {
    id: "s3-state-backend",
    name: "S3 State Backend",
    description: "SeaweedFS S3-compatible state and artifact storage",
    type: "app",
    category: "Management Addons",
    chart: "seaweedfs",
    version: "4.54.0",
    schema: { type: "object", properties: {} },
  },
  {
    id: "kyverno",
    name: "Kyverno",
    description: "Kubernetes native policy engine",
    type: "app",
    category: "Management Addons",
    chart: "kyverno",
    version: "3.10.0",
    schema: { type: "object", properties: {} },
  },
  {
    id: "cert-manager",
    name: "cert-manager",
    description:
      "Automated X.509 certificate management and self-signed/ACME ClusterIssuers",
    type: "app",
    category: "Management Addons",
    chart: "cert-manager",
    version: "v1.21.3",
    schema: { type: "object", properties: {} },
  },
  {
    id: "rauthy",
    name: "Rauthy OIDC Provider",
    description:
      "Lightweight Rust OpenID Connect identity provider with Admin UI for users, groups, and OIDC clients",
    type: "app",
    category: "Management Addons",
    chart: "rauthy",
    version: "0.37.2",
    serviceUrl: "https://idp.keel.local:8888/auth/v1/admin",
    schema: { type: "object", properties: {} },
  },
  {
    id: "envoy-gateway",
    name: "Envoy Gateway",
    description: "Kubernetes Gateway API ingress controller",
    type: "app",
    category: "Management Addons",
    chart: "gateway-helm",
    version: "v1.10.0",
    schema: { type: "object", properties: {} },
  },
  {
    id: "prometheus",
    name: "Prometheus",
    description: "Metrics collection and alerting for progressive delivery",
    type: "app",
    category: "Management Addons",
    chart: "prometheus",
    version: "29.48.0",
    schema: { type: "object", properties: {} },
  },
  {
    id: "flagger",
    name: "Flagger",
    description: "Progressive delivery canary operator",
    type: "app",
    category: "Management Addons",
    chart: "flagger",
    version: "1.45.0",
    schema: { type: "object", properties: {} },
  },
  {
    id: "pulumi-kubernetes-operator",
    name: "Pulumi Kubernetes Operator",
    description: "PKO v2 infrastructure stack reconciler",
    type: "app",
    category: "Management Addons",
    chart: "pulumi-kubernetes-operator",
    version: "2.10.1",
    schema: { type: "object", properties: {} },
  },
  {
    id: "flux2",
    name: "Flux CD",
    description:
      "GitOps engine that reconciles the management plane and every tenant from Git, running in multi-tenancy lockdown",
    type: "app",
    category: "Management Addons",
    chart: "flux2",
    version: "2.20.0",
    schema: {
      type: "object",
      properties: {
        logLevel: {
          type: "string",
          title: "Log Level",
          enum: ["debug", "info", "error"],
          default: "info",
        },
      },
    },
  },
  {
    id: "forgejo",
    name: "Forgejo Git Forge",
    description:
      "Optional in-cluster Forgejo hosting the management GitOps repository and pull requests",
    type: "app",
    category: "Management Addons",
    chart: "forgejo",
    version: "17.3.3",
    serviceUrl: "https://forgejo.keel.local:8888",
    schema: { type: "object", properties: {} },
  },
  {
    id: "keel-server",
    name: "Keel Control Plane Server",
    description:
      "Optional in-cluster keel-server (REST/SSE API and web portal) managed by Flux",
    type: "app",
    category: "Management Addons",
    chart: "keel-server",
    version: "0.7.0",
    serviceUrl: "https://keel.keel.local:8888",
    schema: { type: "object", properties: {} },
  },
]
