# Keel Control Plane for a Workload Cluster (`keel-server` app)

The built-in `keel-server` catalog app runs a **separate Keel control plane** on a remote workload cluster. Use it when a team should get its own Keel: its own GitOps repository, its own OIDC client and admin group, its own tenants, and the same PR + ephemeral PKO preview workflow, on a cluster it owns. The management plane still installs, upgrades and audits that server like any other tenant app, through a PR.

It is not the management plane's own server. That one is `catalog/core/keel-server`, which `keel bootstrap init --with-keel-server` installs (see [Running keel-server In-Cluster](../operations/in-cluster-server.md)) and admins manage from the Topology Map.

> **Status: experimental.** The app is covered by unit and API tests, but not yet verified end to end on a live workload cluster.

## What it Installs

The app is a `runtime: manifest` app (Flux engine). It reuses the management add-on's known-good install config, and tests fail when the copies drift apart:

| Object | Namespace | Source |
| :--- | :--- | :--- |
| `ServiceAccount`, `ClusterRole`s `keel-server-read` / `keel-server-tenant`, the `keel-server` `Role`s and `RoleBinding`s | `keel-system`, `keel-previews`, `flux-system` | `deploy/bootstrap/keel-server/rbac.yaml`, with narrower Kyverno access (see below) |
| `ValidatingAdmissionPolicy` and `ValidatingAdmissionPolicyBinding` `keel-server-reconcile-only` | cluster-scoped | The keel-server rule of Keel's Kyverno `protect-control-plane` policy, ported to native admission |
| Preview workspace `ServiceAccount` `keel-preview-readonly` and its `view` / `system:auth-delegator` bindings | `keel-previews` | `deploy/bootstrap/manifests/preview-workspace-rbac.yaml` |
| `Deployment` and `Service` `keel-server` | `keel-system` | `deploy/bootstrap/keel-server/deployment.yaml` (same pod spec, `restricted` PSS) |
| `Secret` `keel-server-credentials` (Git token, OIDC client secret) | `keel-system` | Substituted by Flux, never in Git |
| `HTTPRoute` `keel-server-ingress` | `keel-system` | `deploy/seed/addons/keel-server/httproute.yaml`, attached to the Gateway you choose |

Differences from the management bundle, because the workload cluster does not have Keel's management-plane policies:

- **Reconcile requests only.** The server may `patch` Flux `Kustomization`s, `HelmRelease`s, `GitRepository`s and PKO `Stack`s in `flux-system` so the portal can request a reconcile. On the management cluster, Keel's Kyverno `protect-control-plane` policy limits that to the reconcile annotations. That policy does not exist on a workload cluster, so the app ships the same CEL rule as a native `ValidatingAdmissionPolicy` (Kubernetes 1.30 or later, no Kyverno needed). An update by `keel-system/keel-server` to those objects in `flux-system` or `keel-system` may only add or change `reconcile.fluxcd.io/requestedAt` / `pulumi.com/reconcile-request`. Spec, label, finalizer and other annotation changes are denied, so a leaked token cannot point the cluster-admin `kustomize-controller` at other content. A test keeps the rule identical to `deploy/seed/kyverno-policies/protect-control-plane.yaml`.
- **Kyverno writes withheld.** `keel-server-read` reads Kyverno `policies.kyverno.io` objects cluster-wide, but can only write the `restrict-pulumi-providers` `ValidatingPolicy` (the provider allowlist the server re-applies from `providers/registry.yaml`). It cannot create or change `PolicyException`s or `GeneratingPolicy`s, which would let a leaked token exempt workloads from the team's policies or generate objects with Kyverno's own permissions.

