# Security Policy

Keel bootstraps clusters and stores forge tokens, OIDC session keys, SSH keys and SOPS `age` keys, so security reports are very welcome.

## Reporting a Vulnerability

Please **do not** open a public issue, pull request or discussion for a security problem.

Report it privately through GitHub Security Advisories: open the [Security tab](https://github.com/rdylanwalker/keel/security) of this repository and click **Report a vulnerability**, or go directly to <https://github.com/rdylanwalker/keel/security/advisories/new>.

Please include:

- the Keel version (`keel --version`) and the bootstrap target and forge you used;
- what an attacker can do, and from which position (for example a tenant member, a tenant admin, an unauthenticated network client);
- steps or a proof of concept to reproduce it;
- any logs or configuration that help, with secrets removed.

## Supported Versions

Keel is alpha software. During the alpha, only the **latest release** receives security fixes. Fixes are not backported, and breaking releases have no upgrade path, so a fix may mean re-bootstrapping onto the new release.

## Scope

In scope:

- the `keel` CLI and `keel-server` (API, web UI, authentication and sessions);
- Keel-generated Kubernetes manifests, RBAC, Kyverno policies and Stage 0 programs in this repository (`deploy/`);
- tenant isolation: a tenant reading or changing another tenant's or the platform's data, repositories, secrets or cloud credentials;
- secret handling: credentials written to Git unencrypted, returned by the API, or logged;
- the release artifacts and their signatures.

Out of scope, unless Keel's configuration of them causes the problem:

- vulnerabilities in upstream projects Keel installs (Flux CD, Pulumi and PKO, Kyverno, Forgejo, Rauthy, SeaweedFS, Envoy Gateway, Flagger, Prometheus, cert-manager). Please report those upstream.
- limitations that are already documented in [Known Limitations](./docs/reference/known-limitations.md) and [Security & Tenancy: Residual exposure](./docs/operations/security-and-tenancy.md#residual-exposure).
- the local development defaults: the self-signed CA, `*.keel.local` host names, the ephemeral in-cluster Forgejo and the admin password printed by `keel bootstrap init`.

## What to Expect

Keel is a small alpha project with limited maintainer time. We aim to:

- acknowledge your report within about a week;
- tell you whether we can reproduce it and how we plan to fix it within about a month;
- credit you in the advisory and release notes, unless you prefer not to be named.

These are goals, not guarantees. Please give us a reasonable time to release a fix before you disclose the issue publicly, and coordinate the disclosure date with us through the advisory.
