# Git Repositories & Forges

Keel stores all desired cluster state, tenant definitions, and catalog blueprints in a GitOps repository (`infra-gitops`). Depending on whether you are running a durable environment, a self-contained local cluster, or offline UI tests, you can choose one of three Git setups.

| Mode | Flux In-Cluster Sync? | Durable Across `kind` Recreate? | Best For |
| :--- | :--- | :--- | :--- |
| **External Git (GitHub / GitLab / Forgejo)** | Yes (outbound `1m` poll) | Yes | Real deployments & local `kind` behind NAT |
| **In-Cluster Forge (`--with-forge`)** | Yes (cluster-internal HTTP) | Ephemeral on `kind` | Self-contained local testing with real PRs |
| **Local Filesystem (`--repo-dir` only)** | No (pods cannot read host paths) | Local disk only | Offline UI/PR workflow testing |

---

## 1. External Git (GitHub, GitLab, or External Forgejo) — Recommended

Because Flux CD uses **outbound pull-polling** (`interval: 1m`), a local `kind` or `k3s` cluster behind NAT or a firewall can reconcile a remote GitHub or GitLab repository without inbound ports or webhooks. If you delete and recreate `kind`, your Git history and PRs remain intact.

### Step 1: Create an empty repository & Personal Access Token (PAT)

1. Create a new repository on GitHub/GitLab/Forgejo (for example `https://github.com/my-org/keel-gitops.git`).
2. Create a PAT with repository read/write + Pull Request permissions and export it:
   - **GitHub (Fine-grained PAT scoped to the repo):** `Pull requests` (Read & Write), `Issues` (Read & Write, for PR preview comments), and `Contents` (Read & Write) — or a Classic PAT with `repo`.
   - **GitLab (Project or Personal Access Token):** `api` scope (with Developer or Maintainer role on the project).

```bash
export KEEL_GIT_TOKEN="ghp_your_token_here"
```

### Step 2: Bootstrap with `--git-url`

```bash
keel bootstrap init \
  --target kind \
  --cluster-name keel-mgmt \
  --local-dev-docker-socket-exception \
  --repo-dir ./keel-gitops \
  --git-url https://github.com/my-org/keel-gitops.git
```

What this does automatically:
- Infers the forge provider (`github`, `gitlab`, or `forgejo`) from `--git-url` (override with `--forge-type` / `--forge-api-url` for self-hosted instances).
- Writes `.keel/config.yaml` with `forge.provider`, `forge.repo_url`, and `forge.api_url` (never storing your token in Git).
- Creates the `flux-system/git-credentials` Kubernetes `Secret` from `KEEL_GIT_TOKEN` (or `--git-token`) and attaches `spec.secretRef` to Flux's `GitRepository`.
- Commits the Stage 0 seed layout to `main` and pushes to `origin` if reachable.

### Step 3: Start `keel serve`

```bash
keel serve --bind 127.0.0.1:8080 --repo-dir ./keel-gitops
```

`keel serve` reads `KEEL_GIT_TOKEN` (or `--git-token`) and `.keel/config.yaml` to open real Pull Requests via `GitHubProvider`, `GitLabProvider`, or `ForgejoGiteaProvider`.

---

## 2. In-Cluster Forgejo (`--with-forge`)

If you want real Flux pull-polling and Forge PRs without an external GitHub/GitLab account, pass `--with-forge` during bootstrap:

```bash
keel bootstrap init \
  --target kind \
  --cluster-name keel-mgmt \
  --local-dev-docker-socket-exception \
  --with-forge \
  --repo-dir ./keel-gitops
```

This installs a single-pod SQLite **Forgejo** instance into `keel-system` (`forgejo-http.keel-system.svc.cluster.local:3000`), automatically creates the `keel/infra-gitops` repository, mints an API token into the `keel-system/forge-token` Kubernetes `Secret`, and pushes the seed `main` branch.

Then start `keel serve`:

```bash
keel serve --bind 127.0.0.1:8080 --repo-dir ./keel-gitops
```

`keel serve` automatically reads `keel-system/forge-token` and opens a background port-forward to Forgejo when running on `localhost`, so you can immediately submit and merge Pull Requests from the UI or CLI.

> **Note:** Inside a local `kind` cluster, in-cluster Forgejo storage is ephemeral if you delete the `kind` cluster. Use an external Git repository (`--git-url`) for durable state.

---

## 3. Local Filesystem Git (`--repo-dir` only)

When you omit `--git-url` and `--with-forge`, `keel` creates a local bare repository alongside your working tree (e.g. `./keel-gitops.git`) and uses `LocalGitProvider` to emulate PRs with `proposal/*` branches:

```bash
keel bootstrap init --target kind --cluster-name keel-mgmt --repo-dir ./keel-gitops
keel serve --bind 127.0.0.1:8080 --repo-dir ./keel-gitops
```

> **Note:** Pods inside `kind` cannot mount or clone a host filesystem path, so Flux's `GitRepository` is skipped in this mode and resources remain `Pending` unless you switch to an external Git repository or `--with-forge`.