The image is `ghcr.io/rdylanwalker/keel`, pinned to the same release as `catalog/core/keel-server` and the `keel` crate version. semantic-release bumps both entries in the Keel source tree with every release, and a test fails if they drift. Renovate does not touch this image. Your GitOps repository keeps the copy `keel bootstrap init` seeded into `catalog/apps/keel-server/` (seeding only happens once, into an empty catalog), and manifest apps have no version picker on the **Edit** form. See [Upgrade](#upgrade).

Placement:

- `scope: cluster`: the app ships `ClusterRole`s and an admission policy, so it only installs into a remote workload cluster. The Deploy dialog never offers the management cluster.
- Once per cluster: every object has a fixed name, so two instances would overwrite each other's `Secret`, environment and route, and deleting either would remove the other's objects. A second instance for the same kubeconfig Secret of a tenant is rejected (`409`), and you edit the existing instance instead. Keel cannot tell whether two tenants' kubeconfig Secrets point at the same cluster, so do not install it from two tenants onto one cluster.
- It spans three namespaces, declared in `spec.placement` (`namespace: keel-system`, `additionalNamespaces: [keel-previews, flux-system]`). Keel creates all three first (restricted PSS, `ssa: IfNotPresent`, never pruned) and sets no Flux `targetNamespace`, so the per-namespace `Role`s stay where they belong. See [Placement](overview.md#placement-specplacementscope).
- Any other `targetNamespace` is rejected (`400`).

## Prerequisites

Set these up before you install the app:

1. **A workload cluster registered for the tenant**, running Kubernetes 1.30 or later (for `admissionregistration.k8s.io/v1` `ValidatingAdmissionPolicy`), with a kubeconfig Secret in `tenant-<id>` (the Deploy dialog lists it). The identity that kubeconfig impersonates (`tenant-<id>-sa` on the workload cluster) must be allowed to create `ClusterRole`s and `ClusterRoleBinding`s with the permissions they grant (or hold `escalate` / `bind`), `ValidatingAdmissionPolicy`/`ValidatingAdmissionPolicyBinding` objects, and the three namespaces.
2. **Not a Keel-bootstrapped cluster.** On a cluster set up by `keel bootstrap init`, Keel's `protect-control-plane` Kyverno policy blocks tenant ServiceAccounts from writing to `keel-system` and `flux-system`, and that cluster already runs its own keel-server.
3. **Flux and PKO on the workload cluster:** install the catalog apps `flux2` (keep its multi-tenancy lockdown on) and `pulumi-kubernetes-operator` there first, optionally `kyverno` for the team's own policies. The team's Keel commits PRs to its repository, and that cluster's Flux reconciles them.
4. **A Keel GitOps repository for the team**, reachable over HTTPS. Prepare it **before** you install the app:
   - `.keel/config.yaml` on `main`, naming its forge so the server can open pull requests:

     ```yaml
     apiVersion: keel.io/v1alpha1
     kind: KeelConfig
     clusterName: team-a
     bootstrapTarget: kubeconfig
     forge:
       provider: forgejo # forgejo | gitea | github | gitlab
       repoUrl: https://git.example.com/team-a/infra-gitops.git
       apiUrl: https://git.example.com/api/v1
     ```

     The `oidc` block is optional: the app's OIDC fields override it.

     Do not rely on an empty remote. If `main` does not exist, the server seeds it on first start with the management defaults: `forge.provider: local` (a bare repository that does not exist in the pod, so every pull request fails) and `clusters/management/management-stack.yaml` pointing at the management plane's in-cluster SeaweedFS. The server reads `.keel/config.yaml` only at startup, and it cannot open a PR to fix its own forge. If that already happened, push the corrected `.keel/config.yaml` (and state backend, below) straight to `main`, then restart the server on the workload cluster: `kubectl -n keel-system rollout restart deploy/keel-server`.
   - **The state backend** for the team's Pulumi `Stack`s and previews, as `spec.backend` of `clusters/management/management-stack.yaml` (for example `s3://team-a-pulumi-state?endpoint=s3.example.com&region=us-east-1`). The server reads the backend URL from that key at startup. Without it, it assumes `s3://pulumi-state`. Every tenant needs a usable `keel-pulumi-passphrase` Secret (key `passphrase`) in `tenant-<id>` on the workload cluster before its first Pulumi proposal. If you also create `keel-system/keel-s3-credentials` (state backend keys), each tenant additionally needs its own scoped `keel-tenant-s3-credentials` Secret (`accessKey`, `secretKey`, `stateBackendUrl`) in `tenant-<id>`. The server only provisions scoped identities itself for the management plane's in-cluster SeaweedFS, and otherwise rejects Pulumi proposals with `409` until they exist.
   - **A Flux sync on the workload cluster**, polling the repository (`GitRepository` with `interval: 1m`, for example via `flux create source git`), plus a `Kustomization` for the team's own paths. Tenants need a confined reconciler, as on the management plane: apply `deploy/seed/flux-system/tenant-reconciler-rbac.yaml` and a `keel-tenants`-style `Kustomization` modelled on `deploy/seed/flux-system/gotk-sync.yaml`:

     ```yaml
     apiVersion: kustomize.toolkit.fluxcd.io/v1
     kind: Kustomization
     metadata:
       name: keel-tenants
       namespace: flux-system
     spec:
       interval: 1m
       path: ./tenants
       prune: true
       serviceAccountName: keel-tenant-reconciler
       sourceRef:
         kind: GitRepository
         name: flux-system
     ```

     The server itself cannot create `tenant-<id>` namespaces, so tenant onboarding goes through this `Kustomization`. Without `serviceAccountName`, tenant objects are applied with `kustomize-controller`'s cluster-admin rights, which breaks namespace-per-tenant isolation. With the `flux2` app's lockdown on (`defaultServiceAccount: default`), it is forbidden instead. The reconciler's RBAC cannot create `Role`s or `ClusterRole`s, but it can bind `ClusterRole/admin` in any namespace. On the management plane, Keel's Kyverno `restrict-tenant-reconciler` policy confines it to `tenant-*` namespaces. On the workload cluster, install the `kyverno` app and apply `deploy/seed/kyverno-policies/restrict-tenant-reconciler.yaml`, or review every change under `tenants/` carefully.
5. **A Gateway API Gateway** on the workload cluster (for example the `envoy-gateway` catalog app plus a `Gateway`). Its listeners must terminate TLS for the hostname and admit `HTTPRoute`s from `keel-system` (`allowedRoutes.namespaces.from: All` or a selector), and a DNS record must point the hostname at it.
6. **An OIDC client** in the team's identity provider: a confidential client using the authorization code flow with PKCE, redirect URI `https://<hostname><port suffix>/api/v1/auth/callback`, and a `groups` claim. Members of the admin group (default `keel-admins`) administer this Keel. Tenant access follows the `tenant-<id>` / `tenant-<id>-admin` groups, as on the management plane. If the IdP's certificate comes from a private CA, the server only trusts it through a Secret `keel-system/keel-wildcard-tls` on the workload cluster whose `ca.crt` key holds that CA (PEM), read at startup.
7. **A Git token** with read/write access to the repository and permission to open pull requests.

## Form Fields

| Field | Required | Notes |
| :--- | :--- | :--- |
| Public Hostname | yes | Must be unique. Platform hosts of the management domain (`idp.`, `keel.`, `forgejo.`) and hostnames another app already routes are rejected (`400` / `409`). |
| Public HTTPS Port Suffix | no | Empty (port 443) by default, `:8888` for a local kind Gateway. Used in the OIDC redirect URI. |
| OIDC Issuer URL | yes | `https://` only. Discovery at `<issuer>/.well-known/openid-configuration`. Sets `KEEL_OIDC_ISSUER_URL`. |
| OIDC Client ID | yes | Sets `KEEL_OIDC_CLIENT_ID`. The redirect URI (`KEEL_OIDC_REDIRECT_URL`) is derived from the hostname. |
| OIDC Client Secret | yes | Secret field. Sent on the token exchange (`client_secret_post`). Sets `KEEL_OIDC_CLIENT_SECRET`. |
| Admin Group | no | Default `keel-admins`. Sets `KEEL_ADMIN_GROUPS`, which adds to the built-in `keel-admins` / `admin` / `admins` groups. |
| GitOps Repository URL | yes | HTTPS clone URL without credentials (`http://` and URLs with `user@` are rejected, because the token is sent with every clone, fetch and push). Sets `KEEL_GIT_URL`. |
| Git HTTPS Username | no | Default `keel` (Forgejo/Gitea). Use `x-access-token` for GitHub or `oauth2` for GitLab. |
| Git / Forge API Token | yes | Secret field. Sets `KEEL_GIT_TOKEN`. |
| Gateway Name / Namespace | no | Default `keel-ingress` in `keel-system`. |

The two secret fields follow the usual manifest-app secret path: Keel SOPS-encrypts them with the tenant `age` key into `config/secrets.enc.yaml` (`<instance>-secrets` in `tenant-<id>` on the management cluster). The app's Flux `Kustomization` substitutes them into `keel-system/keel-server-credentials` on the workload cluster. Git only holds `${...}` placeholders and SOPS ciphertext, and the values are never echoed in API errors. On **Edit**, a blank secret field keeps its stored value. Secrets and env are read when the pod starts, so restart the Deployment after rotating one. Values must not contain whitespace, `"`, `\` or `$` (Flux substitution).

The app only supports the Flux engine (`actionEngine: pulumi` is rejected with `400`).

## Install

From **Catalog & Form Wizard**, pick **Keel Control Plane (Workload Cluster)**, choose the tenant's workload cluster, fill in the form and open the PR. `keel app install` has no target-cluster option, so for automation call the API directly: `POST /api/v1/proposals` with `"catalogId": "keel-server"`, `"targetCluster": "<kubeconfig Secret>"` and the form values, for example:

```json
{
  "tenant": "team-a",
  "catalogId": "keel-server",
  "targetCluster": "team-a-cluster-kubeconfig",
  "values": {
    "hostname": "keel.team-a.example.com",
    "oidcIssuerUrl": "https://auth.team-a.example.com/auth/v1",
    "oidcClientId": "team-a-keel",
    "oidcClientSecret": "<client secret>",
    "adminGroup": "team-a-platform",
    "gitUrl": "https://git.example.com/team-a/infra-gitops.git",
    "gitToken": "<token>"
  }
}
```

After the PR merges, open `https://<hostname><port suffix>` and sign in through the team's IdP.

## Upgrade

Installed instances run the image tag of the `manifests.yaml` in your GitOps repository's `catalog/apps/keel-server/`, rendered into `tenants/<id>/apps/<name>/manifests.yaml` when the instance was proposed. A new Keel release does not change either file. To upgrade:

1. Open a PR in the GitOps repository that sets `catalog/apps/keel-server/catalog.yaml` `spec.upstream.version` and the `ghcr.io/rdylanwalker/keel:<tag>` image in `catalog/apps/keel-server/manifests.yaml` to the new release (copy both files from that release's `deploy/catalog/apps/keel-server/`), and merge it.
2. **Edit** each installed instance and submit it unchanged (leave the secret fields blank to keep them). Keel re-renders the instance's `manifests.yaml` with the new image in a new PR. Merging it rolls out the new server.

## Server Environment Overrides

The app configures `keel serve` through environment variables, which any in-cluster deployment can also use:

| Variable | Effect |
| :--- | :--- |
| `KEEL_OIDC_ISSUER_URL`, `KEEL_OIDC_CLIENT_ID`, `KEEL_OIDC_REDIRECT_URL` | Override the `oidc` block of the served repository's `.keel/config.yaml` field by field and enable OIDC. Missing fields fall back to that block. If the result is incomplete or a URL is not an absolute `http(s)` URL, the server refuses to start. |
| `KEEL_OIDC_CLIENT_SECRET` | Makes the server a confidential OIDC client: the token exchange also sends `client_secret`. Never written to Git, logged or returned. Blank means a public PKCE client. |
| `KEEL_ADMIN_GROUPS` | Comma-separated IdP groups that grant cluster-administrator rights, in addition to `keel-admins`, `admin` and `admins`. |
