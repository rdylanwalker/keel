# Running keel-server In-Cluster

`keel serve` is stateless, so it can run on a workstation (the default quick-start path) or inside the management cluster. For the in-cluster case there are two options:

- **Flux-managed add-on (recommended):** `keel bootstrap init --with-keel-server`. See [Flux-Managed Add-on](#flux-managed-add-on).
- **Manual:** apply the least-privilege Kustomize bundle in `deploy/bootstrap/keel-server/` yourself. See [Manual Deploy](#manual-deploy).

> **Status: experimental.** The bundle is not yet verified end to end on a live cluster.

## Repository Bootstrap

The pod's `/var/lib/keel` is an `emptyDir`, so the working tree is rebuilt from the GitOps remote on every start. `keel serve --git-url <url>` (or `KEEL_GIT_URL`) does this:

- If `--repo-dir` is missing or empty, the server clones `<url>` into it. If the remote is empty, it seeds `main` and pushes it.
- If `--repo-dir` is already a clone (a container restart with the same volume), the server re-points `origin` at `<url>` if needed, fetches it, and hard-syncs `main` to `origin/main`.
- If `--repo-dir` is a non-empty directory that isn't a clone, the server refuses to start rather than overwrite it.
- With `--git-url` set, no sibling bare repository is created. The remote URL is also the source that preview `GitRepository` objects pull from.

HTTP(S) credentials come only from `KEEL_GIT_TOKEN` (paired with `KEEL_GIT_USERNAME`, default `keel`). Keel passes them to `git` through ephemeral config environment variables, never argv, `.git/config`, or logs. URLs with embedded `user:token@` credentials are rejected.

The bundle sets these values through the `keel-server-config` `configMapGenerator` in `kustomization.yaml`. By default it points at the Stage 0 in-cluster Forgejo repository, `http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git`. It reads `KEEL_GIT_TOKEN` from the `keel-system/forge-token` Secret (key `token`) that `keel bootstrap init` creates. To use an external forge, override the ConfigMap in an overlay:

```yaml
configMapGenerator:
  - name: keel-server-config
    namespace: keel-system
    behavior: merge
    literals:
      - KEEL_GIT_URL=https://github.com/acme/infra-gitops.git
      - KEEL_GIT_USERNAME=x-access-token
```

Inside a pod (`KUBERNETES_SERVICE_HOST` set), the laptop-only automatic port-forwards to Forgejo and Envoy Gateway are skipped, because the in-cluster Service DNS resolves directly.

### OIDC and Admin Group Overrides

By default the server reads its OIDC client from the `oidc` block of the served repository's `.keel/config.yaml`. The environment variables `KEEL_OIDC_ISSUER_URL`, `KEEL_OIDC_CLIENT_ID` and `KEEL_OIDC_REDIRECT_URL` override that block field by field, `KEEL_OIDC_CLIENT_SECRET` makes the server a confidential client, and `KEEL_ADMIN_GROUPS` adds comma-separated IdP admin groups to `keel-admins` / `admin` / `admins`. The [`keel-server` catalog app](../catalog/keel-server.md) uses them to run a separate Keel for a team on a workload cluster.

## Flux-Managed Add-on

```bash
keel bootstrap init --target kind --with-forge --with-keel-server --repo-dir ./keel-gitops
# or, with an external forge:
keel bootstrap init --target kind --with-keel-server \
  --git-url https://github.com/acme/infra-gitops.git --git-token "$KEEL_GIT_TOKEN" --repo-dir ./keel-gitops
```

`--with-keel-server` is off by default. With it, `keel bootstrap init` writes:

- `clusters/management/core-addons/keel-server.yaml`: the protected Flux `Kustomization` `flux-system/keel-server`. Its name matches the catalog id. It pins `ghcr.io/rdylanwalker/keel` through `spec.images[].newTag` to the release tag of the `keel` CLI you bootstrapped with, never `latest`.
- `clusters/management/addons/keel-server/`: `rbac.yaml` and `deployment.yaml` from this bundle, plus a `keel-server-ingress` HTTPRoute for `https://keel.<domain>[:port]` (from `--oidc-public-url`, default `https://keel.keel.local:8888`). Its `kustomization.yaml` generates `keel-server-config` with the GitOps remote and HTTPS username.
- `catalog/core/keel-server/catalog.yaml`: the management add-on entry. Upgrades through the management API change only the image tag.

There is no Stage 0 install, so Flux is the only owner. The flag needs `--repo-dir` (the add-on reaches Git through the seed push) and a Git remote the pod can clone, so pass `--with-forge` or an HTTP(S) `--git-url` without credentials in the URL. A local `--repo-dir` without a remote is rejected, and so is `--with-forge` together with `--git-url`: the server would get Forgejo credentials for an external remote.

Secrets are never written to Git:

- `keel-system/forge-token` (key `token`) supplies `KEEL_GIT_TOKEN`. Stage 0 creates it with `--with-forge`. With `--git-url`, it is created from `--git-token`. If neither applies (for example with `--skip-core-install`), bootstrap prints a note and you create it yourself:

  ```bash
  kubectl -n keel-system create secret generic forge-token --from-file=token=<path-to-token-file>
  ```

- The session key is generated by the server into a Kubernetes Secret on first use.
- With an external `--oidc-issuer`, bootstrap prints the redirect URIs to register on the OIDC client: `https://keel.<domain>[:port]/api/v1/auth/callback`, plus `http://127.0.0.1:8080/api/v1/auth/callback` for port-forward access. The Stage 0 Rauthy client already allows both.

Open the server at `https://keel.<domain>[:port]` (add the host to `/etc/hosts` for the local ingress), or run `kubectl -n keel-system port-forward svc/keel-server 8080:8080`.

The add-on omits `tenant-default-rolebinding.yaml`, because `tenants/default/rbac.yaml` (owned by the `keel-tenants` Flux Kustomization) already carries that binding. The Flux Kustomization has no `targetNamespace`, because the bundle spans `keel-system`, `keel-previews`, `flux-system` and cluster-scoped RBAC.

## Manual Deploy

The bundle uses the released image `ghcr.io/rdylanwalker/keel`, pinned to the release that matches the checkout (semantic-release bumps the tag together with the crate version). Each release publishes the image with `<version>` and `latest` tags, but the bundle never uses `latest`. The image runs as non-root UID `65532`, with `ENTRYPOINT keel`, and ships `git`, `openssh`, `sops`, and `age`.

```bash
cd deploy/bootstrap/keel-server
# Optional: pin another release or point at a mirror.
kustomize edit set image ghcr.io/rdylanwalker/keel=ghcr.io/rdylanwalker/keel:<version>
kubectl apply -k .
```

The bundle contains:

- `ServiceAccount` `keel-system/keel-server`.
- `Deployment` `keel-system/keel-server`: one replica, `restricted` Pod Security (non-root UID `65532`, read-only root filesystem, every capability dropped, `RuntimeDefault` seccomp). It runs `keel serve --bind 0.0.0.0:8080 --repo-dir /var/lib/keel/work` with `KEEL_GIT_URL`/`KEEL_GIT_USERNAME` from `ConfigMap/keel-server-config` and `KEEL_GIT_TOKEN` from `Secret/forge-token`, plus `/healthz` readiness and liveness probes.
- `Service` `keel-system/keel-server` on port `8080`.
- `RoleBinding` `tenant-default/keel-server` (`tenant-default-rolebinding.yaml`, manual install only).

Expose the Service through your Envoy Gateway `HTTPRoute` or ingress of choice. The OIDC redirect URI is derived from the request `Host`, so register that external origin as a redirect URI on the IdP client.

## RBAC Summary

| Scope | Grants |
| :--- | :--- |
| Cluster (`ClusterRole/keel-server-read`) | **Read-only** `get/list/watch` on PKO `stacks`, `programs`, `workspaces`, `updates`; Flux `helmreleases`, `kustomizations`, `gitrepositories`, `helmrepositories`, `ocirepositories`; Flagger `canaries`; `namespaces`, `events`; Kyverno `policyreports`. Also write access to Kyverno `validatingpolicies`, `generatingpolicies`, and `policyexceptions`, so the server can re-apply `restrict-pulumi-providers`. **No cluster-wide Secret access.** |
| `keel-system` (`Role/keel-server`) | `secrets` and `configmaps` (session key, encrypted user credentials, forge token), `pods`, and `pods/exec` limited to the SeaweedFS filer pod (`s3-state-backend-seaweedfs-filer-0`). Exec is needed to provision per-tenant SeaweedFS identities through `weed shell`, with the script sent on stdin. Also `statefulsets` (read). |
| `keel-previews` (`Role`) | Create and delete preview `stacks`, `gitrepositories`, report `configmaps`, mirrored tenant-scoped `secrets`, and per-preview provider-credential `secrets` copies. Read `workspaces`, `updates`, and workspace `pods`/`pods/log`. |
| `flux-system` (`Role`) | Reconcile annotations on `kustomizations`, `helmreleases`, `gitrepositories`, and `stacks`. The Kyverno `protect-control-plane` policy rejects any other change by `keel-system/keel-server` to Flux objects and Stacks in `flux-system` and `keel-system` (spec, labels, finalizers, or annotations other than `reconcile.fluxcd.io/requestedAt` / `pulumi.com/reconcile-request`). Those objects are reconciled by the cluster-admin `kustomize-controller`, so a spec change would be privilege escalation. Management add-on upgrades and rollbacks go through Git PRs. |
| Each `tenant-<id>` (`RoleBinding` to `ClusterRole/keel-server-tenant`) | Tenant `secrets` (scoped S3 and Git credentials, workload kubeconfigs), `stacks` (patch/delete for unlock, force-delete, and restart), `helmreleases` and `kustomizations` (reconcile), `pods`, `pods/log`, `configmaps`, and `deployments`. |

Tenant namespaces are created dynamically. The tenant RBAC bundle that Keel generates for each new tenant (`tenants/<id>/rbac.yaml`) includes the `keel-server` `RoleBinding`. The Kyverno `restrict-tenant-reconciler` policy allows exactly this binding (`ClusterRole/keel-server-tenant` bound to `keel-system/keel-server`) and rejects every other cross-namespace ServiceAccount binding.

## Configuration

| Setting | How |
| :--- | :--- |
| GitOps remote | `--git-url` / `KEEL_GIT_URL` (+ `--git-username` / `KEEL_GIT_USERNAME`). See [Repository Bootstrap](#repository-bootstrap). |
| Forge token | `--git-token`, `KEEL_GIT_TOKEN`, or the `keel-system/forge-token` Secret. If none is set and `.keel/config.yaml` names a remote forge, the server falls back to `LocalGitProvider` with a warning. |
| Break-glass admins | `KEEL_BREAK_GLASS_ADMINS` (comma-separated OIDC `sub`/`email`). Default `admin@keel.local`. |
| OIDC CA | For the Stage 0 self-signed ingress, the server trusts `ca.crt` from `keel-system/keel-wildcard-tls`. |
| Bind address | The default `--bind` is `127.0.0.1:8080`. In a pod, pass `0.0.0.0:8080`, as the shipped `Deployment` does. |

## Security Notes

- Workload-cluster kubeconfigs stored in tenant `*-kubeconfig` Secrets are validated before use. `exec` credential plugins, `auth-provider`, and file-path credentials (`tokenFile`, `client-certificate`, `client-key`, `certificate-authority`) are rejected, so a tenant can't make `keel-server` run binaries or read local files such as its own ServiceAccount token. Managed-cloud kubeconfigs that rely on `exec` plugins, such as EKS `aws eks get-token` or GKE `gke-gcloud-auth-plugin`, are therefore skipped for live status. Store a static token instead.
- 5xx responses return a generic message plus a `correlationId`. The full error is logged server-side under the same id.
