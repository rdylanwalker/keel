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

> **Per-tenant repositories (upcoming).** A later Keel release will give each tenant its own repository on the same forge, with a read-only credential that works only on that repository. Keel will not create these repositories or credentials on external forges until then, but plan the operator token now:
>
> - **GitHub:** use a fine-grained token whose resource owner is the organisation, with the repository permissions `Administration: Read & write` (creating organisation repositories and adding per-repository **deploy keys**), `Contents: Read & write`, `Pull requests: Read & write` and `Issues: Read & write`. A classic PAT needs only `repo`, from an organisation member who is allowed to create repositories. Do not grant `admin:org` or the organisation `Administration` permission for this: they also manage members, teams and organisation settings. Tenant credentials will be read-only SSH deploy keys, one per repository, with `ssh.github.com:443` as the fallback when port 22 is blocked. Organisation policy must allow deploy keys. Disable private repository forking: an organisation owner turns off *Member privileges → Repository forking* in the UI (or calls `PATCH /orgs/{org}` with `members_can_fork_private_repositories: false`, which needs `admin:org` or organisation `Administration: write`; grant that only if you want Keel to set the policy). Keel will also set `allow_forking: false` on each tenant repository when it creates them (a later release).
> - **Branch protection:** Keel will protect `main` on every tenant repository (no direct or force pushes, no deletion, also for admins; changes only through merged PRs, with no required approvals because Keel merges after its own checks). GitHub offers branch protection on private repositories only on paid plans (Pro, Team, Enterprise); on GitHub Free Keel will report that `main` could not be protected, and the forge will not block direct pushes to tenant repositories. On GitLab, Keel replaces the default `main` rule (Maintainers may push) with "no one may push, Maintainers may merge".
> - **GitLab:** the token needs the `api` scope with at least the Maintainer role on the target group, so it can create projects, create project **deploy tokens** (`read_repository`) and set `forking_access_level: disabled` on each tenant project.
> - **Forgejo/Gitea (external):** the token needs `write:organization` on the organisation that will hold tenant repositories. Repository-scoped read tokens need Forgejo 15 or later and a dedicated bot account like the one `--with-forge` creates (see [Per-tenant repository foundations](#per-tenant-repository-foundations)); Gitea has no repository-scoped tokens. Set `[repository] DISABLE_FORKS = true`, `[service] ALLOW_ONLY_EXTERNAL_REGISTRATION = true` (or `DISABLE_REGISTRATION = true`) and `[service] DEFAULT_ALLOW_CREATE_ORGANIZATION = false` on the instance.

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
- Does **not** share that operator token with tenants. `tenant-default` gets its own private repository next to the platform repository (`<owner>/<repo>-tenant-default`) and a read-only credential that works on that repository only (GitHub: a read-only deploy key; GitLab: a project deploy token), stored in `tenant-default/keel-gitops-credentials`. Every tenant created later gets the same. The token therefore needs to create repositories, deploy keys/tokens and branch protection in that owner or group. With an external Forgejo/Gitea, create the `keel-tenants` organisation and a restricted tenant-reader bot first and store its `username`/`password` in `keel-system/keel-forgejo-tenant-reader`.
- Creates the repository (private) if it does not exist yet. Keel first works out who the owner in the URL is: an organisation (GitHub, Forgejo/Gitea) or GitLab group, or the token owner's own account. It refuses any other account instead of creating the repository somewhere else. A Forgejo/Gitea token needs `write:user` (which includes `read:user`) plus `write:repository` to create a repository in its own account, and `write:organization` (which includes the `read:organization` Keel uses to recognise the organisation) for an organisation; otherwise create the repository by hand first.
- Commits the Stage 0 seed layout to `main` and pushes to `origin` if reachable.

### Step 3: Start `keel serve`

```bash
keel serve --bind 127.0.0.1:8080 --repo-dir ./keel-gitops
```

`keel serve` reads `KEEL_GIT_TOKEN` (or `--git-token`, or the `keel-system/forge-token` Secret) and `.keel/config.yaml` to open real Pull Requests via `GitHubProvider`, `GitLabProvider`, or `ForgejoGiteaProvider`.

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

### Per-tenant repository foundations

Bootstrap also prepares the forge for per-tenant repositories, where each tenant's GitOps content will live in its own repository that only that tenant's credentials can read. Keel does not create tenant repositories yet, and `tenant-default` still uses `keel/infra-gitops`. Every step below is idempotent, so a re-run repairs drift, and every step fails closed on objects Keel does not control:

| Object | Settings |
| :--- | :--- |
| Organisation `keel-tenants` | Private, `repo_admin_change_team_access: false`. Created by the Stage 0 admin `keel`, the account that owns `forge-token`. Tenant repositories will be created here. On every run the `Owners` team must contain exactly `keel`, and no team other than `Owners` and `tenant-readers` may cover all repositories or have any repository. Otherwise bootstrap stops setting up the foundations, because someone else created or changed the organisation. |
| Bot user `keel-tenant-reader` | Not an admin. Restricted, so it sees only what it is explicitly given. Cannot create repositories or organisations, has no git hooks or local imports, and has private visibility. Its email is `keel-tenant-reader-<random>@users.noreply.invalid`: the `.invalid` domain is reserved, so no identity provider can issue it and SSO account linking by email (`ACCOUNT_LINKING: auto`) can never sign in as the bot. The random password, the email and the account's Forgejo id are stored only in the `keel-system/keel-forgejo-tenant-reader` Secret (keys `username`, `password`, `email`, `userId`) and never written to Git or logs. Each run sets the Forgejo password back to the value in that Secret. Keel adopts an existing `keel-tenant-reader` account only if its id matches `userId`; an account someone else registered first is refused, and you must delete it in Forgejo before bootstrap continues. On every run Keel also deletes every token the bot owns that Keel did not mint (anything not named `keel-tenant-repo-*`, scoped to `read:repository` only and restricted to `keel-tenants` repositories), and all of the bot's SSH keys, GPG keys and OAuth2 applications. A bot with two-factor authentication enabled fails the login check. |
| Team `tenant-readers` in `keel-tenants` | Read access to the `repo.code` unit only, on every current and future repository in the organisation (`includes_all_repositories`). Its only member is the bot: Keel removes anyone else on every run. |
| Forgejo hardening values | `[repository] DISABLE_FORKS = true`: tenant users will get read access to their own repository, and without this setting they could fork it and keep the fork after they lose access. `[service] ALLOW_ONLY_EXTERNAL_REGISTRATION = true`: no local sign-up, so nobody can register Keel's account names first (OIDC users are still created on first login). `[service] DEFAULT_ALLOW_CREATE_ORGANIZATION = false`: only site admins create organisations, so nobody can create `keel-tenants` first. Set in both the Stage 0 release values and the seed `HelmRelease`, but see the note on existing clusters below. |

**Existing clusters with an ephemeral Forgejo.** Any change to the Forgejo values restarts its pod, and with `persistence.enabled: false` the restart deletes every repository, including `keel/infra-gitops`, which Flux reconciles from. So when bootstrap finds a running Forgejo without persistence whose configuration lacks the hardening values, it leaves them out of `core-addons/forgejo.yaml` and prints a note. Forks, local sign-up and organisation creation by non-admins then stay enabled. The fail-closed checks above still protect the bot, the organisation and the team. To get the hardening, back up Forgejo (`forgejo dump` or a pushed mirror), set `persistence.enabled: true` in `core-addons/forgejo.yaml` in Git, and only then re-run `keel bootstrap init`. Fresh installs always get the hardening values.

**Why a bot with a password.** Each tenant's Flux `GitRepository` and PKO Stacks will get their own HTTPS token. The token has only the `read:repository` scope and is restricted to that one repository (Forgejo `POST /api/v1/users/{user}/tokens` with `repositories: [{owner, name}]`). Repository-specific access tokens were added in **Forgejo 15**. Earlier Forgejo versions and Gitea silently ignore `repositories` and mint an account-wide token; Keel detects that, deletes the token and fails, so it never stores one. A repository-specific token can still read *public* repositories that are not on its list; tenant repositories are private, so this does not widen access to them. Forgejo mints tokens only for the account that authenticates with basic auth. The least-privileged account that can do this is a dedicated bot that can read nothing outside `keel-tenants` and can write nothing. The alternatives rejected were giving keel-server the Stage 0 admin password (site admin and sudo), or a `write:user` token, which would also control the admin's own keys and settings.

**`forge-token` scopes.** The `keel-system/forge-token` API token has `write:repository`, `write:issue`, `read:user` and `write:organization`, and no `write:user` or `write:admin`. `write:organization` is needed to create repositories in `keel-tenants`. Its owner, `keel`, is a site admin, and Forgejo treats a site admin as an owner of every organisation. With this scope the token can therefore change the teams, members and settings of **every** organisation on the instance, including user-created ones, and could add accounts to `tenant-readers`. Treat `forge-token` as an instance-wide administrative credential: Stage 0 re-checks the `keel-tenants` owners and team membership on every run, but between runs only the Secret's access control protects it. Clusters bootstrapped before this release keep their old token until the next `keel bootstrap init`; on an ephemeral Forgejo, back it up first (see above). Each run mints a new `keel-stage0-*` token and does not revoke older ones, because a running keel-server keeps using the token it started with; delete stale `keel-stage0-*` tokens in Forgejo (*Settings → Applications* as `keel`) after keel-server has restarted.

If any of these steps fails, bootstrap does not abort. It prints a note, and you re-run `keel bootstrap init` to retry.

Then start `keel serve`:

```bash
keel serve --bind 127.0.0.1:8080 --repo-dir ./keel-gitops
```

`keel serve` automatically reads `keel-system/forge-token` and opens a background port-forward to Forgejo when running on `localhost`, so you can immediately submit and merge Pull Requests from the UI or CLI.

### How PKO clones the in-cluster Forgejo

Flux pulls the repository over the plain `http://forgejo-http...` Service URL. The Pulumi Kubernetes Operator only clones `https://` and `ssh://` URLs, so tenant and preview PKO Stacks clone the same repository through the Forgejo HTTPS ingress instead:

- Bootstrap writes `forge.publicUrl` to `.keel/config.yaml`: the Forgejo HTTPS origin derived from `--oidc-public-url` (default `https://forgejo.keel.local:8888`, same host pattern and port as the IdP). `keel serve` reads it at start-up and turns `http://forgejo-http.keel-system.svc.cluster.local:3000/<org>/<repo>.git` into `<publicUrl>/<org>/<repo>.git` for every Stack `projectRepo`.
- In-cluster pods resolve the Forgejo host to Envoy Gateway through a CoreDNS rewrite that bootstrap applies whenever `--with-forge` is set.
- The ingress certificate is signed by the self-signed Keel CA. Bootstrap copies the CA's public certificate (`ca.crt` only) into the `keel-ca-bundle` ConfigMap in `keel-system`, `keel-previews` and every existing tenant namespace, and the Kyverno `sync-keel-ca-bundle` policy copies it into tenant namespaces created later. After the CA is renewed, re-run `keel bootstrap init` to refresh every copy. The PKO operator and the Stack workspace pods mount it at `/etc/keel-ca` with `SSL_CERT_DIR=/etc/ssl/certs:/etc/keel-ca`.

If a Pulumi proposal fails with `409` and asks you to set `forge.publicUrl`, the repository's `.keel/config.yaml` predates this setting: add it under `forge:` on `main` (for example `publicUrl: https://forgejo.keel.local:8888`) and restart `keel serve`. Stack URLs on external HTTPS forges are used as they are. Tenant Pulumi Stacks refuse SSH sources (`ssh://` or `git@host:path`) with `409` for now, because they clone with the read-only HTTP(S) `git-credentials` token and SSH deploy keys are not supported yet; if tenants need Pulumi Stacks, use an `https://` `--git-url`. Any other `http://` URL is refused, because PKO cannot clone it.

> **Note:** Inside a local `kind` cluster, in-cluster Forgejo storage is ephemeral if you delete the `kind` cluster. Use an external Git repository (`--git-url`) for durable state.

---

## 3. Local Filesystem Git (`--repo-dir` only)

When you omit `--git-url` and `--with-forge`, `keel` creates a local bare repository alongside your working tree (e.g. `./keel-gitops.git`) and uses `LocalGitProvider` to emulate PRs with `proposal/*` branches:

```bash
keel bootstrap init --target kind --cluster-name keel-mgmt --repo-dir ./keel-gitops
keel serve --bind 127.0.0.1:8080 --repo-dir ./keel-gitops
```

> **Note:** Pods inside `kind` cannot mount or clone a host filesystem path, so Flux's `GitRepository` is skipped in this mode and resources remain `Pending` unless you switch to an external Git repository or `--with-forge`.
