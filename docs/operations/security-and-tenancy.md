# Security, Multi-Tenancy & Canary Rollouts

Keel's security model rests on least privilege. Tenants never hold control-plane credentials, every mutation goes through a reviewed Pull Request, and the control plane itself fails closed when something it relies on is missing.

## Multi-Tenant Isolation

Every tenant maps to a dedicated Kubernetes namespace (`tenant-<id>`) and a Git directory (`tenants/<id>/`):

- **Flux multitenancy lockdown.** Stage 0 installs the `flux2` chart (`2.20.0`) with `multitenancy.enabled: true`. This adds `--no-cross-namespace-refs=true` and `--default-service-account=default` to the kustomize and helm controllers. Any Flux object without `spec.serviceAccountName` impersonates the permissionless `default` ServiceAccount of its own namespace. Platform objects name a privileged controller identity instead: root and add-on Kustomizations impersonate `kustomize-controller`, and the core add-on HelmReleases live in `flux-system` and impersonate `helm-controller` (installing into `keel-system`). Every tenant Kustomization and HelmRelease sets `spec.serviceAccountName: tenant-<id>-sa`, and Kustomizations also get a `targetNamespace`. Flux impersonates `system:serviceaccount:tenant-<id>:tenant-<id>-sa` for local and remote reconciliation alike: when the object targets a workload cluster through `spec.kubeConfig.secretRef`, the impersonation happens on that workload cluster (see [Workload-Cluster Kubeconfigs](#workload-cluster-kubeconfigs)).
- **Tenant reconciler.** The root `keel-tenants` Kustomization, which applies `tenants/`, impersonates `flux-system/keel-tenant-reconciler` (`deploy/seed/flux-system/tenant-reconciler-rbac.yaml`), not the cluster-admin `kustomize-controller`. Its ClusterRole covers only the kinds Keel writes under `tenants/`, and it may bind only `ClusterRole/admin` (namespaced) or `system:auth-delegator`.
- **Placement guard for cluster-scoped apps.** Catalog apps with `spec.placement.scope: cluster` install CRDs, `ClusterRole`s or webhooks. Keel only proposes them onto a remote workload cluster and rejects other targets with `400`. The scope is declared by the catalog author (tenant admins can author their own entries), so it is a declarative guard for early errors and UX hints, not the enforcement boundary. Cluster-scoped objects on the management cluster are actually blocked because tenant Flux and PKO objects impersonate `tenant-<id>-sa`, which only has namespace-scoped RBAC (see the Flux multitenancy bullet above). See [Placement](../catalog/overview.md#placement-specplacementscope).
- **No access to Flux artifacts.** A Flux artifact is a tarball of a whole Git source; the platform artifact contains the entire GitOps repository, including every tenant's directory. The `source-controller` artifact server (pod port 9090, Service `source-controller.flux-system:80`) therefore only accepts connections from `flux-system` (`flux-system/allow-source-controller-artifacts`). Only the Flux controllers and the protected `management-cluster-core` Stack, which both run in `flux-system`, read artifacts. Tenant and preview PKO Stacks clone their own Git source directly (PKO `gitSource`). A tenant can widen its own egress, because it holds `admin` in its namespace, so the restriction is enforced on source-controller's side.
  - Clusters bootstrapped by Keel 0.9 or earlier allowed port 9090 from every namespace. Re-running `keel bootstrap init` restricts it to `flux-system` once no PKO Stack outside `flux-system` still uses `fluxSource`, live or committed under `tenants/` on `origin/main` of the `--repo-dir` repository. Until then port 9090 stays open to `flux-system` plus only the namespaces of those Stacks, so they keep working (also on a new or rebuilt cluster, where the policy does not exist yet), and the bootstrap summary lists them. Follow [Upgrading from Keel 0.9](upgrading.md): it re-runs `keel bootstrap init` before the migration (for `forge.publicUrl` and the Keel CA trust that migrated Stacks need) and again after it (to close port 9090).
  - Bootstrap fetches `origin` in `--repo-dir` and checks `origin/main`, so a clone that predates the merged migration PR does not block. If the fetch fails it checks the working copy instead and the summary says so; pull `--repo-dir` first in that case.
  - If the PKO Stacks cannot be listed, bootstrap keeps an existing policy as it is and recreates a missing one with the earlier open rule (port 9090 from every namespace), then asks you to re-run it.
- **Platform-owned tenant objects.** The objects Keel writes for a tenant carry `keel.io/platform-owned: "true"`: everything in `tenants/<id>/rbac.yaml` (namespace, ServiceAccount, quota, LimitRange, the `default-deny-all` and `allow-pko-workspace` NetworkPolicies, RoleBindings) and, for `tenant-default`, the `keel-gitops` GitRepository. Kyverno lets only the platform change them (see `protect-platform-owned-tenant-objects` below). Tenant app objects are never labelled.
  - `keel bootstrap init` creates the `tenant-default` baseline only when the namespace does not exist yet, before it installs the Kyverno policies. On re-runs it writes only the unlabelled tenant Secrets, so administrators of managed clusters (EKS access entries, GKE IAM, AKS Entra ID) are never blocked by the policy during bootstrap.
  - Tenants created before the label existed stay unprotected until relabelled, and `keel-server` logs a `platform-owned-baseline` warning for each of them at start-up. Run `keel tenant migrate-baseline --tenant <id>` and merge the PR it opens; it only adds the label to `tenants/<id>/rbac.yaml` (and `git-repository.yaml`, if present), plus the `allow-pko-workspace` NetworkPolicy when the baseline lacks it. `tenant-default` is relabelled by the next `keel bootstrap init --repo-dir <dir>`, which rewrites its seed files. Without `--repo-dir`, run `keel tenant migrate-baseline --tenant default` instead.
- **PKO workspace network access.** Every tenant baseline includes `allow-pko-workspace`. PKO workspace pods (`auto.pulumi.com/component: workspace`) accept the operator from `keel-system` on port 50051. Their egress is limited to:
  - cluster DNS;
  - the `keel-ingress` Envoy proxy pods, which serve the in-cluster Forgejo over HTTPS;
  - the in-cluster S3 state backend on port 8333;
  - the local-dev `flux-system/keel-docker-proxy` on port 2375 (kind-in-kind blueprints; deployed only with `--local-dev-docker-socket-exception`);
  - private addresses (`10.0.0.0/8`, `100.64.0.0/10`, `172.16.0.0/12`, `192.168.0.0/16`) on TCP 22, 443, 6443 and 8006: the Kubernetes API server, private forges and the Proxmox VE API;
  - public IPv4 addresses on any port, for cloud APIs and external forges.

  Link-local `169.254.0.0/16`, which holds the cloud instance-metadata endpoint, is never allowed. Other `keel-system` workloads, such as Forgejo's HTTP Service, the IdP, keel-server and the SeaweedFS admin and filer ports, are not allowlisted. Some CNIs also match pod IPs against `ipBlock` rules, so treat this as defense in depth. A private forge or provider API on another port needs an extra tenant NetworkPolicy. The policy selects pods by a label that tenants can set on any pod, and tenant identities can create their own NetworkPolicies. Default-deny for the remaining tenant pods therefore holds only for pods that do neither. Tenants created through the API by earlier releases lack it, so their Stacks cannot clone or report; `keel tenant migrate-baseline --tenant <id>` adds it. In `tenant-default`, earlier releases shipped an `allow-pko-workspace` that allowed all egress. Re-running `keel bootstrap init --repo-dir` replaces it with this allowlist, so first add a tenant NetworkPolicy for any private endpoints that default-tenant Stacks reach on other ports. `keel-previews` has no NetworkPolicy.
- **PKO clone URLs and CA trust.** PKO only clones `https://` and `ssh://` URLs. Tenant and preview Stacks clone the in-cluster Forgejo through its HTTPS origin (`forge.publicUrl` in `.keel/config.yaml`, platform configuration), while Flux keeps the `http://` Service URL. Any other `http://` source is refused with `409`. Workspaces and the PKO operator trust the Keel CA through the `keel-ca-bundle` ConfigMap, which holds only the public `ca.crt`; the CA and wildcard Secrets (private keys) are never mounted or copied. The mount is optional, so without a local CA (external forge with a public CA) the system roots apply.
- **No platform previews yet.** Every PKO preview is a tenant preview in `keel-previews`. Management add-on PRs are Flux-only and get no PKO preview, so there is no separate `keel-previews-platform` namespace yet. One will be added before any platform preview is introduced, so tenant preview code never runs next to a whole-repository credential.
- **Pod Security Standards.** Tenant namespaces enforce `restricted`. The Kyverno `disallow-privileged-containers` policy also covers init and ephemeral containers and host namespaces.
- **Per-tenant SOPS + `age` keys.** Secret form fields (`writeOnly: true` or `format: password`) are encrypted with the tenant's `age` public key before `git add`. The GUI masks them in the `values.yaml` preview.

## Identity & Authorization

`keel-server` requests the `openid`, `profile`, `email`, and `groups` scopes. It stores the verified claims in a stateless `AES-256-GCM`-encrypted session (the `keel_session` cookie for the browser, a Bearer token for the CLI).

| Role | Granted by | Can |
| :--- | :--- | :--- |
| Cluster admin | Group `keel-admins` (also `admin` / `admins`); during `BreakGlass`, only identities in `KEEL_BREAK_GLASS_ADMINS` | Everything: create and delete tenants, toggle providers, upgrade and roll back add-ons, manage the catalog, merge any PR. |
| Tenant admin | Group `tenant-<id>-admin` | Merge, reject, or reopen PRs whose diff stays inside `tenants/<id>/` (excluding `tenants/<id>/rbac.yaml`) and the tenant's entries in `tenants/kustomization.yaml`. Force-delete, unlock, and restart stacks. Fetch Helm schemas. Override an SSH target when the stack has none. |
| Tenant member | Group `tenant-<id>` | Read the tenant's stacks and releases, open proposals, request reconciliation, use SSH info. |

Only the `tenant-<id>` and `tenant-<id>-admin` group names grant tenant access, so an unrelated IdP group that happens to equal a tenant id can't grant access. Tenant ids ending in `-admin` are rejected at creation to avoid group-name collisions.

- **Phase-bound sessions.** Each session records the auth phase it was issued under (`OidcActive` or `BreakGlass`). A phase change invalidates every outstanding session.
- **Break-glass admins.** `KEEL_BREAK_GLASS_ADMINS` is a comma-separated list of OIDC `sub` or `email` values (case-insensitive) and defaults to `admin@keel.local`. Being in the `BreakGlass` phase does **not** make other users admins.
- **OIDC hardening.** PKCE `S256`, `state`, and a random `nonce` are sealed in the `keel_pkce` cookie, and the callback requires the `id_token` nonce to match. The CLI uses the RFC 8252 loopback flow with a 60-second, single-use code bound to its PKCE verifier.
- **CSRF / Origin.** Cookie-authenticated `POST`, `PUT`, `PATCH`, and `DELETE` requests must carry a same-origin `Origin`, `Referer`, or `Sec-Fetch-Site: same-origin|none`. Requests with none of these are rejected. Cookie-authenticated WebSocket upgrades (the SSH terminal) need a same-origin `Origin`. CLI `Authorization: Bearer` requests are exempt.
- **Server is authoritative.** The GUI hides controls you can't use, such as provider toggles, add-on upgrades, merge, force-delete, and unlock. The server enforces every one of these checks regardless.
- **Git audit trail.** Commits are authored as the OIDC user. Proposal create, merge, close, and reopen are audit-logged (`target: keel::audit`). Personal SSH keys and PATs are encrypted with `AES-256-GCM` in `keel-system`, keyed by OIDC `sub`, and never returned over the API.
- **Stage 0 Rauthy seeding.** Stage 0 creates the `keel-admins` and `tenant-default` groups, the `keel-server` PKCE client, and the initial admin. Manage users and groups in the Rauthy Admin UI at `https://idp.keel.local:8888/auth/v1/admin`.

## Tenant Credentials (Least Privilege)

Tenants never receive control-plane credentials:

- **Scoped S3 state identity.** Each tenant gets its own SeaweedFS identity, `keel-tenant-<id>`, limited to `Read`/`Write`/`List` on `pulumi-state/tenants/<id>/*`. The keys live in `tenant-<id>/keel-tenant-s3-credentials` and are mirrored to `tenant-<id>/keel-s3-credentials` for PKO `Stack.spec.envRefs`. Tenant `Stack`s use the backend `s3://pulumi-state/tenants/<id>?...`.
  - `keel bootstrap init` provisions `tenant-default`. `keel-server` provisions other tenants when they are created, and lazily on their first Stack proposal. It uses kube-rs `pods/exec` into the SeaweedFS filer and sends the `weed shell` script on stdin, so keys never appear in argv or logs.
  - If a tenant has no usable scoped identity, Stack proposals fail with `409 Conflict`. Admin keys are never substituted.
- **Per-tenant state encryption passphrase.** Pulumi `secret: true` config and outputs in a tenant's state are encrypted with that tenant's own passphrase. It is 256 bits of CSPRNG output, base64url-encoded, stored in `tenant-<id>/keel-pulumi-passphrase` (key `passphrase`), created once and never rotated automatically (it protects existing state, so back it up with your other cluster Secrets).
  - Every PKO `Stack` sets `PULUMI_CONFIG_PASSPHRASE` in its workspace pod through `valueFrom.secretKeyRef` to the namespace-local Secret. Keel never writes a literal passphrase into a `Stack`, and a workspace pod whose Secret is missing does not start.
  - The passphrase is created together with the scoped S3 identity: `keel bootstrap init` for `tenant-default`, and `keel-server` on tenant creation and on lazy tenant provisioning. Stack proposals for a tenant without a usable passphrase fail with `409 Conflict`; the proposal path never generates a replacement.
  - Management Stacks use their own Secrets, `flux-system/keel-pulumi-passphrase` and `keel-system/keel-pulumi-passphrase`, created by `keel bootstrap init`.
  - `keel tenant migrate-state` reads the tenant passphrase from the cluster and passes it to `pulumi` through the environment only; it refuses to run if the Secret is missing.
- **Read-only tenant Git token.** `tenant-<id>/git-credentials` holds a read-only token for the tenant Flux `GitRepository`. With the in-cluster Forgejo, Stage 0 mints a dedicated `read:repository` token. With an external forge (`--git-url` / `--git-token`), the operator token is **not** shared: set `KEEL_TENANT_GIT_TOKEN` to a read-only token when running `keel bootstrap init` if the tenant repository needs authentication.
- **Previews** use the same scoped tenant credentials, mirrored into `keel-previews`, and a read-only workspace ServiceAccount. The tenant passphrase is copied into `keel-previews` as `keel-preview-<pr>-keel-pulumi-passphrase`, owned by the preview `Stack` and garbage-collected with it. See [Pre-Merge PKO Previews](../getting-started/previews.md).
- **No credential cloning.** No policy or controller copies control-plane S3 admin keys or Git tokens into tenant namespaces. `keel bootstrap init` removes any such copies it finds (Secrets labelled `keel.io/secret-kind` `s3-credentials` / `git-credentials` in `tenant-*` namespaces).

## Kyverno Admission Guardrails

Kyverno `3.10.0` runs as a single release in `keel-system`:

- **`restrict-pulumi-providers`.** PKO `Stack`s may only use providers enabled in `providers/registry.yaml`, plus Pulumi's built-in providers (always including `kubernetes`). Stage 0, `keel serve`, and provider PRs all build this policy with one shared helper, so they can't drift apart.
- **`restrict-tenant-reconciler`** (`failurePolicy: Fail`). Confines writes by `keel-tenant-reconciler` to `tenant-*` namespaces. Allows only `system:auth-delegator` `ClusterRoleBinding`s for tenant ServiceAccounts. Rejects tenant `RoleBinding`s that reference ServiceAccounts from other namespaces, except for binding `ClusterRole/keel-server-tenant` to `keel-system/keel-server`.
- **`restrict-tenant-route-hostnames`** (`failurePolicy: Fail`). Denies Gateway API routes (`HTTPRoute`, `GRPCRoute`, `TLSRoute`) in `tenant-*` namespaces whose hostnames are a platform host (the IdP host, `idp.`/`keel.`/`forgejo.` under the Keel domain, and the `keel.local` defaults) or a wildcard covering one. `keel bootstrap init` renders the list from `--oidc-public-url`. Tenant routes share the `keel-ingress` listeners, so without it a tenant could take over the IdP issuer host. Tenant ServiceAccounts may create routes through `ClusterRole/keel-tenant-gateway-routes`, which is aggregated into `admin`.
- **`protect-control-plane`.** Denies deletion of `management-cluster-core`, of any PKO `Stack` labelled `keel.io/protect: "true"`, and of any `flux-system` HelmRelease or Kustomization labelled `keel.io/protect: "true"` (the management add-ons, such as `flux2`; see [Break-Glass Recovery](break-glass-recovery.md#flux-self-upgrade-failures) for deliberate deletion). Also blocks every tenant and `keel-previews` ServiceAccount from mutating anything in `flux-system` or `keel-system`, and limits `keel-system/keel-server` to reconcile-annotation patches (`reconcile.fluxcd.io/requestedAt` / `pulumi.com/reconcile-request`) on Flux objects and Stacks in those namespaces.
- **`restrict-tenant-flux-sources`** (`failurePolicy: Fail`). Only evaluates writes in `tenant-*` namespaces, so `flux-system` never depends on it:
  - Denies `ExternalArtifact` and every `source.extensions.fluxcd.io` kind (`ArtifactGenerator`) in `tenant-*` namespaces, for every requester. These objects republish an artifact; the flux2 source-watcher stays disabled.
  - Requires Flux Kustomizations in `tenant-<id>` to set `spec.serviceAccountName: tenant-<id>-sa`.
  - For both rules, updates that keep `spec` unchanged (finalizer removal, reconcile annotations) and objects being deleted always pass, and deletion is never blocked.
- **`restrict-tenant-flux-source-status`** (`failurePolicy: Fail`). Denies writes to the `status` of Flux source objects by tenant identities (`tenant-*` ServiceAccounts, the `keel-previews` identity, and `tenant-*` groups), in any namespace. A forged `status.artifact` would otherwise make kustomize-controller fetch someone else's artifact. The tenant-identity check is part of the policy's match conditions, so `source-controller`'s own status writes never reach Kyverno and an unavailable Kyverno cannot stall Flux.
- **`protect-platform-owned-tenant-objects`** (`failurePolicy: Fail`). Only `flux-system/keel-tenant-reconciler` (used by `keel-tenants` to apply `tenants/<id>/`), `keel-system/keel-server`, `source-controller` (GitRepositories only, for its finalizer), the `kube-system` controllers and break-glass cluster admins (`system:masters`, `kubeadm:cluster-admins`) may create, change or delete objects labelled `keel.io/platform-owned`, including removing the label. `kustomize-controller` is deliberately not allowed: every platform-owned object is applied through `keel-tenants`, and allowing it would let any `flux-system` Kustomization rewrite tenant baselines. Tenant identities are denied even though they hold `admin` in their namespace; change these objects through a PR to `tenants/<id>/` instead. One change is exempt: an update of the `keel-gitops` GitRepository that only sets the `reconcile.fluxcd.io/requestedAt` annotation, so `flux reconcile source git keel-gitops -n tenant-<id>` works for anyone RBAC allows to patch it. Changing any other field, label or annotation of `keel-gitops` is still denied. The manual `kubectl apply -k deploy/bootstrap/keel-server` install ships no tenant objects, so it is never denied by this policy. On managed clusters whose administrators are not in those groups, use Git, or remove the policy temporarily as a break-glass step.
- **`sync-keel-ca-bundle`** (`GeneratingPolicy`). Copies `ca.crt` of `keel-system/keel-ca-bundle` (the public Keel CA certificate) into a `keel-ca-bundle` ConfigMap in `keel-previews` and every `tenant-*` namespace, new and existing, and restores edited or deleted copies. It reads no Secret, and its RBAC (`keel-kyverno-ca-bundle-sync`) covers only ConfigMaps. After a CA rotation, re-run `keel bootstrap init`: it rewrites the source and every existing copy and restarts the PKO operator.
- **Docker socket.** Host Docker socket mounts are blocked by default. On local `kind`, `--local-dev-docker-socket-exception` adds a `PolicyException` scoped to PKO workspace pods.

## Workload-Cluster Kubeconfigs

Tenant `*-kubeconfig` Secrets, written by cluster blueprints, are untrusted input. `keel-server` lists them only inside the owning tenant namespace and validates them before building a client:

- **Rejected:** `exec` credential plugins, `auth-provider`, and file-path credentials (`tokenFile`, `client-certificate`, `client-key`, `certificate-authority`). These would let a tenant run binaries in, or read files from, `keel-server`.
- **Accepted:** inline credentials only (`token`, `username`/`password`, `client-*-data`, `certificate-authority-data`).

Flux controllers cannot run `exec` plugins either. Cluster blueprints therefore never write cloud-CLI kubeconfigs:

- **Static Flux credential.** The `aws-eks-cluster`, `gcp-gke-cluster`, and `azure-aks-cluster` blueprints authenticate their own Kubernetes provider with the deployer's short-lived cloud credential (`aws:eks:getClusterAuth`, `gcp:organizations:getClientConfig`, or the AKS admin credential), create a `kube-system/keel-flux` ServiceAccount bound to `cluster-admin` with a long-lived `kubernetes.io/service-account-token` Secret, and write a kubeconfig that carries that inline token and the cluster CA. `kind-cluster` writes the kind admin kubeconfig (client certificate) with the API server rewritten to its in-network address.
- **Tenant impersonation target.** Because Flux impersonates the tenant ServiceAccount on the workload cluster, every cluster blueprint also creates the `keel-tenant-flux` ClusterRoleBinding there, binding `cluster-admin` to `ServiceAccount tenant-<id>/tenant-<id>-sa`. The tenant owns the cluster outright, so its reconciliation identity is cluster-admin on that cluster only; it has no extra rights on the management cluster.

Any other kubeconfig used as a Flux target (see [Provisioning](../getting-started/provisioning.md#bring-your-own-workload-cluster)) must follow the same rules.

## Proposal Integrity

- **SHA-pinned merges.** The server resolves the PR head once, checks the tenant path scope against `main...<sha>`, and asks the forge to merge exactly that SHA. A push in between is rejected.
- **Path safety.** `..`, absolute paths, and backslashes in kustomization entries are rejected.
- **Template injection.** `${var}` values that make up a whole YAML value are quoted. Values used mid-string are validated. The rendered output is re-parsed and its document count and kinds are checked before commit.
- **Diffs.** PR diffs are merge-base (`main...head`) diffs, so old PRs don't show unrelated changes.
- **Stack deletions on merge** are derived from the `stack.yaml` files the PR deletes, never from the PR title.

## Helm Schema Fetching

`POST /api/v1/catalog/fetch-schema` is limited to cluster admins and tenant admins. For HTTP(S) chart repositories, DNS is resolved once and pinned. Private, loopback, link-local, and mapped addresses are rejected, and each redirect is re-validated. Index, archive, and unpacked sizes are capped, and no temp files are written. OCI repositories use `helm pull`, and the child process is killed on timeout.

## HTTP Security Headers

Every response carries these headers (`crates/keel-server/src/security.rs`):

- `Content-Security-Policy: default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; font-src 'self' data:; frame-ancestors 'none'; base-uri 'self'; form-action 'self' <issuer-origin>`. Responses include the SPA, its assets, and the REST/SSE API.
  - `'wasm-unsafe-eval'` is there for the WASM core of the SSH terminal (wterm). It does **not** allow JavaScript `eval`.
  - `form-action` adds the OIDC issuer origin so the login redirect can complete.
- `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, and `X-Frame-Options: DENY`.

If you put a proxy in front of `keel-server` that rewrites CSP, keep `'wasm-unsafe-eval'` in `script-src`. Without it, the browser terminal can't start.

## Error Responses

5xx responses never include git, helm, or kube stderr or server paths. The body is `{"error": "...", "correlationId": "..."}`, and the full error is logged server-side under the same correlation id. The GUI shows the correlation id so you can find the log entry.

## Control-Plane Anti-Lockout

- Management-cluster core stacks carry `protect: true` (`clusters/management/management-stack.yaml`). Force-delete refuses stacks labelled `keel.io/protect`.
- Stage 0 core resources are `protect: true` in `deploy/bootstrap/control-plane/Pulumi.yaml`. To tear them down with Pulumi, first run `pulumi state unprotect --all` against that stack, or simply delete the cluster.
- `keel bootstrap recover` re-applies the escrowed Stage 0 state from versioned S3. See [Break-Glass Recovery](break-glass-recovery.md).

## VM SSH Access

1. **Tenant RBAC.** `GET /api/v1/stacks/{ns}/{name}/ssh-info` and the WebSocket terminal `GET /api/v1/stacks/{ns}/{name}/ssh/ws` require tenant access. Unknown stacks return `404`, and stacks that are not VMs return an error. Keel never invents connection info.
2. **Target restrictions.** Targets resolved from the stack (Pulumi outputs, `values.yaml`, `stack.yaml`) are trusted because they come from the reviewed GitOps pipeline. A client-supplied host or port is accepted only if it equals the resolved target. Only when the stack has **no** resolved endpoint may a **tenant admin** supply one, and it must not be private (RFC 1918), loopback, link-local, CGNAT, IPv6 ULA, or a cluster-internal name such as `*.svc`. DNS is resolved and every address is checked before connecting, which defeats DNS rebinding.
3. **Host-key verification.** The server computes the `SHA256:` fingerprint of the key the server presents and shows it in the GUI with a verified or unverified badge. If the stack publishes `sshHostKeyFingerprint` or `sshHostKey` outputs (a fingerprint or OpenSSH public key, as a comma or newline separated list), the key is **enforced** and a mismatch aborts the connection. A malformed pin fails closed. The bundled VM blueprints do not publish these outputs yet, so connections show as unverified until you add them, for example from cloud-init.
4. **No private keys over REST.** Profile SSH keys are decrypted only in server memory for the `russh` handshake.
5. **No shell interpolation.** `keel infra ssh --exec` runs `ssh` with an explicit argv.
6. **Audit.** Every connection attempt is logged with the user, client IP, tenant, stack, and resolved target.

## Progressive Delivery (Flagger + Envoy Gateway)

Applications whose `CatalogApplication` sets `spec.progressiveDelivery.flaggerSupported: true` can opt into canary rollouts (`--canary` in the CLI, or **Enable Flagger Canary** in the GUI). Keel then generates:

- **`helmrelease-canary.yaml`**: the Flux `HelmRelease`.
- **`canary.yaml`**: a Flagger `Canary` (`provider: gatewayapi:v1`) targeting `keel-system/keel-ingress`. It gates on `envoy-gateway-request-success-rate` ≥ 99% and `envoy-gateway-request-duration` p99 ≤ 500 ms over `2m` windows.
- **`networkpolicy-canary.yaml`**: allows ingress from Envoy proxy pods in `keel-system` through `default-deny-all`.

For an app on a remote workload cluster, `canary.yaml` and `networkpolicy-canary.yaml` go into `<app>/canary/` and are applied there by a separate `<instance>-canary` Flux `Kustomization` (`canary-kustomization.yaml`). That directory also contains `namespace.yaml`, which creates the target namespace on the workload cluster so the canary objects don't wait for Helm to create it. The namespace is annotated `kustomize.toolkit.fluxcd.io/prune: disabled` and `kustomize.toolkit.fluxcd.io/ssa: IfNotPresent`, so Flux never deletes it or overwrites an existing one. Bundle canary items whose `HelmRelease` targets a remote cluster use the same layout under the item directory (`<bundle>-<item>-canary`).

### Helm app secrets, routes and network policy

Helm apps keep their secret fields in the SOPS-encrypted `<instance>-secrets` Secret in `<app>/secrets.enc.yaml`, listed in `tenants/kustomization.yaml` and applied in the tenant namespace of the management cluster by the root `keel-tenants` Kustomization. The `HelmRelease` reads it through `spec.valuesFrom`, so the plaintext never appears in Git or in the `HelmRelease` object. Helm stores the merged values in its release Secret on the target cluster, as for any Helm value.

Catalog entries declare the routes their chart renders (`spec.gatewayRoutes`). Keel rejects an enabled route without hostnames (it would catch every host on the shared listeners), applies the same platform-host and uniqueness checks as for manifest app routes, and pins the route to the `keel-ingress` HTTPS listeners. The `restrict-tenant-route-hostnames` policy still checks the rendered route at admission. Entries with `spec.networkPolicy` get a Keel-generated `NetworkPolicy` through `default-deny-all` when installed on the management cluster. Its egress to `keel-system` only reaches the `keel-ingress` Envoy proxy pods on their listener ports, never the state backend, the management forge or keel-server, and public egress is limited to TCP 80/443 outside private and reserved ranges. `spec.lockedValues` carry settings the form cannot change, such as the `restricted` Pod Security contexts of the built-in `forgejo` and `seaweedfs` apps. Entries with locked values or routes also reject any value their schema does not declare, so chart options like `extraDeploy`, sidecars, extra routes or a ServiceAccount override cannot bypass those settings. No Helm app value may name the tenant reconciler ServiceAccount. See [Helm app secrets, routes and NetworkPolicy](../catalog/overview.md#helm-app-secrets-routes-and-networkpolicy).

### Manifest app secrets on remote clusters

Flux-engine manifest apps keep their substitution sources (`<instance>-values` ConfigMap and the SOPS-encrypted `<instance>-secrets` Secret) under `<app>/config/`, applied by a separate `<instance>-config` Kustomization in the tenant namespace of the management cluster. The app Kustomization depends on it and substitutes from it, so a remote app never receives the `<instance>-secrets` Secret. The substituted values do end up in the rendered objects on the workload cluster (for example the app's own Secret), just as Helm values do for a remote HelmRelease. Only target workload clusters you trust with those values.

Namespaces that Keel creates on workload clusters (`namespace.yaml` of remote manifest apps and remote canaries) carry the same `restricted` Pod Security Standards labels as `tenant-<id>` (`pod-security.kubernetes.io/enforce`, `audit` and `warn` set to `restricted`). Workload clusters have no guaranteed Kyverno baseline, so these labels are what enforce PSS there. Because the namespace uses `ssa: IfNotPresent`, an existing namespace keeps its own labels.

Charts deployed into tenant namespaces must have a `restricted`-compliant `securityContext` (`allowPrivilegeEscalation: false`, `runAsNonRoot: true`, a numeric `runAsUser`, `capabilities.drop: ["ALL"]`).
