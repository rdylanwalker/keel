# Break-Glass Recovery

Keel is built so that you cannot lock yourself out of the control plane (`NFR-REL-01`). If the management cluster's add-ons are damaged, or the Git forge or `keel-server` is unreachable, you can re-apply the Stage 0 control plane straight from escrowed Pulumi state. Git, PRs, and the API are not involved.

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

> **Escrow the passphrase.** On the first run, `keel bootstrap init` prints a one-time notice with the passphrase location. Copy the passphrase into your password manager or vault. The S3 escrow cannot be decrypted without it if you lose both this workstation and the cluster.

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

If the cluster itself is gone, use your escrowed copies of the S3 keys and passphrase, and point `--s3-endpoint` at wherever the bucket now lives.

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
