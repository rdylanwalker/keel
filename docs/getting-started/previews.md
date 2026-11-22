# Pre-Merge PKO Previews

After Stage 0, every change Keel makes is a Pull Request. For Pulumi-backed changes, Keel also runs a **real `pulumi preview`** before merge, using an ephemeral PKO v2 `Stack` (`spec.preview: true`) in the `keel-previews` namespace. Reviewers see what would actually change in their clouds, not a guess built from YAML.

## Which Changes Get a Real Preview

| Change | Preview |
| :--- | :--- |
| Infrastructure blueprint (`keel infra provision`, GUI blueprints) | Real PKO preview. |
| Raw-manifest app with `--engine pulumi` | Real PKO preview of the `ConfigGroup` program. |
| Helm / Kustomize apps and raw-manifest apps with `--engine flux` | No Pulumi program exists, so the report is **synthesized** from the proposed manifests and marked as such. |
| No live cluster connected (e.g. `just serve`) | Synthesized, with `run.phase: notExecuted`. |

## Reading a Preview Report

Every report returned by `GET /api/v1/tenants/{tenant}/proposals/{id}/preview`, shown by `keel preview status`, and displayed in the PR & Preview Inspector carries two fields:

- **`source`**
  - `pko`: parsed from a real `pulumi preview` run in the PKO workspace pod.
  - `synthesized`: simulated by the control plane from the proposed manifests. A synthesized report is labelled **"Simulated diff — PKO preview not executed"**, has no policy verdict, and is never presented as PKO output.
- **`run.phase`**: the status of the ephemeral PKO run.
  - `pending`: the preview `Stack` is applied, but PKO has not started an `Update` yet.
  - `running`: the PKO preview `Update` is in progress.
  - `succeeded`: `pulumi preview` completed, and the report now comes from PKO.
  - `failed`: `pulumi preview` failed, or the preview `Stack` was rejected at admission. `run.message` gives the reason.
  - `notExecuted`: no PKO preview ran. The reason is in `run.message`, for example a Flux-only change, no cluster, or missing tenant credentials.

`run.stack` (`keel-previews/<name>`) is set only when a preview `Stack` was actually applied. The CLI prints a preview stack only in that case.

## Lifecycle

1. The proposal is pushed to `keel/<tenant>/<action>-<timestamp>` and a PR is opened. The PR title is a Conventional Commit, such as `feat(<tenant>): provision <name>`.
2. `keel-server` server-side-applies a preview `Stack` into `keel-previews`. The workspace pod makes a shallow clone of the proposal branch directly from the forge (PKO `gitSource`). It authenticates with a per-preview copy of the tenant's read-only Git credential, which is deleted together with the preview. No Flux `GitRepository` or `source-controller` artifact is involved. The repository URL is the one in the platform-written `tenants/<id>/git-repository.yaml`, the same source the tenant's own Stacks clone.
3. Keel immediately posts a **provisional** PR comment ("Provisional diff — PKO preview running"), synthesized from the proposed manifests.
4. PKO runs `pulumi preview`. Keel follows the `Stack` status and the `auto.pulumi.com` `Update` object, then parses the plan from the workspace pod's `pulumi` log. PKO doesn't store per-resource preview steps anywhere else.
5. Keel stores the report in a `ConfigMap` owned by the preview `Stack` and records the commit it covers. It then posts the **real** results as a follow-up PR comment and pushes progress over SSE (`keel preview status --watch`, GUI).
6. A **preview reconciler runs every 60 s** (with jitter). It compares live preview objects with forge PR states. Previews for merged or closed PRs are garbage-collected, and finished runs are re-published. All preview state lives in the cluster, not in server memory, so this keeps working across `keel-server` restarts and replicas.

## Credentials Used by Previews

Previews never receive control-plane credentials:

- The workspace pod runs as `keel-previews/keel-preview-readonly`, which has TokenReview and read-only `view` access but **no Secret access**.
- The preview mirrors the tenant's **scoped** SeaweedFS identity (limited to `pulumi-state/tenants/<id>/*`) into `keel-previews`. It clones the tenant's own repository at the PR's head commit, with its own copy of the tenant's **read-only** repository credential (`keel-preview-<key>-keel-gitops-credentials`, only the keys PKO needs), owned by the preview `Stack` and deleted with it. `<key>` is the repository-scoped preview key (`<tenant>-<pr>` for tenant repositories). If the tenant has no scoped identity, the preview is not executed (`notExecuted`) instead of falling back to admin keys.
- Every preview `Stack` uses the tenant state backend `s3://<bucket>/tenants/<id>`, both for changes to existing stacks and for brand-new blueprints. Keel pins the backend when it synthesizes the preview and refuses to launch a preview whose backend is outside the tenant prefix.
- **Cloud provider credentials.** Keel copies exactly the Secrets that the tenant `Stack` references through `envRefs` / `secretsRef`, and only the keys it references. They go into `keel-previews` as `keel-preview-<pr>-<name>`, labelled `keel.io/pr-id` and `keel.io/tenant`. Each copy has an `ownerReference` to the preview `Stack`, so Kubernetes deletes it when the preview is garbage-collected. The preview `Stack`'s refs are rewritten to point at the copies. For a new stack, the `<instance>-secrets` values come from the PR itself, because Flux has not decrypted them into the tenant namespace yet. Secrets that the Stack doesn't reference are never copied. The read-only preview ServiceAccount still can't read Secrets through the API: the kubelet injects the referenced values into the workspace pod. If a referenced Secret or key is missing, the preview is torn down and reported as `notExecuted`.

## Limitations

- The PR comment table and GUI show only what PKO prints in its workspace log, up to the last 4000 lines.
- A preview needs the tenant's repository credential (`tenant-<id>/keel-gitops-credentials`). If it is missing or unusable, the preview is reported as `notExecuted` with the reason; run `keel tenant repair-credentials <id>` to re-mint it. Keel never falls back to the platform repository or a platform token.
- Every proposal for a tenant (Helm, manifest, bundle and Stack) is refused with `409 Conflict` until the skeleton PR in the tenant's repository (adding `tenants/<id>/kustomization.yaml`) is merged. Pulumi Stack proposals also need the platform stub PR merged, so that the tenant's `keel-gitops` source points at the tenant repository; until then they are refused with `409 Conflict` (logged with `audit="stack-source-denied"`). See [Create a tenant](provisioning.md#2-create-a-tenant) for the merge order.
- GitHub-hosted tenant repositories are cloned over SSH. Previews pin the PR commit and mount the pinned host keys, but the PKO operator does not get those host keys yet, so GitHub-hosted tenant Stacks are not verified end to end.
- With the development-only Local Git provider, tenants created through the API have no Git source of their own, so their Pulumi Stacks and Pulumi-engine apps are refused with `409`.

## Deep Links

The GUI preview inspector can be deep-linked with `#preview/<tenant>/<pr-id>` (PR numbers are per repository; use `management` for platform-repository PRs). The link survives the OIDC login round-trip.
