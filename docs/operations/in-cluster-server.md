# Running keel-server In-Cluster

`keel serve` is stateless, so it can run on a workstation (the default quick-start path) or inside the management cluster. `deploy/bootstrap/keel-server/` ships a least-privilege Kustomize bundle for the in-cluster case.

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

## Deploy

The bundle uses the released image `ghcr.io/rdylanwalker/keel:latest`. Each release publishes it with `<version>` and `latest` tags. It runs as non-root UID `65532`, with `ENTRYPOINT keel`, and ships `git`, `openssh`, `sops`, and `age`.

```bash
cd deploy/bootstrap/keel-server
# Optional: pin a release or point at a mirror.
kustomize edit set image ghcr.io/rdylanwalker/keel=ghcr.io/rdylanwalker/keel:<version>
kubectl apply -k .
```

The bundle contains:

- `ServiceAccount` `keel-system/keel-server`.
- `Deployment` `keel-system/keel-server`: one replica, `restricted` Pod Security (non-root UID `65532`, read-only root filesystem, every capability dropped, `RuntimeDefault` seccomp). It runs `keel serve --bind 0.0.0.0:8080 --repo-dir /var/lib/keel/work` with `KEEL_GIT_URL`/`KEEL_GIT_USERNAME` from `ConfigMap/keel-server-config` and `KEEL_GIT_TOKEN` from `Secret/forge-token`, plus `/healthz` readiness and liveness probes.
- `Service` `keel-system/keel-server` on port `8080`.

Expose the Service through your Envoy Gateway `HTTPRoute` or ingress of choice. The OIDC redirect URI is derived from the request `Host`, so register that external origin as a redirect URI on the IdP client.

## RBAC Summary

| Scope | Grants |
| :--- | :--- |
| Cluster (`ClusterRole/keel-server-read`) | **Read-only** `get/list/watch` on PKO `stacks`, `programs`, `workspaces`, `updates`; Flux `helmreleases`, `kustomizations`, `gitrepositories`, `helmrepositories`, `ocirepositories`; Flagger `canaries`; `namespaces`, `events`; Kyverno `policyreports`. Also write access to Kyverno `validatingpolicies`, `generatingpolicies`, and `policyexceptions`, so the server can re-apply `restrict-pulumi-providers`. **No cluster-wide Secret access.** |
| `keel-system` (`Role/keel-server`) | `secrets` and `configmaps` (session key, encrypted user credentials, forge token), `pods`, and `pods/exec`. Exec is needed to provision per-tenant SeaweedFS identities through `weed shell`, with the script sent on stdin. Also `statefulsets` (read) and `helmreleases` (patch, for add-on upgrade and rollback). |
| `keel-previews` (`Role`) | Create and delete preview `stacks`, `gitrepositories`, report `configmaps`, mirrored tenant-scoped `secrets`, and per-preview provider-credential `secrets` copies. Read `workspaces`, `updates`, and workspace `pods`/`pods/log`. |
| `flux-system` (`Role`) | Reconcile annotations on `kustomizations`, `gitrepositories`, and `stacks`. |
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
