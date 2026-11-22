# Upgrading from Keel 0.9

This release stops sharing Flux `source-controller` artifacts with tenants. Tenant and preview PKO Stacks now clone their own Git source with PKO `gitSource`, and the objects Keel writes for each tenant are labelled `keel.io/platform-owned` and protected by Kyverno. Upgrade in this order. In particular, `keel tenant migrate-stack-sources` only works after step 2.

1. **Upgrade the CLI and `keel-server`.** At start-up, the server logs a `legacy-stack-source` warning for each tenant Stack that still uses `fluxSource`, and a `platform-owned-baseline` warning for each tenant baseline that has no label.
2. **Re-run `keel bootstrap init` with your original flags and `--repo-dir`.** This run:
   - writes `forge.publicUrl` to `.keel/config.yaml` (with `--with-forge`);
   - writes the `keel-ca-bundle` ConfigMaps and the PKO operator CA mount, then restarts PKO;
   - applies the CoreDNS rewrite for the Forgejo host;
   - relabels `tenant-default` and installs the new Kyverno policies, including `sync-keel-ca-bundle`.

   Then restart `keel serve` or the in-cluster `keel-server`, which reads `forge.publicUrl` only at start-up. Until no PKO Stack outside `flux-system` uses `fluxSource` any more, port 9090 stays open to the namespaces of those Stacks, and the bootstrap summary lists them. Without `forge.publicUrl`, Pulumi proposals and Stack migrations for in-cluster Forgejo sources return `409` (see [How PKO clones the in-cluster Forgejo](../getting-started/git-repositories.md#how-pko-clones-the-in-cluster-forgejo)).
3. **Migrate each tenant's Stacks.** Run `keel tenant migrate-stack-sources --tenant <id>`, with `--dry-run` first, then merge the PR.
4. **Relabel each tenant's baseline.** Run `keel tenant migrate-baseline --tenant <id>` and merge the PR. It also adds a missing `allow-pko-workspace` NetworkPolicy. Step 2 already relabels `tenant-default` when you pass `--repo-dir`.
5. **Re-run `keel bootstrap init`.** This limits port 9090 to `flux-system`. If the summary says `origin/main` could not be fetched, pull the `--repo-dir` repository first.
6. **Clean up old previews.** Close or re-run PRs whose previews were created before `gitSource`.

PKO workspaces in `tenant-default` used to have unrestricted egress. They are now limited to the allowlist in [Security, Tenancy & Canary](security-and-tenancy.md#multi-tenant-isolation). A private forge or provider API on a port other than 22, 443, 6443 or 8006 needs an extra tenant NetworkPolicy; add it before step 2.

The PKO management add-on form also changed: `controller.replicas` and `controller.securityContext` are gone, replaced by `replicaCount` and `controller.logLevel`. Upgrade PRs that set the old keys are rejected with `400`.
