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

export const DEMO_TENANTS: TenantSummary[] = [
  { id: "default", namespace: "tenant-default" },
  { id: "acme", namespace: "tenant-acme" },
  { id: "globex", namespace: "tenant-globex" },
  { id: "stark", namespace: "tenant-stark" },
]

/**
 * Demo-mode catalog mirroring the real built-in catalog under `deploy/catalog` (same ids, names,
 * descriptions, categories, versions, runtime, placement), mapped the way `getCatalog` maps
 * `GET /api/v1/catalog` (bundles, then infra blueprints, then apps, each sorted by id). Schemas are
 * representative subsets of the real `spec.schema`. `demoFixtures.test.ts` guards against drift.
 */
export const DEFAULT_CATALOG: CatalogItem[] = [
  {
    id: "kind-canary-platform",
    name: "Local kind Cluster + Canary Platform",
    description:
      "Provision a local kind workload cluster via PKO v2 and install Envoy Gateway, Prometheus, and Flagger into keel-system for progressive canary rollouts.",
    type: "bundle",
    category: "platform",
    version: "bundle",
    protected: false,
    bundleItems: [
      { id: "cluster", kind: "infra", catalogRef: "kind-cluster" },
      {
        id: "envoy-gateway",
        kind: "app",
        catalogRef: "envoy-gateway",
        dependsOn: ["cluster"],
        targetClusterFrom: "cluster",
        targetNamespace: "keel-system",
      },
      {
        id: "prometheus",
        kind: "app",
        catalogRef: "prometheus",
        dependsOn: ["cluster"],
        targetClusterFrom: "cluster",
        targetNamespace: "keel-system",
      },
      {
        id: "flagger",
        kind: "app",
        catalogRef: "flagger",
        dependsOn: ["cluster", "envoy-gateway", "prometheus"],
        targetClusterFrom: "cluster",
        targetNamespace: "keel-system",
      },
      {
        id: "canary-config",
        kind: "infra",
        catalogRef: "canary-gateway-config",
        dependsOn: ["cluster", "envoy-gateway", "flagger"],
        targetClusterFrom: "cluster",
      },
    ],
    schema: {
      type: "object",
      required: ["cluster"],
      properties: {
        cluster: {
          type: "object",
          title: "Workload Cluster (kind)",
          required: ["clusterName", "kubernetesVersion", "workerCount"],
          properties: {
            clusterName: {
              type: "string",
              title: "Cluster Name",
              default: "workload-dev",
            },
            kubernetesVersion: {
              type: "string",
              title: "Kubernetes Version",
              default: "v1.35.8",
              enum: ["v1.37.0", "v1.36.4", "v1.35.8", "v1.34.11", "v1.33.12"],
            },
            workerCount: {
              type: "integer",
              title: "Worker Node Count",
              default: 1,
              minimum: 0,
              maximum: 5,
            },
          },
        },
        flagger: {
          type: "object",
          title: "Flagger",
          properties: {
            meshProvider: {
              type: "string",
              title: "Mesh / Gateway Provider",
              default: "gatewayapi:v1",
            },
            metricsServer: {
              type: "string",
              title: "Prometheus Metrics Server URL",
              default: "http://prometheus.keel-system:80",
            },
          },
        },
      },
    },
  },
  {
    id: "aws-vm",
    name: "AWS EC2 Virtual Machine",
    description:
      "Provision an AWS EC2 virtual machine with SSH access (defaults to free-tier t3.micro)",
    type: "infra",
    category: "compute",
    version: "1.0.0",
    provider: "aws",
    resourceKind: "vm",
    pulumiRuntime: "yaml",
    entrypoint: "Pulumi.yaml",
    protectByDefault: true,
    protected: false,
    schema: {
      type: "object",
      required: ["sshPublicKey"],
      properties: {
        instanceName: {
          type: "string",
          title: "Instance Name",
          description: "Name tag assigned to the EC2 instance.",
          default: "keel-free-vm",
        },
        region: {
          type: "string",
          title: "AWS Region",
          description: "AWS region where the EC2 instance will be provisioned.",
          default: "us-east-1",
        },
        instanceType: {
          type: "string",
          title: "EC2 Instance Type",
          description:
            "EC2 instance type (defaults to free-tier eligible t3.micro).",
          default: "t3.micro",
        },
        diskSizeGb: {
          type: "integer",
          title: "Root Volume Size (GB)",
          description:
            "Root EBS volume size in GiB (AWS Free Tier includes up to 30 GB).",
          default: 20,
          minimum: 8,
          maximum: 16384,
        },
        sshUser: { type: "string", title: "SSH Username", default: "ubuntu" },
        sshPublicKey: { type: "string", title: "Operator SSH Public Key" },
        awsAccessKeyId: {
          type: "string",
          title: "AWS Access Key ID",
          writeOnly: true,
        },
        awsSecretAccessKey: {
          type: "string",
          title: "AWS Secret Access Key",
          writeOnly: true,
          format: "password",
        },
      },
    },
    uiSchema: {
      instanceType: {
        "ui:placeholder": "t3.micro",
        "ui:help":
          "Free-tier default: t3.micro (also supports t2.micro, t3.small, m5.large, etc.).",
      },
      sshPublicKey: {
        "ui:widget": "textarea",
        "ui:help": "SSH public key injected into the EC2 instance key pair.",
      },
      awsSecretAccessKey: {
        "ui:widget": "password",
        "ui:help":
          "Encrypted via SOPS + tenant age key before committing to Git.",
      },
    },
  },
  {
    id: "kind-cluster",
    name: "Local kind Workload Cluster",
    description:
      "Provision a local Kubernetes-in-Docker (kind) workload cluster via Pulumi's OpenTofu/Terraform bridge (tehcyx/kind).",
    type: "infra",
    category: "kubernetes-cluster",
    version: "1.0.0",
    provider: "kind",
    resourceKind: "cluster",
    pulumiRuntime: "yaml",
    entrypoint: "Pulumi.yaml",
    protectByDefault: false,
    protected: false,
    schema: {
      type: "object",
      required: ["clusterName", "kubernetesVersion", "workerCount"],
      properties: {
        clusterName: {
          type: "string",
          title: "Cluster Name",
          description:
            "DNS-compatible name for the local kind workload cluster.",
          pattern: "^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$",
          minLength: 2,
          maxLength: 63,
          default: "workload-dev",
        },
        kubernetesVersion: {
          type: "string",
          title: "Kubernetes Version",
          description: "Upstream kindest/node Kubernetes image version tag.",
          default: "v1.35.8",
          enum: ["v1.37.0", "v1.36.4", "v1.35.8", "v1.34.11", "v1.33.12"],
        },
        workerCount: {
          type: "integer",
          title: "Worker Node Count",
          description:
            "Number of worker nodes provisioned alongside the control-plane node.",
          default: 1,
          minimum: 0,
          maximum: 3,
        },
      },
    },
    uiSchema: {
      clusterName: {
        "ui:placeholder": "workload-dev",
        "ui:help": "Lowercase alphanumeric DNS label (e.g., workload-dev).",
      },
      workerCount: {
        "ui:help":
          "Set to 0 for a single-node control-plane cluster, or 1–3 for dedicated worker nodes.",
      },
    },
  },
  {
    id: "postgres-managed",
    name: "Managed PostgreSQL Instance",
    description:
      "Provision an encrypted PostgreSQL database instance exporting connection details to tenant Secrets.",
    type: "infra",
    category: "database",
    version: "1.0.0",
    provider: "aws",
    resourceKind: "service",
    pulumiRuntime: "yaml",
    entrypoint: "Pulumi.yaml",
    protectByDefault: true,
    protected: false,
    schema: {
      type: "object",
      required: ["dbName", "storageGb", "dbPassword"],
      properties: {
        dbName: {
          type: "string",
          title: "Database Name",
          default: "appdb",
          pattern: "^[a-z][a-z0-9_]{1,62}$",
        },
        instanceClass: {
          type: "string",
          title: "RDS Instance Class",
          default: "db.t4g.micro",
          enum: ["db.t4g.micro", "db.t4g.small", "db.t4g.medium"],
        },
        storageGb: {
          type: "integer",
          title: "Allocated Storage (GiB)",
          default: 20,
          minimum: 20,
          maximum: 1000,
        },
        dbPassword: {
          type: "string",
          title: "Master Database Password",
          writeOnly: true,
          minLength: 16,
        },
      },
    },
    uiSchema: {
      dbPassword: {
        "ui:widget": "password",
        "ui:help":
          "Encrypted via SOPS + tenant age key before committing to Git.",
      },
    },
  },
  {
    id: "proxmox-k3s-cluster",
    name: "Proxmox VE k3s Workload Cluster",
    description:
      "Provision a multi-node k3s workload cluster on Proxmox VE via bpg/proxmox OpenTofu/Terraform bridge.",
    type: "infra",
    category: "kubernetes-cluster",
    version: "1.0.0",
    provider: "proxmox",
    resourceKind: "cluster",
    pulumiRuntime: "yaml",
    entrypoint: "Pulumi.yaml",
    protectByDefault: true,
    protected: false,
    schema: {
      type: "object",
      required: ["clusterName", "nodeName", "templateVmId", "sshPublicKey"],
      properties: {
        clusterName: {
          type: "string",
          title: "Cluster Name",
          pattern: "^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$",
        },
        nodeName: {
          type: "string",
          title: "Proxmox Target Node",
          default: "pve1",
        },
        templateVmId: {
          type: "integer",
          title: "Cloud-Init Template VM ID",
          default: 900,
          minimum: 100,
        },
        cores: {
          type: "integer",
          title: "vCPU Cores per Node",
          default: 4,
          minimum: 2,
          maximum: 32,
        },
        memoryMb: {
          type: "integer",
          title: "Memory (MiB) per Node",
          default: 8192,
          minimum: 2048,
        },
        sshPublicKey: { type: "string", title: "Operator SSH Public Key" },
      },
    },
    uiSchema: {
      sshPublicKey: {
        "ui:widget": "textarea",
        "ui:help": "ed25519 or RSA public key injected via cloud-init.",
      },
    },
  },
  {
    id: "dex",
    name: "Dex OIDC Provider",
    description:
      "OpenID Connect identity provider providing authentication and federated user management.",
    type: "app",
    category: "security-identity",
    version: "0.21.0",
    repoUrl: "https://charts.dexidp.io",
    chart: "dex",
    runtime: "helm",
    placementScope: "cluster",
    flaggerSupported: false,
    defaultCanary: false,
    protected: false,
    schema: {
      type: "object",
      properties: {
        replicaCount: {
          type: "integer",
          title: "Replica Count",
          default: 1,
          minimum: 1,
          maximum: 5,
        },
      },
    },
  },
  {
    id: "flagger",
    name: "Flagger Progressive Delivery",
    description:
      "Automated canary promotion and rollback controller integrating with Envoy Gateway (Gateway API v1) and Prometheus.",
    type: "app",
    category: "progressive-delivery",
    version: "1.45.0",
    repoUrl: "https://flagger.app",
    chart: "flagger",
    runtime: "helm",
    placementScope: "cluster",
    flaggerSupported: false,
    defaultCanary: false,
    protected: false,
    schema: {
      type: "object",
      required: ["meshProvider", "metricsServer"],
      properties: {
        meshProvider: {
          type: "string",
          title: "Mesh / Gateway Provider",
          description:
            "Traffic routing provider used by Flagger for canary weight shifting.",
          default: "gatewayapi:v1",
          enum: ["gatewayapi:v1", "kubernetes", "istio", "nginx"],
        },
        metricsServer: {
          type: "string",
          title: "Prometheus Metrics Server URL",
          description:
            "In-cluster Prometheus URL queried by Flagger for canary metric checks.",
          default: "http://prometheus.keel-system:80",
        },
      },
    },
  },
  {
    id: "forgejo",
    name: "Forgejo Git Forge",
    description:
      "Self-service Forgejo Git forge (single replica, SQLite, persistent volume) with an optional public HTTPRoute on the shared keel-ingress Gateway and OpenID Connect login (e.g. a tenant Rauthy or Dex).",
    type: "app",
    category: "developer-tools",
    version: "17.3.3",
    repoUrl: "oci://code.forgejo.org/forgejo-helm",
    chart: "forgejo",
    runtime: "helm",
    flaggerSupported: false,
    defaultCanary: false,
    protected: false,
    schema: {
      type: "object",
      required: ["gitea"],
      properties: {
        gitea: {
          type: "object",
          title: "Forgejo",
          required: ["admin"],
          properties: {
            admin: {
              type: "object",
              title: "Bootstrap Administrator",
              required: ["username", "email", "password"],
              properties: {
                username: {
                  type: "string",
                  title: "Admin Username",
                  description: "Forgejo reserves 'admin'; pick another name.",
                  pattern: "^[A-Za-z0-9][A-Za-z0-9._-]{0,38}$",
                  not: { pattern: "^[Aa][Dd][Mm][Ii][Nn]$" },
                  default: "forgejo-admin",
                },
                email: {
                  type: "string",
                  title: "Admin Email",
                  format: "email",
                  default: "admin@keel.local",
                },
                password: {
                  type: "string",
                  title: "Admin Password",
                  description: "12-128 characters without whitespace.",
                  format: "password",
                  writeOnly: true,
                  pattern: "^\\S{12,128}$",
                },
              },
            },
          },
        },
        persistence: {
          type: "object",
          title: "Persistence",
          properties: {
            size: {
              type: "string",
              title: "Volume Size",
              description:
                "Fixed after install (resizing needs a manual PersistentVolumeClaim expansion).",
              pattern: "^[1-9][0-9]*(Mi|Gi|Ti)$",
              default: "10Gi",
            },
          },
        },
        httpRoute: {
          type: "object",
          title: "Public Route (keel-ingress)",
          properties: {
            enabled: {
              type: "boolean",
              title: "Expose Through keel-ingress",
              default: true,
            },
            hostnames: {
              type: "array",
              title: "Public Hostname",
              maxItems: 1,
              items: { type: "string", format: "hostname" },
            },
          },
        },
      },
    },
    uiSchema: {
      "ui:order": ["httpRoute", "gitea", "persistence"],
      gitea: {
        admin: {
          password: {
            "ui:widget": "password",
            "ui:help":
              "Encrypted via SOPS + per-tenant age key before committing to Git (never stored in plaintext). Leave blank on Edit to keep it.",
          },
        },
      },
      httpRoute: {
        hostnames: { items: { "ui:placeholder": "git-<tenant>.keel.local" } },
      },
    },
  },
  {
    id: "kyverno",
    name: "Kyverno Policy Engine",
    description:
      "Kubernetes-native policy engine (CEL ValidatingPolicy, MutatingPolicy, GeneratingPolicy and image verification) for a workload cluster. Install once per cluster.",
    type: "app",
    category: "security-identity",
    version: "3.10.0",
    repoUrl: "https://kyverno.github.io/kyverno/",
    chart: "kyverno",
    runtime: "helm",
    placementScope: "cluster",
    placementNamespace: "kyverno",
    flaggerSupported: false,
    defaultCanary: false,
    protected: false,
    schema: {
      type: "object",
      properties: {
        admissionController: {
          type: "object",
          title: "Admission Controller",
          properties: {
            replicas: {
              type: "integer",
              title: "Replica Count",
              description:
                "Use 3 for high availability (the layout Kyverno documents for HA).",
              default: 1,
              enum: [1, 3, 5],
            },
          },
        },
        backgroundController: {
          type: "object",
          title: "Background Controller",
          properties: {
            enabled: {
              type: "boolean",
              title:
                "Enable Background Controller (GeneratingPolicy, mutate-existing)",
              default: true,
            },
          },
        },
        reportsController: {
          type: "object",
          title: "Reports Controller",
          properties: {
            enabled: {
              type: "boolean",
              title: "Enable Policy Reports Controller",
              default: true,
            },
          },
        },
      },
    },
  },
  {
    id: "podinfo",
    name: "Podinfo Microservice",
    description:
      "Lightweight Go web application for verifying Flux HelmReleases, SOPS + age secret encryption, and Flagger + Envoy Gateway canary rollouts.",
    type: "app",
    category: "demo-microservice",
    version: "6.7.1",
    repoUrl: "https://stefanprodan.github.io/podinfo",
    chart: "podinfo",
    runtime: "helm",
    flaggerSupported: true,
    defaultCanary: false,
    protected: false,
    schema: {
      type: "object",
      required: ["replicaCount", "ingressHost", "apiToken"],
      properties: {
        replicaCount: {
          type: "integer",
          title: "Replica Count",
          description:
            "Number of podinfo pod replicas managed by the Deployment.",
          default: 2,
          minimum: 1,
          maximum: 10,
        },
        ingressHost: {
          type: "string",
          title: "Ingress Hostname",
          description:
            "DNS hostname exposed through Envoy Gateway HTTPRoute and monitored by Flagger.",
          format: "hostname",
          default: "podinfo.keel.local",
        },
        apiToken: {
          type: "string",
          title: "API Secret Token",
          description:
            "Sensitive bearer token partitioned out of plaintext values.yaml and encrypted via SOPS + tenant age key.",
          writeOnly: true,
          minLength: 8,
        },
      },
    },
    uiSchema: {
      ingressHost: {
        "ui:placeholder": "podinfo.keel.local",
        "ui:help":
          "Routed via Envoy Gateway HTTPRoute; traffic shifts progressively when Flagger canary delivery is enabled.",
      },
      apiToken: {
        "ui:widget": "password",
        "ui:help":
          "Marked writeOnly: true — automatically encrypted with SOPS + per-tenant age key before git staging (never stored in plaintext).",
      },
    },
  },
  {
    id: "prometheus",
    name: "Prometheus",
    description:
      "Metrics server scraping Envoy Gateway and cluster workloads for Flagger canary SLO evaluation (HTTP 5xx rate and p99 latency).",
    type: "app",
    category: "observability",
    version: "29.48.0",
    repoUrl: "https://prometheus-community.github.io/helm-charts",
    chart: "prometheus",
    runtime: "helm",
    placementScope: "cluster",
    flaggerSupported: false,
    defaultCanary: false,
    protected: false,
    schema: {
      type: "object",
      properties: {
        alertmanager: {
          type: "object",
          title: "Alertmanager",
          properties: {
            enabled: {
              type: "boolean",
              title: "Enable Alertmanager",
              default: false,
            },
          },
        },
        server: {
          type: "object",
          title: "Prometheus Server",
          properties: {
            fullnameOverride: {
              type: "string",
              title: "Service Name Override",
              default: "prometheus",
            },
            retention: {
              type: "string",
              title: "Retention Period",
              default: "7d",
            },
          },
        },
      },
    },
  },
  {
    id: "rauthy",
    name: "Rauthy OIDC Provider",
    description:
      "Self-service Rauthy OpenID Connect identity provider (single node, Hiqlite storage) with Admin UI, exposed on the shared keel-ingress Gateway.",
    type: "app",
    category: "security-identity",
    version: "0.37.2",
    repoUrl: "https://github.com/sebadob/rauthy",
    path: "./manifests.yaml",
    runtime: "manifest",
    defaultEngine: "flux",
    flaggerSupported: false,
    defaultCanary: false,
    protected: false,
    schema: {
      type: "object",
      required: [
        "hostname",
        "adminEmail",
        "encKeyId",
        "encKey",
        "hiqliteSecretRaft",
        "hiqliteSecretApi",
        "adminPassword",
      ],
      properties: {
        hostname: {
          type: "string",
          title: "Public Hostname",
          description:
            "Unique DNS name routed by the keel-ingress Gateway HTTPRoute (e.g. auth-<tenant>.keel.local); also Rauthy's issuer host (https://<hostname>[port]/auth/v1). Platform hosts (idp., keel., forgejo.) and hostnames already used by another app are rejected.",
          format: "hostname",
        },
        publicPortSuffix: {
          type: "string",
          title: "Public HTTPS Port Suffix",
          description:
            "Appended to the hostname in the issuer and WebAuthn origin. ':8888' for the local kind Gateway; empty when served on 443.",
          pattern: "^(:[0-9]{1,5})?$",
          default: ":8888",
        },
        adminEmail: {
          type: "string",
          title: "Bootstrap Admin Email",
          description:
            "Initial rauthy_admin account (created on first start only).",
          format: "email",
          default: "admin@keel.local",
        },
        logLevel: {
          type: "string",
          title: "Log Level",
          enum: ["error", "warn", "info", "debug", "trace"],
          default: "info",
        },
        adminForceMfa: {
          type: "boolean",
          title: "Require MFA for Admins",
          description:
            "Force rauthy_admin accounts to register a passkey before using the Admin UI (needs a browser-trusted certificate). Off by default so the bootstrap admin can sign in on the local kind CA; enable it after registering a passkey.",
          default: false,
        },
        storageSize: {
          type: "string",
          title: "Data Volume Size",
          description:
            "PersistentVolumeClaim size for the Hiqlite database (/app/data).",
          pattern: "^[1-9][0-9]*(Mi|Gi)$",
          default: "1Gi",
        },
        encKeyId: {
          type: "string",
          title: "Encryption Key ID",
          description:
            "Identifier of the active encryption key (ENC_KEY_ACTIVE), 2-20 alphanumeric characters. Not secret.",
          pattern: "^[a-zA-Z0-9]{2,20}$",
          default: "keel1",
        },
        encKey: {
          type: "string",
          title: "Encryption Key (base64, 32 bytes)",
          description:
            "Generate with: openssl rand -base64 32. Never change it after the first start without key rotation; data at rest is encrypted with it.",
          format: "password",
          writeOnly: true,
          pattern: "^[A-Za-z0-9+/]{43}=$",
        },
        hiqliteSecretRaft: {
          type: "string",
          title: "Hiqlite Raft Secret",
          description:
            "16-128 characters [A-Za-z0-9+/=_.-]. Generate with: openssl rand -base64 48",
          format: "password",
          writeOnly: true,
          pattern: "^[A-Za-z0-9+/=_.-]{16,128}$",
        },
        hiqliteSecretApi: {
          type: "string",
          title: "Hiqlite API Secret",
          description:
            "16-128 characters [A-Za-z0-9+/=_.-], different from the raft secret. Generate with: openssl rand -base64 48",
          format: "password",
          writeOnly: true,
          pattern: "^[A-Za-z0-9+/=_.-]{16,128}$",
        },
        adminPassword: {
          type: "string",
          title: "Bootstrap Admin Password",
          description:
            "14-128 characters without whitespace, double quotes, backslashes or '$'. Only applied on the very first start (empty database).",
          format: "password",
          writeOnly: true,
          pattern: '^[^"\\\\\\s$]{14,128}$',
        },
      },
    },
    uiSchema: {
      "ui:order": [
        "hostname",
        "publicPortSuffix",
        "adminEmail",
        "adminPassword",
        "logLevel",
        "adminForceMfa",
        "storageSize",
        "encKeyId",
        "encKey",
        "hiqliteSecretRaft",
        "hiqliteSecretApi",
      ],
      hostname: {
        "ui:placeholder": "auth-<tenant>.keel.local",
        "ui:help":
          "Exposed on the shared keel-ingress Gateway (HTTPS listeners); add a DNS / hosts entry pointing at the Gateway.",
      },
      encKey: {
        "ui:widget": "password",
        "ui:help":
          "Encrypted via SOPS + per-tenant age key before committing to Git (never stored in plaintext). Re-enter the same value on every Edit.",
      },
      hiqliteSecretRaft: {
        "ui:widget": "password",
        "ui:help":
          "Encrypted via SOPS + per-tenant age key before committing to Git.",
      },
      hiqliteSecretApi: {
        "ui:widget": "password",
        "ui:help":
          "Encrypted via SOPS + per-tenant age key before committing to Git.",
      },
      adminPassword: {
        "ui:widget": "password",
        "ui:help":
          "Encrypted via SOPS + per-tenant age key before committing to Git. Change it in the Rauthy Admin UI after the first login.",
      },
    },
  },
  {
    id: "seaweedfs",
    name: "SeaweedFS S3 Storage",
    description:
      "Self-service S3-compatible object storage (SeaweedFS master, volume, filer and authenticated S3 gateway on persistent volumes) with optional buckets and an optional public S3 HTTPRoute on the shared keel-ingress Gateway.",
    type: "app",
    category: "storage",
    version: "4.54.0",
    repoUrl: "https://seaweedfs.github.io/seaweedfs/helm",
    chart: "seaweedfs",
    runtime: "helm",
    flaggerSupported: false,
    defaultCanary: false,
    protected: false,
    schema: {
      type: "object",
      required: ["s3"],
      properties: {
        s3: {
          type: "object",
          title: "S3 Gateway",
          required: ["credentials"],
          properties: {
            credentials: {
              type: "object",
              title: "Admin Credentials",
              required: ["admin"],
              properties: {
                admin: {
                  type: "object",
                  title: "Admin Identity",
                  required: ["accessKey", "secretKey"],
                  properties: {
                    accessKey: {
                      type: "string",
                      title: "Access Key ID",
                      description:
                        "16-128 characters [A-Za-z0-9]. Generate with: openssl rand -hex 10",
                      format: "password",
                      writeOnly: true,
                      pattern: "^[A-Za-z0-9]{16,128}$",
                    },
                    secretKey: {
                      type: "string",
                      title: "Secret Access Key",
                      description:
                        "32-128 characters [A-Za-z0-9+/=]. Generate with: openssl rand -base64 30",
                      format: "password",
                      writeOnly: true,
                      pattern: "^[A-Za-z0-9+/=]{32,128}$",
                    },
                  },
                },
              },
            },
            createBuckets: {
              type: "array",
              title: "Buckets",
              description: "Created (if missing) on install and upgrade.",
              maxItems: 20,
              default: [{ name: "data" }],
              items: {
                type: "object",
                required: ["name"],
                properties: {
                  name: {
                    type: "string",
                    title: "Bucket Name",
                    pattern: "^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$",
                  },
                },
              },
            },
          },
        },
      },
    },
    uiSchema: {
      s3: {
        "ui:order": ["credentials", "createBuckets"],
        credentials: {
          admin: {
            accessKey: {
              "ui:widget": "password",
              "ui:help":
                "Encrypted via SOPS + per-tenant age key before committing to Git. Leave blank on Edit to keep it.",
            },
            secretKey: {
              "ui:widget": "password",
              "ui:help":
                "Encrypted via SOPS + per-tenant age key before committing to Git. Leave blank on Edit to keep it.",
            },
          },
        },
      },
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
    name: "podinfo",
    status: "Progressing",
    message: "Advance podinfo canary weight 40% (5xx rate 0.02%, latency 18ms)",
    revision: "6.7.1",
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
    provider: "aws",
    type: "workload",
    status: "synced",
    resourceCount: 5,
    lastSynced: "1 minute ago",
    resourceKind: "service",
  },
]

/**
 * Demo PRs across repositories: every tenant has its own repository in the `keel-tenants`
 * organisation (PR numbers repeat between repositories), platform changes live in the platform
 * repository (`management` scope).
 */
export const DEFAULT_PROPOSALS: Proposal[] = [
  {
    id: "7",
    title:
      "Provision Managed PostgreSQL Instance (postgres-managed) for tenant-acme",
    branch: "keel/acme/postgres-managed-7",
    tenant: "acme",
    status: "open",
    createdAt: "10 minutes ago",
    prUrl: "https://git.internal/keel-tenants/infra-tenant-acme/pulls/7",
    scope: "acme",
    repository: {
      label: "tenant-acme",
      fullName: "keel-tenants/infra-tenant-acme",
    },
    repoUrl: "https://git.internal/keel-tenants/infra-tenant-acme",
  },
  {
    id: "8",
    title:
      "Resize pve-workload-cluster-prod (proxmox-k3s-cluster) to 8 vCPU cores",
    branch: "keel/acme/proxmox-k3s-cluster-8",
    tenant: "acme",
    status: "open",
    createdAt: "25 minutes ago",
    prUrl: "https://git.internal/keel-tenants/infra-tenant-acme/pulls/8",
    scope: "acme",
    repository: {
      label: "tenant-acme",
      fullName: "keel-tenants/infra-tenant-acme",
    },
    repoUrl: "https://git.internal/keel-tenants/infra-tenant-acme",
  },
  {
    id: "7",
    title: "Install podinfo (podinfo) for tenant-globex",
    branch: "keel/globex/podinfo-7",
    tenant: "globex",
    status: "open",
    createdAt: "40 minutes ago",
    prUrl: "https://git.internal/keel-tenants/infra-tenant-globex/pulls/7",
    scope: "globex",
    repository: {
      label: "tenant-globex",
      fullName: "keel-tenants/infra-tenant-globex",
    },
    repoUrl: "https://git.internal/keel-tenants/infra-tenant-globex",
  },
  {
    id: "42",
    title: "chore(tenants): create tenant stark",
    branch: "keel/stark/create-tenant-42",
    tenant: "stark",
    status: "open",
    createdAt: "1 hour ago",
    prUrl: "https://git.internal/keel-io/infra/pulls/42",
    scope: "management",
    repository: { label: "platform" },
    repoUrl: "https://git.internal/keel-io/infra",
  },
]

/** Preview of acme PR #7: the `postgres-managed` blueprint's `Pulumi.yaml` for tenant-acme. */
export const DEFAULT_PREVIEW: PreviewReport = {
  prUrl: "https://git.internal/keel-tenants/infra-tenant-acme/pulls/7",
  policyStatus: "passed",
  source: "pko",
  run: { phase: "succeeded" },
  summary: {
    create: 3,
    update: 0,
    delete: 0,
    replace: 0,
    same: 0,
    drift: 0,
    total: 3,
  },
  resources: [
    {
      urn: "urn:pulumi:tenant-acme::postgres-managed::pulumi:pulumi:Stack::postgres-managed-tenant-acme",
      resource_type: "pulumi:pulumi:Stack",
      name: "postgres-managed-tenant-acme",
      op: "Create",
      changed_fields: [],
    },
    {
      urn: "urn:pulumi:tenant-acme::postgres-managed::pulumi:providers:aws::default",
      resource_type: "pulumi:providers:aws",
      name: "default",
      op: "Create",
      changed_fields: ["region"],
    },
    {
      urn: "urn:pulumi:tenant-acme::postgres-managed::aws:rds/instance:Instance::postgresInstance",
      resource_type: "aws:rds/instance:Instance",
      name: "postgresInstance",
      op: "Create",
      changed_fields: [
        "engine",
        "engineVersion",
        "instanceClass",
        "allocatedStorage",
        "storageEncrypted",
        "dbName",
        "password",
      ],
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
