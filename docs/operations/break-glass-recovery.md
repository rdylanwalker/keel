# Break-Glass Recovery

Keel is built so that you cannot lock yourself out of the control plane (`NFR-REL-01`). If the management cluster's add-ons are damaged, or the Git forge or `keel-server` is unreachable, you can re-apply the Stage 0 control plane straight from escrowed Pulumi state. Git, PRs, and the API are not involved.

> **This is not a backup.** The default escrow is the SeaweedFS S3 service **inside the management cluster**, so recovery needs a reachable cluster. If the cluster is lost, the in-cluster escrow is lost with it, and only your local `~/.config/keel/stage0/<cluster>/` copy remains. Copy that directory and the passphrase off the workstation if you need to survive losing both. Live E2E covers only `recover --dry-run`; a real run was tested manually on v0.12.0.

## What Stage 0 Escrows

`keel bootstrap init` keeps durable Stage 0 state for each cluster:

| Location | Contents |
| :--- | :--- |
| `~/.config/keel/stage0/<cluster>/state/` | Local Pulumi backend (`file://`) for every Stage 0 stack. |
| `~/.config/keel/stage0/<cluster>/programs/<program>/` | The materialized `Pulumi.yaml` and encrypted stack config (`Pulumi.stage0-<program>-<cluster>.yaml`). |
| `~/.config/keel/stage0/<cluster>/pulumi-passphrase` | Generated Pulumi secrets passphrase (mode `0600`; directories are `0700`). |
| `keel-system/keel-stage0-pulumi-passphrase` (key `passphrase`) | In-cluster copy of the passphrase. |
| `s3://pulumi-state/stage0/<cluster>/` | S3 escrow: Pulumi checkpoints (`.pulumi/stacks/...`), programs and encrypted config (`config/<program>/`), and `manifest.json`. |

The config root is `$KEEL_CONFIG_DIR`, or `$XDG_CONFIG_HOME/keel`, or `~/.config/keel`.

Stage 0 stacks are named `stage0-<program>-<cluster>`. Examples are `stage0-control-plane-keel-mgmt`, `stage0-control-plane-forgejo-keel-mgmt`, `stage0-control-plane-rauthy-keel-mgmt`, and the provisioning stack for the target, such as `stage0-kind-keel-mgmt`.

During escrow, Keel turns on **bucket versioning** for `pulumi-state`, so every `pulumi up` keeps an older object version you can return to. If the S3 backend rejects versioning, Keel instead writes timestamped snapshots to `stage0/<cluster>/snapshots/<stack>/<timestamp>.json`. The bootstrap summary says which mode is active.

> **Escrow the passphrase.** When it generates the passphrase, `keel bootstrap init` prints a notice with the passphrase location, and repeats it on later runs until one completes (a failed run prints it to stderr). Copy the passphrase into your password manager or vault. Without it, no copy of the state can be decrypted once the workstation and the in-cluster Secret are gone.

## Runbook

Prerequisites: `pulumi` and `kubectl` on `PATH`, and a kubeconfig for the target cluster.

### 1. Reach the S3 escrow

For the in-cluster SeaweedFS backend:

```bash
kubectl -n keel-system port-forward svc/s3-state-backend-seaweedfs-s3 8333:8333
```

Leave this running in its own terminal.

### 2. Export S3 credentials and the passphrase

```bash
export AWS_ACCESS_KEY_ID="$(kubectl -n keel-system get secret s3-state-backend-seaweedfs-s3-secret \
  -o jsonpath='{.data.admin_access_key_id}' | base64 -d)"
export AWS_SECRET_ACCESS_KEY="$(kubectl -n keel-system get secret s3-state-backend-seaweedfs-s3-secret \
  -o jsonpath='{.data.admin_secret_access_key}' | base64 -d)"

# Passphrase: from your vault, from the local file, or from the in-cluster Secret.
kubectl -n keel-system get secret keel-stage0-pulumi-passphrase \
  -o jsonpath='{.data.passphrase}' | base64 -d > /tmp/keel-passphrase && chmod 600 /tmp/keel-passphrase
```

