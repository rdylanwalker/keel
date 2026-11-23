# Known Limitations

Keel is alpha software. This page collects the limitations and open bugs a user is likely to hit, with workarounds where one exists. It describes v0.12.0.

## Releases and Scope

- **No upgrade path.** A release marked breaking in the changelog (a `!` or `BREAKING CHANGE` entry) does not migrate clusters, GitOps repositories or tenant repositories. Start over with a fresh `keel bootstrap init` on a new cluster and a new repository.
- **Local Docker socket flag is unsafe for tenants.** `--local-dev-docker-socket-exception`, used in the Quick Start for nested `kind` clusters, exposes the host Docker socket without authentication to PKO workspace pods and to any tenant pod that sets the workspace label. That is root on the host, so tenant isolation does not hold with this flag. Use it on throwaway machines only.
- **Single-node management cluster.** The management cluster is a single node with no high availability.
- **Tested path.** The live E2E suite covers only local `kind` with the in-cluster Forgejo and Rauthy. Everything else is experimental; see the maturity table in the [README](https://github.com/rdylanwalker/keel#maturity-what-is-tested).
- **Platforms.** Only Linux x86_64 (including WSL2) is tested. Release binaries for Linux arm64, macOS arm64 and Windows x86_64 are built but not tested. There is no macOS Intel build.

## Bootstrap Targets

- **Proxmox VE and Hetzner create a VM only.** `--target proxmox` and `--target hetzner` provision the protected VM but install no Kubernetes and export no kubeconfig, so `keel bootstrap init` stops with an error after creating the VM. Install k3s on the VM yourself and attach it with `--target kubeconfig`. The seeded `proxmox-k3s-cluster` blueprint also creates a VM only.
- **Cloud bootstrap is untested.** `--target aws|gcp|azure` installs single-node k3s on a 4 GB VM (`t3.medium`, `e2-medium`, `Standard_B2s`). These are not free-tier sizes. No automated test runs them, and the local management node alone peaks at about 7 GiB of memory, so 4 GB is probably too small.
- **Local Git only.** With `--repo-dir` and no `--with-forge` or network `--git-url`, Flux cannot read the repository: merged changes are never deployed, `keel infra provision` fails with `409 ... is not a network Git URL`, and there is no tenant isolation. The bootstrap summary still prints `Flux Polling: ... (outbound pull-polling active)`, and a later note says that Flux cannot reach the repository. The note is correct. Use `--with-forge` or `--git-url`.

## Storage and Recovery

- **The in-cluster Forgejo is ephemeral.** `--with-forge` installs Forgejo with persistence disabled. Replacing the Forgejo pod deletes every repository, tenant repositories included. Mirror anything you want to keep.
- **The state escrow lives in the same cluster.** Stage 0 copies its Pulumi state to the in-cluster SeaweedFS. `keel bootstrap recover` repairs a damaged control plane while the cluster is reachable; it is not a backup against losing the cluster. Copy `~/.config/keel/stage0/<cluster>/` and the passphrase somewhere else.
- **`recover --dry-run` lists fewer stacks than a real run.** Without `--stack`, the dry run shows only `stage0-control-plane-<cluster>`, but the real run recovers every `control-plane*` stack in the escrow (for example also `-forgejo` and `-rauthy`).
- **Teardown leftovers.** `kind delete cluster` leaves `./keel-gitops`, `./keel-gitops.git`, `./keel-gitops-tenant-repos/` and `~/.config/keel/stage0/<cluster>/`. A later bootstrap with the same names silently reuses the old bare repository and its history. Delete them when you delete the cluster.

## Git and Forges

- **Gitea cannot host tenant repositories.** Gitea has no repository-scoped tokens. Use Forgejo 15 or later, GitHub or GitLab.
- **External forges are untested against real services.** GitHub, GitLab and external Forgejo are only tested against a mock forge.
- **The local clone cannot pull.** With `--with-forge`, the remote of `./keel-gitops` is the Forgejo port-forward (`http://127.0.0.1:3000/...`) without a credential, so `git pull` asks for a username. Use the web UI or the Forgejo UI to read the repository.
- **Two proposals that change the same app conflict.** Keel rebuilds a proposal that fell behind `main` when it only conflicts in the generated `kustomization.yaml` index (see [Merging a proposal that is behind `main`](cli.md#merging-a-proposal-that-is-behind-main)). If two open proposals change the same app or Stack, the second one's merge is refused with the conflicting files. Close it and propose the change again.
- **Update PRs show a misleading preview.** Editing an existing Helm app shows `+ Create HelmRelease` in the synthesized preview, not an update. The Git diff is correct.
- **GitHub tenant repositories pin GitHub's SSH host keys.** Keel writes `known_hosts` from GitHub's published host keys when it issues a tenant's deploy key. If GitHub rotates its host keys, tenant clones fail until you run `keel tenant repair-credentials <id>`, which issues a new key with fresh host keys. Tenant repositories on GitHub are always cloned over `ssh://git@github.com:22`, so the cluster needs port 22 egress.
- **GitHub Free cannot protect private tenant repositories.** Branch protection on private repositories needs a paid GitHub plan, so the forge does not block direct pushes there.
- **Forgejo repository names are visible to other tenants.** A tenant's repository-scoped Forgejo token can read the name and description of other tenant repositories, but not their content.
- **Misleading forge error text.** Some token and repository errors are reported as `Invalid pull request input`.
- **Tenants created through the API on the Local Git provider.** They have no tenant Git source: Pulumi Stacks and previews return `409`, and Flux manifest apps never reconcile. Helm apps work.
- More tenancy gaps, such as the operator token on external forges and the platform `age` key, are listed in [Security & Tenancy: Residual exposure](../operations/security-and-tenancy.md#residual-exposure).

## Apps and Canaries

### Canary apps on the management cluster

**v0.12.0 only; fixed in the next release.** In v0.12.0, the tenant ServiceAccount has no permission to manage Flagger `Canary` objects. A canary app (`--canary`, or **Enable Flagger Canary**) installed into a tenant namespace on the management cluster therefore fails with `canaries.flagger.app "<app>" is forbidden`, and because the tenant's root Kustomization fails as a whole, the tenant's other apps stop updating too. Canaries on a remote workload cluster are not affected. On v0.12.0, grant the permission through an aggregated role:

```bash
kubectl --context kind-keel-mgmt apply -f - <<'EOF'
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: keel-tenant-flagger-workaround
  labels:
    rbac.authorization.k8s.io/aggregate-to-admin: "true"
rules:
  - apiGroups: ["flagger.app"]
    resources: ["canaries"]
    verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
EOF
```

v0.12.0 has no admission policy for Canaries, so this role lets every tenant admin point Flagger's webhooks and metric queries anywhere. Use it only on a single-user test cluster. The next release ships its own role (`keel-tenant-flagger-canaries`) together with the `restrict-tenant-flagger-canaries` Kyverno policy; after upgrading, delete the workaround with `kubectl delete clusterrole keel-tenant-flagger-workaround`.

### Other app limitations

- **v0.12.0: the CLI does not apply schema defaults.** In v0.12.0, `keel app install` sends only the values in your file, while the web UI fills in the catalog's defaults. For `podinfo`, the default `securityContext` is then missing and Kyverno rejects the Deployment (`disallow-privileged-containers`). Copy the defaults you need into the values file; [Things to Try](../getting-started/things-to-try.md#2-deploy-an-application-with-canary-rollouts-podinfo) shows the `podinfo` file. The next release fills in [schema defaults](../catalog/overview.md#schema-defaults) on the server for every first install.
- **v0.12.0: `keel app list` hides failures.** In v0.12.0, a release that fails to reconcile stays `Pending` without a reason. Check Flux directly:

  ```bash
  kubectl --context kind-keel-mgmt -n tenant-default get kustomizations,helmreleases
  kubectl --context kind-keel-mgmt -n tenant-default describe kustomization keel-tenant-root
  ```

  The next release shows the reason in a `MESSAGE` column (`statusMessage` in `GET /api/v1/releases`, the status tooltip in the web UI). This includes a failing tenant root Kustomization that blocks the release from being applied.

- **YAML 1.1 booleans.** Keel writes YAML with a YAML 1.2 serializer, which leaves string values such as `yes`, `no`, `on`, `off`, `y` and `n` unquoted. Helm and Kustomize read YAML 1.1 and turn them into booleans. Avoid these as string values in forms, or quote them in the PR.
- **Editing a Helm app replaces all its secrets.** When you edit a Helm app, re-enter every secret field: secrets you leave blank are dropped.
- **Set a host for every canary.** A canary without `ingressHost` creates an `HTTPRoute` without a host name, which matches every host on the shared `keel-ingress` Gateway.
- **Canary analysis is not tested.** E2E checks that a canary deploys and initializes, but not traffic shifting, promotion or rollback.
- **Apps are served over HTTPS only.** Canary apps are reachable through `keel-ingress` at `https://<host>:8888` on local `kind`. Plain HTTP on port 8888 does not respond.

## Operations

- **Slow, silent first preview.** The first PKO preview pulls the 3.5 GB `pulumi/pulumi:latest-nonroot` image, and `kind` nodes pull images one at a time. `keel preview status --watch` prints nothing for a few minutes while this happens.
- **A Stack stalls if its Git credential Secret is deleted.** If `tenant-<id>/keel-gitops-credentials` is deleted, the tenant's Stacks fail with `SpecInvalid` and do not recover when the Secret is restored, only when the Stack's spec next changes.
- **`keel serve` needs the hosts entries too.** If `idp.keel.local` does not resolve on the machine running `keel serve`, OIDC discovery fails silently and the login page is blank (Rauthy returns 404). Add the `/etc/hosts` entries before starting the server.
- **Stop `keel serve` with Ctrl+C.** Other signals (for example `kill`) can leave its `kubectl port-forward` to port 8888 running. The next `keel serve` then picks a random port while the IdP URL still says `:8888`, and login only works while the orphaned process lives. Kill leftover `kubectl port-forward` processes before restarting.
- **Credentials in the bootstrap output.** The bootstrap summary prints the generated admin password in plain text. Keep that terminal output private, or pass `--admin-password` / `KEEL_ADMIN_PASSWORD`.
