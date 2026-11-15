# Security, Multi-Tenancy & Canary Rollouts

## Multi-Tenant Isolation

Every tenant maps to a dedicated Kubernetes namespace (`tenant-<tenant-id>`) and Git directory (`tenants/<tenant-id>/`):

- **Flux Isolation:** Flux controllers run with `--no-cross-namespace-refs=true`, preventing one tenant from referencing another tenant's `GitRepository`, `Secret`, or `Kustomization`.
- **Pod Security Standards:** Tenant namespaces enforce the Kubernetes `restricted` Pod Security Standard.
- **Per-Tenant SOPS + `age` Keys:** Secrets submitted through catalog forms (`writeOnly: true` or `format: password`) are encrypted with the tenant's `age` public key before being committed to Git.

## Kyverno Admission Guardrails

Keel deploys Kyverno policies during Stage 0 bootstrap to enforce:

- **Provider Allowlisting:** PKO v2 `Stack`s can only execute providers that are enabled in `providers/registry.yaml`.
- **Docker Socket Protection:** Host Docker socket mounts are blocked by default. On local `kind` development clusters, passing `--local-dev-docker-socket-exception` to `keel bootstrap init` generates an explicit `PolicyException` scoped to PKO workspace pods so they can provision local workload `kind` clusters.

## Control-Plane Anti-Lockout

- Management-cluster core stacks carry `protect: true` (`clusters/management/management-stack.yaml`) so accidental deletion PRs cannot destroy the control plane.
- Break-glass recovery (`keel bootstrap recover`) restores management state directly from versioned S3 object storage.

## Progressive Delivery (Flagger + Envoy Gateway)

Applications whose `CatalogApplication` declares `spec.progressiveDelivery.supported: true` can opt into automated canary rollouts (`--canary` in CLI or the canary toggle in the GUI). Keel synthesizes a Flagger `Canary` resource alongside the Flux `HelmRelease`, shifting traffic through **Envoy Gateway** based on **Prometheus** error-rate and latency metrics.