Without `--stack`, recovery covers the `control-plane*` stacks listed in the escrow manifest. To also re-run the cluster-provisioning stack, name it explicitly, for example `--stack stage0-kind-keel-mgmt`.

If the cluster itself is gone, the in-cluster bucket is gone too. Recovery then works only if you copied the state to another S3 endpoint yourself: point `--s3-endpoint` at it and use your saved keys and passphrase.

### 3. Inspect what is available

```bash
keel bootstrap recover --s3-endpoint http://127.0.0.1:8333 --cluster-name keel-mgmt --list-versions
```

This lists each stack checkpoint version (newest first) and any snapshots.

### 4. Dry run

```bash
keel bootstrap recover \
  --s3-endpoint http://127.0.0.1:8333 \
  --cluster-name keel-mgmt \
  --stack stage0-control-plane-keel-mgmt \
  --dry-run
```

The dry run prints the backend URL (`s3://pulumi-state/stage0/keel-mgmt?endpoint=...`), the stacks it will recover, the prerequisites, and the exact `pulumi` argv it would run. Secrets are passed only through environment variables and stdin, and never appear in argv.

Without `--stack`, a real run recovers every `control-plane*` stack in the escrow manifest (`s3://pulumi-state/stage0/<cluster>/manifest.json`: `stage0-control-plane-<cluster>`, plus `stage0-control-plane-forgejo-<cluster>` and `stage0-control-plane-rauthy-<cluster>` when bootstrap installed them). The dry run shows the same list: with `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` set it reads the manifest, otherwise it lists every control-plane stack Keel can create and says so. If the manifest cannot be read (S3 port-forward not up yet, unrelated AWS keys exported), the dry run still prints the plan with that candidate list and names the S3 error.

### 5. Execute

```bash
keel bootstrap recover \
  --s3-endpoint http://127.0.0.1:8333 \
  --cluster-name keel-mgmt \
  --stack stage0-control-plane-keel-mgmt \
  --passphrase-file /tmp/keel-passphrase
```

For each stack, recovery:

1. Re-materializes the program and its encrypted config from the escrow, falling back to the embedded program.
2. Optionally restores `--state-version <id>` or `--snapshot <timestamp>` first.
3. Runs `pulumi refresh` (unless you pass `--skip-refresh`), then `pulumi up`, against the S3 backend.
4. Copies the result back into the local durable backend.

Recovering onto a **new** cluster: add `--kubeconfig <file>` and `--context <ctx>`. These override the escrowed `kubeconfig` / `kubeContext` config for stacks that deploy into the cluster.

When you are done, remove `/tmp/keel-passphrase` and stop the port-forward.

## Flux Self-Upgrade Failures

After Stage 0, Flux manages itself. The protected `flux-system/flux2` HelmRelease (`clusters/management/core-addons/flux2.yaml`) adopts the Stage 0 `flux2` release, so a Flux upgrade is an ordinary management add-on PR. Such a PR can change only the chart version, `logLevel`, and controller sizing and scheduling. The multitenancy lockdown and CRD retention values are locked.

**Why the release does not wait.** helm-controller upgrades its own Deployment. With `helm upgrade --wait`, the old helm-controller pod would get SIGTERM as soon as the new pod is Ready, while it was still waiting on the other controllers. That leaves the release `pending-upgrade`, and the new controller would then roll a healthy upgrade back, in a loop. So `install`, `upgrade` and `rollback` all set `disableWait: true`. The upgrade applies the manifests and records the release as `deployed` within seconds, before the rollout replaces the old pod.

**Readiness and rollback.** The controller rollouts are the readiness gate. Every Flux controller Deployment uses RollingUpdate with `maxUnavailable: 0`, so a broken new controller never replaces the running one, and the old controllers keep reconciling. After a Flux upgrade, check the rollouts:

```bash
for d in $(kubectl -n flux-system get deployments -o name); do
  kubectl -n flux-system rollout status "$d" --timeout=10m
done
```

