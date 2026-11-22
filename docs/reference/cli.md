# CLI Reference (`keel`)

Every flag on this page comes from `keel <command> --help` for the current build. Run `keel <command> --help` to see the exact help for the binary you have installed.

## Global Options

These options work with every subcommand:

| Option | Description |
| :--- | :--- |
| `--server <URL>` / `KEEL_SERVER` | Base URL of `keel-server` for API commands. Defaults to `http://127.0.0.1:8080`. |
| `-v`, `-vv` | Verbose errors. By default each error prints on one line. `-v` prints the full cause chain, and `-vv` adds more detail. |
| `-h`, `--help` / `-V`, `--version` | Help and version. `-V` is accepted only at the top level. |

`--tenant` always takes a **bare tenant id**, such as `default`. A leading `tenant-` prefix is stripped, so `--tenant tenant-default` and `--tenant default` mean the same thing.

## Authentication

| Command | Description |
| :--- | :--- |
| `keel auth login [--no-browser] [--force-prompt] [--timeout-secs <N>]` | Browser login using OIDC Authorization Code + PKCE with an RFC 8252 loopback redirect. The CLI listens on `http://127.0.0.1:<random>/callback` and opens `/api/v1/auth/cli/login`. After you sign in, it exchanges the single-use code at `/api/v1/auth/cli/exchange` for a Bearer session token. `--no-browser` only prints the URL, `--force-prompt` makes the IdP ask for credentials again, and `--timeout-secs` defaults to `300`. |
| `keel auth status` | Shows the stored session and the identity the server reports for it. |
| `keel auth logout` | Removes the stored session token for this server. |

Session tokens are stored per server URL in `credentials.json` (mode `0600`) in the Keel config directory: `$KEEL_CONFIG_DIR`, else `$XDG_CONFIG_HOME/keel`, else `~/.config/keel`. `http://localhost:<port>` and `http://127.0.0.1:<port>` count as the same server. Tokens expire after 1 hour. They are also bound to the server's auth phase, so a phase change, such as leaving break-glass, invalidates them.

```bash
keel auth login --server http://127.0.0.1:8080
keel auth status
```

## Proposals (Pull Requests)

| Command | Description |
| :--- | :--- |
| `keel proposal list [--tenant <id>] [--status open\|merged\|closed\|all] [--output table\|json]` | Lists proposals with their tenant and repository. Without `--tenant`, lists every repository you can see (cluster admins: the platform repository and every tenant repository); repositories the forge could not list are reported on stderr. Defaults to `--status open`. |
| `keel proposal show <PR> --tenant <id> [--output table\|json]` | Shows one proposal. PR numbers are per repository: `--tenant` is the tenant whose repository holds the PR, or `management` for platform-repository PRs (catalog, management add-ons, tenant stubs). |
| `keel proposal merge <PR> --tenant <id> [--output ...]` | Merges an open proposal in the `--tenant` repository. The server pins the merge to the head SHA it reviewed, so the merge is refused if the branch moved after verification. Requires cluster admin or `tenant-<id>-admin` (tenant repository PRs whose changes stay under `tenants/<id>/`). |
| `keel proposal close <PR> --tenant <id> [--output ...]` | Closes (rejects) an open proposal. Its ephemeral preview is garbage-collected. |
| `keel preview status --pr <PR> --tenant <id> [--watch]` | Renders the preview diff (`+` create, `~` update, `-` delete, `!` replace) and whether it came from a real PKO run or was simulated. `--watch` streams SSE progress until the preview finishes. |

## Catalog, Applications & Infrastructure

| Command | Description |
| :--- | :--- |
| `keel catalog list [--tenant <id>] [--output table\|json]` | Lists applications, infrastructure blueprints, and bundles. |
| `keel catalog show <ID> [--output table\|json]` | Shows one catalog entry, including its values JSON Schema. |
| `keel app install --app <ID> --tenant <id> --values-file <FILE> [--canary] [--engine flux\|pulumi]` | Validates values against the catalog JSON Schema and opens a deployment PR. `--engine` applies only to raw-manifest apps. |
| `keel app list [--tenant <id>] [--output table\|json]` | Lists deployed releases (Flux `HelmRelease`s and `Kustomization`s). |
| `keel infra list [--tenant <id>] [--provider <id>] [--output table\|json]` | Lists PKO `Stack`s with live status. |
| `keel infra provision --blueprint <ID> --tenant <id> --values-file <FILE>` | Validates values against a blueprint schema and opens a provisioning PR. |
| `keel infra ssh <STACK> [--tenant <id>] [--user <u>] [--port <p>] [-i <key>] [--exec] [--output table\|json\|command]` | Prints SSH connection info for a VM stack, or with `--exec` runs `ssh` directly (no shell involved). `--tenant` defaults to `default`. Unknown stacks and stacks that are not VMs return an error instead of made-up connection info. |
| `keel infra force-delete <STACK> [--tenant <id>] [--namespace <ns>] [--output ...]` | Clears finalizers and force-deletes a stuck PKO `Stack`. `--tenant` defaults to `default` and resolves to `tenant-<id>` unless `--namespace` is given. Requires tenant admin. Protected (`keel.io/protect`) stacks are refused. |
| `keel infra unlock <STACK> [--tenant <id>] [--namespace <ns>] [--output ...]` | Cancels a stale Pulumi state lock and requests reconciliation. `--tenant` defaults to `default` and resolves to `tenant-<id>` unless `--namespace` is given. Requires tenant admin. |

