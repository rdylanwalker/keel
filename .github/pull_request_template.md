## Summary

<!-- Brief summary of what this PR does, why it is needed, and how it was implemented. -->

## Related Issues / PRs

<!-- Reference related issue(s) or PR(s) if applicable, e.g.: Fixes #123, Resolves #456, or Relates to #789 -->

Fixes #

## Type of Change

<!-- Please mark the relevant option(s) with an [x]: -->

- [ ] `feat`: New feature or enhancement (minor SemVer bump)
- [ ] `fix`: Bug fix (patch SemVer bump)
- [ ] `feat!`: Breaking change (major SemVer bump)
- [ ] `refactor`: Code refactoring without behavior change
- [ ] `docs`: Documentation update
- [ ] `test`: Adding or updating tests
- [ ] `ci` / `chore`: Build system, CI workflow, or dependency updates

## Architectural & Spec Guardrails Checklist

<!-- Keel enforces strict architectural invariants from SDD.md and AGENTS.md. -->

- [ ] **Stateless Control Plane:** No embedded or external database introduced; state resides in Git and Kubernetes Secrets (`NFR-REL-01`).
- [ ] **Flux CD & PKO v2:** Uses Flux CD for GitOps reconciliation and PKO v2 for IaC (no Argo CD, Crossplane, or custom CRDs).
- [ ] **Direct `git` CLI:** All Git operations execute via explicit argument vectors (`tokio::process::Command::new("git")`), not shell invocation (`NFR-SEC-06`).
- [ ] **Zero Upstream Modifications:** Upstream Helm charts/Kustomize bases remain unmodified; customizations flow through `catalog.yaml` and `values.yaml`.
- [ ] **Secret Hygiene:** No unencrypted credentials or secrets in Git or logs; Git secrets encrypted via SOPS + `age` (`NFR-SEC-05`).
- [ ] **SDD Alignment:** If introducing a new architectural pattern or changing contracts, `SDD.md` has been updated alongside the code.

## Verification & Quality Gates

<!-- Please confirm which quality gates were run locally before submitting: -->

- [ ] `just agent-rust <crate>` or `cargo test -p <crate>` passed for modified Rust crates
- [ ] `just agent-ui` or `pnpm --dir ui test:unit` passed if UI was modified
- [ ] `just agent-e2e` or `pnpm --dir e2e test` passed if E2E or user-facing workflows were modified
- [ ] Code formatting and linting pass (`cargo fmt`, `clippy -D warnings`, `pnpm format:check`, `pnpm lint`)
- [ ] Conventional Commit message guidelines followed for all commits

## Additional Notes / Screenshots

<!-- Add any relevant screenshots, terminal recordings, or deployment logs (redacting secrets). -->