If a rollout stays stuck, revert the version with **Rollback** on the `flux2` row (**Topology Map → Deployed Application Releases**) or `POST /api/v1/management/addons/flux2/rollback`. That call opens a PR that restores the previous `flux2` HelmRelease revision. Once the PR merges, the still-running old controllers apply it. If applying an upgrade fails outright, Flux rolls the release back itself (`upgrade.remediation.strategy: rollback`).

**No install remediation.** Flux's install remediation is `helm uninstall`. On the `flux2` release that would delete every Flux controller, including the helm-controller running the uninstall. So `flux2` sets no `install.remediation`. If an install fails, for example while Flux re-adopts the release after its Helm release Secrets were lost, the release stays failed for an operator to fix.

**helm-controller is down.** If no helm-controller is left to remediate, Flux cannot repair itself:

- **Release still installed** (for example a broken upgrade that was never rolled back). Suspend the HelmRelease first. Otherwise the restored helm-controller sees drift from the bad version that is still in Git and upgrades to it again. Then roll back with the Helm CLI, revert the version in Git, and resume:

  ```bash
  kubectl -n flux-system patch helmrelease flux2 --type merge -p '{"spec":{"suspend":true}}'
  helm -n flux-system history flux2
  helm -n flux-system rollback flux2 <last-good-revision> --wait
  # Revert the version in Git (POST /api/v1/management/addons/flux2/rollback) and merge the PR.
  kubectl -n flux-system patch helmrelease flux2 --type merge -p '{"spec":{"suspend":false}}'
  ```

  Running `helm rollback --wait` by hand is safe here because the Helm CLI, not helm-controller, does the waiting.

- **Release missing** (for example after an uninstall). Run `keel bootstrap recover` for the control-plane stack, as described in the [Runbook](#runbook). Recovery reinstalls Flux from the Stage 0 inputs (`--stack stage0-control-plane-<cluster>`), and Flux then adopts the release again from Git. Because of `ignoreChanges`, recovery never touches an installed `flux2` release.

Flux CRDs carry `helm.sh/resource-policy: keep`, so an uninstall leaves every Flux object, and the workloads it manages, in place.

**Deleting a protected Flux object.** The `protect-control-plane` Kyverno policy denies `DELETE` of any `flux-system` HelmRelease or Kustomization labelled `keel.io/protect: "true"`. These objects are applied from Git by the `core-addons` (or `flux-system`) Kustomization, which puts the label back on its next reconcile. A label removed by hand therefore does not last. To delete such an object deliberately:

1. Suspend the parent Kustomization, or remove the label in Git instead:

   ```bash
   kubectl -n flux-system patch kustomization core-addons --type merge -p '{"spec":{"suspend":true}}'
   ```

2. Remove the label: `kubectl -n flux-system label helmrelease <name> keel.io/protect-`.
3. Delete the object.
4. Drop the object from Git, then resume the parent Kustomization (`"suspend":false`).

## Auth Phase During Recovery

While the control plane is in the `BreakGlass` auth phase, cluster-admin is granted **only** to the identities listed in `KEEL_BREAK_GLASS_ADMINS` on `keel-server`. This is a comma-separated list of OIDC `sub` or `email` values and defaults to `admin@keel.local`. Other users keep their normal group-based permissions. Sessions are bound to the phase they were issued under, so every session issued during break-glass becomes invalid when the phase changes.

## Teardown Caveat (`protect: true`)

Stage 0 core resources carry `protect: true`: the namespaces, SOPS `age` key, Flux, Kyverno, PKO, and the S3 state backend. Keel never runs `pulumi destroy` on these stacks. To tear one down deliberately with Pulumi, first run `pulumi state unprotect --all` for that stack, using the same backend and passphrase:

```bash
cd ~/.config/keel/stage0/keel-mgmt/programs/control-plane
export PULUMI_BACKEND_URL="file://$HOME/.config/keel/stage0/keel-mgmt/state"
export PULUMI_CONFIG_PASSPHRASE_FILE="$HOME/.config/keel/stage0/keel-mgmt/pulumi-passphrase"
pulumi state unprotect --all --stack stage0-control-plane-keel-mgmt
pulumi destroy --stack stage0-control-plane-keel-mgmt
```

For a local `kind` cluster, `kind delete cluster --name keel-mgmt` skips all of this.
