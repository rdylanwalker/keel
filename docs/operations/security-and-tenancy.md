# Security, Multi-Tenancy & Canary Rollouts

## Multi-Tenant Isolation

Every tenant maps to a dedicated Kubernetes namespace (`tenant-<tenant-id>`) and Git directory (`tenants/<tenant-id>/`):

- **Flux Isolation:** Flux controllers run with `--no-cross-namespace-refs=true`, preventing one tenant from referencing another tenant's `GitRepository`, `Secret`, or `Kustomization`.
- **Pod Security Standards:** Tenant namespaces enforce the Kubernetes `restricted` Pod Security Standard.
- **Per-Tenant SOPS + `age` Keys:** Secrets submitted through catalog forms (`writeOnly: true` or `format: password`) are encrypted with the tenant's `age` public key before being committed to Git.

## OIDC Identity & Tenant RBAC Mapping

`keel-server` requests the `openid`, `profile`, `email`, and `groups` OIDC scopes by default and stores the verified `id_token` claims (`sub`, `email`, `name`, and `groups`) inside the stateless, `AES-256-GCM`-encrypted `keel_session` cookie.

Authorization in `keel-server` maps OIDC `groups` claims to cluster-admin and tenant-scoped permissions:

- **Cluster Administrators:** Users whose `groups` claim includes `keel-admins`, `admin`, or `admins` (or sessions running in `BreakGlass` auth phase) have full cluster-admin access across all tenants and control-plane endpoints (such as creating/deleting tenants, toggling providers, and managing catalog entries).
- **Tenant Members:** Non-admin users are granted access to a specific tenant `<tenant-id>` when their `groups` claim contains either `<tenant-id>` or `tenant-<tenant-id>` (matching the tenant's Kubernetes namespace). This is enforced on tenant-scoped mutations and operations (such as opening PR proposals, deleting GitOps resources, restarting PKO `Stack` workspaces, and triggering Flux reconciliation).
- **Git Audit Trail & Personal Credentials:** Every PR opened through Keel records the authenticated OIDC user (`name`/`sub` and `email`) as the Git commit author. Personal Git SSH keys and Forge PATs are encrypted with `AES-256-GCM` and stored in `keel-system` keyed by the user's OIDC `sub`.
- **Stage 0 Rauthy Seeding:** Stage 0 bootstraps in-cluster Rauthy (`keel-system/rauthy-config`) with `keel-admins` and `tenant-default` groups, the `keel-server` PKCE client, and the initial admin user assigned to `keel-admins` and `rauthy_admin`. Additional users and tenant groups (`<tenant-id>` or `tenant-<tenant-id>`) can be created in the Rauthy Admin UI (`http://idp.keel.local:8888/auth/v1/admin`).
- **`TenantConfig.oidc_groups`:** `TenantConfig` reserves an `oidc_groups` field for custom group-to-tenant bindings; currently `keel-server` enforces direct `<tenant-id>` / `tenant-<tenant-id>` group matching.

## Kyverno Admission Guardrails

Keel deploys Kyverno policies during Stage 0 bootstrap to enforce:

- **Provider Allowlisting:** PKO v2 `Stack`s can only execute providers that are enabled in `providers/registry.yaml`.
- **Docker Socket Protection:** Host Docker socket mounts are blocked by default. On local `kind` development clusters, passing `--local-dev-docker-socket-exception` to `keel bootstrap init` generates an explicit `PolicyException` scoped to PKO workspace pods so they can provision local workload `kind` clusters.
- **Tenant Secret Synchronization (`sync-tenant-secrets`):** A Kyverno `GeneratingPolicy` (`policies.kyverno.io/v1`, `generateExisting` and `synchronize` enabled) with scoped RBAC (`keel-kyverno-tenant-secret-sync`) automatically clones `flux-system/keel-s3-credentials` and `flux-system/git-credentials` into `tenant-default` and every newly created `tenant-<id>` namespace, keeping credentials synchronized without violating Flux's `--no-cross-namespace-refs=true`.

## Control-Plane Anti-Lockout

- Management-cluster core stacks carry `protect: true` (`clusters/management/management-stack.yaml`) so accidental deletion PRs cannot destroy the control plane.
- Break-glass recovery (`keel bootstrap recover`) restores management state directly from versioned S3 object storage.

## Progressive Delivery (Flagger + Envoy Gateway)

Applications whose `CatalogApplication` declares `spec.progressiveDelivery.flaggerSupported: true` can opt into automated canary rollouts (`--canary` in CLI or the **Enable Flagger Canary** checkbox in the GUI deploy modal).

When canary delivery is enabled, Keel synthesizes:

- **`helmrelease-canary.yaml`**: Standard Flux `HelmRelease` for the application.
- **`canary.yaml`**: A Flagger `Canary` (`flagger.app/v1beta1`, `provider: gatewayapi:v1`) targeting `keel-system/keel-ingress` and evaluating Prometheus metrics (`envoy-gateway-request-success-rate` ≥ 99% and `envoy-gateway-request-duration` p99 ≤ 500ms over `2m` windows) installed during Stage 0.
- **`networkpolicy-canary.yaml`**: A tenant-scoped `NetworkPolicy` permitting TCP ingress on the application's service port from Envoy Gateway proxy pods in `keel-system` through `default-deny-all`.

Because tenant namespaces enforce the `restricted` Pod Security Standard via Kyverno, application charts deployed into tenant namespaces must include a compliant `securityContext` (e.g., `allowPrivilegeEscalation: false`, `runAsNonRoot: true`, a numeric `runAsUser`, and `capabilities.drop: ["ALL"]`).
