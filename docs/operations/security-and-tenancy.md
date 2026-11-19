# Security, Multi-Tenancy & Canary Rollouts

## Multi-Tenant Isolation

Every tenant maps to a dedicated Kubernetes namespace (`tenant-<tenant-id>`) and Git directory (`tenants/<tenant-id>/`):

- **Flux Isolation:** Flux controllers run with `--no-cross-namespace-refs=true`, preventing one tenant from referencing another tenant's `GitRepository`, `Secret`, or `Kustomization`.
- **Pod Security Standards:** Tenant namespaces enforce the Kubernetes `restricted` Pod Security Standard.
- **Per-Tenant SOPS + `age` Keys:** Secrets submitted through catalog forms (`writeOnly: true` or `format: password`) are encrypted with the tenant's `age` public key before being committed to Git.

## OIDC Identity & Tenant RBAC Mapping

`keel-server` requests the `openid`, `profile`, `email`, and `groups` OIDC scopes by default and stores the verified `id_token` claims (`sub`, `email`, `name`, and `groups`) inside the stateless, `AES-256-GCM`-encrypted `keel_session` cookie. Signing out via `POST /api/v1/auth/logout` clears both `keel_session` and `keel_pkce` (`Max-Age=0`).

Authorization in `keel-server` maps OIDC `groups` claims to cluster-admin and tenant-scoped permissions:

- **Cluster Administrators:** Users whose `groups` claim includes `keel-admins`, `admin`, or `admins` (or sessions running in `BreakGlass` auth phase) have full cluster-admin access across all tenants and control-plane endpoints (such as creating/deleting tenants, toggling providers, managing catalog entries, and merging any PR).
- **Tenant Administrators:** Users whose `groups` claim includes `tenant-<tenant-id>-admin` or `<tenant-id>-admin` have tenant-scoped access to `<tenant-id>` and can merge, reject, or reopen PRs (`keel/<tenant-id>/...`) whose git diff is strictly confined to `tenants/<tenant-id>/` (excluding `tenants/<tenant-id>/rbac.yaml`) and `<tenant-id>/` entries in `tenants/kustomization.yaml`. Tenant admins cannot create/delete tenants, modify tenant RBAC/quotas, or touch other tenants or control-plane directories.
- **Tenant Members:** Non-admin users are granted access to a specific tenant `<tenant-id>` when their `groups` claim contains either `<tenant-id>` or `tenant-<tenant-id>` (matching the tenant's Kubernetes namespace). This is enforced on tenant-scoped reads and mutations (such as listing stacks/releases, opening PR proposals, deleting GitOps resources, restarting PKO `Stack` workspaces, and triggering Flux reconciliation), while PR merging/rejection remains restricted to cluster or tenant administrators.
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

## Security Model for Virtual Machine SSH Access

Keel provides direct SSH inspection and terminal connectivity for provisioned VMs with strict defense-in-depth controls:

1. **Tenant RBAC Enforcement:**
   - Stack SSH metadata (`GET /api/v1/stacks/{namespace}/{name}/ssh-info`) and interactive browser WebSocket sessions (`GET /api/v1/stacks/{namespace}/{name}/ssh/ws`) strictly enforce `auth.require_tenant_access(tenant)`.
   - Operators can only inspect or access VMs within tenant namespaces they are authorized to manage (`tenant-<id>-admin`, `<tenant-id>-admin`, or `keel-admins`). Cross-tenant access is rejected at the API boundary.

2. **Zero Plaintext Private Key Exposure Over REST:**
   - The REST endpoint returns only connection metadata (`sshHost`, `sshUser`, `sshPort`, provider) and public command strings.
   - Private keys are never returned in REST responses, serialized in logs, or exposed to unauthorized clients (`NFR-SEC-05`).

3. **In-Memory `AES-256-GCM` Key Decryption:**
   - User SSH private keys saved in personal profiles are encrypted at rest with `AES-256-GCM` inside Kubernetes `Secret`s in `keel-system`.
   - When launching an in-browser terminal session via the pure-Rust `russh` WebSocket bridge, the private key is decrypted exclusively in server memory for the duration of the SSH handshake and scrubbed upon disconnection.

4. **Command Injection Defense (`NFR-SEC-06`):**
   - The `keel infra ssh <stack> --exec` CLI command executes `tokio::process::Command::new("ssh")` with explicit argument vectors attached directly to the active process terminal.
   - Keel never invokes `sh -c` or interpolates user strings into shell command lines, eliminating shell injection vulnerabilities.

5. **Structured Audit Logging (`NFR-OBS-03`):**
   - Every VM SSH connection attempt (CLI or WebSocket bridge) produces a structured audit log event recording the authenticated OIDC user identity (`sub`, `email`), client IP, target tenant, stack name, resolved host/port, and timestamp.