## Tenants

The CLI does not create or delete tenants: use **+ New Tenant** in the portal (see [Create a Tenant](../getting-started/provisioning.md#2-create-a-tenant)) or `POST` / `DELETE /api/v1/tenants`. Each tenant gets its own repository, and its proposals use `--tenant <id>`.

| Command | Description |
| :--- | :--- |
| `keel tenant repair-credentials [<id>] [--output table\|json]` | Cluster admin only (`keel tenants` is an alias). Re-mints the tenant's repository read credential and rewrites the cluster-only `tenant-<id>/keel-gitops-credentials` Secret, then revokes the previous credential and every other read credential of the tenant repository (`POST /api/v1/tenants/{id}/credentials/repair`). A missing `sops-age` Secret is created; an existing one is never replaced. Use it when a credential was lost, leaked or expired; Flux and PKO pick up the new one on their next pull. Fails with `409` while the tenant namespace does not exist yet or once the tenant's removal has merged, and with `400` for the Local Git provider (no tenant repositories). Without an id every tenant is repaired: tenants answering `409` are reported as skipped, other failures are reported per tenant and make the command exit non-zero after the full report (`--output json` prints `{"results": [...], "failed": n}`), and the Local Git provider stops the run. Secret values are never printed. |

## Server

| Command | Description |
| :--- | :--- |
| `keel serve [--bind <ADDR>] [--repo-dir <DIR>] [--bare-repo <DIR>] [--config <FILE>] [--git-token <TOKEN>] [--git-url <URL>] [--git-username <USER>]` | Starts the stateless API/SSE server and the embedded SPA. `--bind` defaults to `127.0.0.1:8080`. `--git-token` can also be set with `KEEL_GIT_TOKEN`. `--git-url` / `KEEL_GIT_URL` sets up `--repo-dir` from a remote GitOps repository: it clones when the directory is missing or empty and fetches otherwise. Credentials come only from `KEEL_GIT_TOKEN`, never from the URL. `--git-username` / `KEEL_GIT_USERNAME` is the HTTPS username paired with the token (default `keel`). See [Running keel-server In-Cluster](../operations/in-cluster-server.md). |

## Stage 0 Bootstrap & Recovery

| Command | Description |
| :--- | :--- |
| `keel bootstrap init --target <kind\|kubeconfig\|proxmox\|aws\|gcp\|azure\|hetzner> ...` | Stage 0 seed bootstrap. See [Stage 0 Bootstrap](../getting-started/bootstrap.md). |
| `keel bootstrap recover --s3-endpoint <URL> ...` | Break-glass recovery: re-applies the escrowed Stage 0 Pulumi state from S3 (`pulumi refresh` + `pulumi up`). See [Break-Glass Recovery](../operations/break-glass-recovery.md). |

### `keel bootstrap init` flags

| Flag | Description |
| :--- | :--- |
| `--target <T>` | Required. One of `kind`, `kubeconfig`, `proxmox`, `aws`, `gcp`, `azure`, `hetzner`. |
| `--cluster-name <NAME>` | Seed cluster name (DNS-1123 label). Default `keel-mgmt`. |
| `--kubeconfig <FILE>`, `--context <CTX>` | Existing cluster to use (`--target kubeconfig`). |
| `--proxmox-url <URL>`, `--region <R>`, `--distro <k3s\|talos>` | Settings for provider targets. |
| `--with-forge` | Deploys an in-cluster Forgejo in `keel-system` during Stage 0. Bootstrap creates the `keel/infra-gitops` repository and the `keel-system/forge-token` Secret. It also sets up the per-tenant repository foundations: the private `keel-tenants` organisation, the restricted `keel-tenant-reader` bot with its credentials in `keel-system/keel-forgejo-tenant-reader`, the read-only `tenant-readers` team, and forks, local sign-up and non-admin organisation creation disabled (see [Per-tenant repository foundations](../getting-started/git-repositories.md#per-tenant-repository-foundations)). It then hands Forgejo to Flux as the protected `forgejo` management add-on (`clusters/management/core-addons/forgejo.yaml`, `catalog/core/forgejo`). Persistence is off by default, so the management API refuses Forgejo upgrades and rollbacks until you set `persistence.enabled: true` in Git. Bootstrap also writes the Forgejo HTTPS origin to `.keel/config.yaml` as `forge.publicUrl` (derived from `--oidc-public-url`, default `https://forgejo.keel.local:8888`) and adds a CoreDNS rewrite that sends that host to Envoy Gateway, so tenant and preview PKO Stacks can clone the in-cluster Forgejo over HTTPS with the Keel CA trusted. See [How PKO clones the in-cluster Forgejo](../getting-started/git-repositories.md#how-pko-clones-the-in-cluster-forgejo). |
| `--with-keel-server` | Runs `keel-server` in the cluster as the Flux-managed `keel-server` add-on, with the image pinned to this CLI's release. Needs `--repo-dir` plus either `--with-forge` or an HTTP(S) `--git-url` (not both). See [Running keel-server In-Cluster](../operations/in-cluster-server.md). |
| `--git-url <URL>`, `--forge-type <local\|github\|gitlab\|forgejo\|gitea>`, `--forge-api-url <URL>`, `--git-token <T>` / `KEEL_GIT_TOKEN` | External Git repository and forge. A remote `--git-url` requires `--git-token`, because bootstrap also creates `tenant-default`'s own repository and read-only credential with it (see [Token permissions for tenant repositories](../getting-started/git-repositories.md#token-permissions-for-tenant-repositories)). A local-path `--git-url` (or `--forge-type local`) selects the development-only Local Git provider, which keeps tenants in the platform repository without isolation. |
| `--oidc-provider <rauthy\|none>` | In-cluster IdP to deploy. Default `rauthy`. |
| `--oidc-issuer <URL>`, `--oidc-client-id <ID>` | Use an external OIDC issuer instead. This disables in-cluster Rauthy. `--oidc-client-id` defaults to `keel-server`. |
| `--oidc-public-url <URL>` | Public HTTPS URL of in-cluster Rauthy. Default `https://idp.keel.local:8888`. The issuer becomes `<url>/auth/v1`, and the Forgejo and portal hosts are derived from the parent domain (`forgejo.<domain>`, `keel.<domain>`). |
| `--admin-username <U>`, `--admin-password <P>` / `KEEL_ADMIN_PASSWORD` | Initial Rauthy admin. `--admin-username` defaults to `admin@keel.local`. The password is generated and printed once if you omit it. |
| `--local-dev-docker-socket-exception` | Adds a Kyverno `PolicyException` that gives local `kind` PKO workspaces Docker access. |
| `--repo-dir <DIR>` | Local directory for the seed GitOps repository. |
| `--skip-core-install` | Skips the Stage 0 control-plane install: the `control-plane*` Pulumi programs (Flux, PKO, Kyverno, SeaweedFS, Envoy Gateway, Prometheus, Flagger, cert-manager, Rauthy, and Forgejo with `--with-forge`), Forgejo repository/token provisioning, and the Stage 0 S3 state escrow. The cluster is still provisioned and the seed GitOps repository is still initialized. With `--with-keel-server`, you must create the `keel-system/forge-token` Secret yourself. |
| `--pulumi-program-dir <DIR>` | Overrides the directory of the Stage 0 `Pulumi.yaml`. |

Environment variables read by `bootstrap init`:

- `KEEL_FORGEJO_ADMIN_PASSWORD`: overrides the generated Forgejo admin password.
- `KEEL_CONFIG_DIR` / `XDG_CONFIG_HOME`: relocate the Stage 0 state directory.

### `keel bootstrap recover` flags

| Flag | Description |
| :--- | :--- |
| `--s3-endpoint <URL>` | Required. S3 endpoint holding the escrow, for example `http://127.0.0.1:8333` through a port-forward. |
| `--bucket <B>` | Default `pulumi-state`. |
| `--region <R>` | Default `us-east-1`. Must match the region used when the state was written. |
| `--cluster-name <NAME>` | Default `keel-mgmt`. Selects `stage0/<cluster>/`. |
| `--stack <stage0-<program>-<cluster>>` | Repeatable. Defaults to the `control-plane*` stacks in the escrow manifest, with `stage0-control-plane-<cluster>` first. The cluster-provisioning stack, such as `stage0-kind-<cluster>`, is recovered only when you name it explicitly. |
| `--kubeconfig <FILE>`, `--context <CTX>` | Target cluster if it changed since bootstrap. Overrides the escrowed `kubeconfig` / `kubeContext` config. |
| `--passphrase-file <FILE>` | Stage 0 Pulumi passphrase. Otherwise read from `PULUMI_CONFIG_PASSPHRASE` or the local `pulumi-passphrase` file. |
| `--state-version <ID>` / `--snapshot <TS>` | Restores a specific S3 object version or a timestamped snapshot before `refresh` / `up`. |
| `--list-versions` | Lists the available state versions and snapshots, then exits. |
| `--skip-refresh` | Skips `pulumi refresh` before `pulumi up`. |
| `--dry-run` | Prints the plan without executing anything. |
