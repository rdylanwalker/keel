# Web Portal

`keel serve` embeds a React + TypeScript single-page app built with **Shadcn UI** (Radix primitives in `ui/src/components/ui/`, configured by `ui/components.json`) and Tailwind CSS.

## Views

| Tab (hash route) | Purpose |
| :--- | :--- |
| **Topology Map** (`#topology`) | Live management, workload, and cloud stacks and app releases. Status overlays come from Flux and PKO through the SSE stream, with polling only while SSE is disconnected. Includes the reconciliation event timeline, Flagger canary gauges, and the Edit / Delete / Reconcile / SSH actions. For cluster admins, the **Deployed Application Releases** table lists the protected management add-ons (Flux `flux2`, SeaweedFS `s3-state-backend`, PKO `pulumi-kubernetes-operator`, Kyverno, Envoy Gateway, Prometheus, Flagger, cert-manager, plus the optional Rauthy, Forgejo, and in-cluster keel-server) as rows above the tenant releases. Each add-on row has a shield icon, a **Protected** badge, and the catalog display name under the release name, and shows live status, namespace, chart (HelmRelease) or add-on id (Kustomization), desired version, Flux kind (`HelmRelease` or `Kustomization`), and last sync. Its actions are Diagnostics / Reconcile / Upgrade / Rollback. Type `management` or `protected` in the table filter to show only add-ons. The rows stay visible when a tenant is selected in the header. Upgrade (version plus a values form built from the add-on's `catalog/core` schema) and Rollback open PRs; an add-on defined by more than one Flux object shows one row per Flux object, each with a conflict warning and its Flux kind, and both actions stay disabled. Add-ons can't be edited or deleted like tenant releases. |
| **Catalog & Form Wizard** (`#catalog`, `#catalog/<cluster\|vm\|service\|app>`) | Browse the catalog, deploy items through schema-driven forms, and author catalog entries. |
| **PR & Preview Inspector** (`#preview`, `#preview/<pr-id>`) | Open, merged, closed, and all PRs, with the merge-base Git diff, the preview report (source badge and run status), and Merge / Reject / Reopen. |
| **Control Plane & Providers** (`#providers`) | Provider enablement (opened as a PR) and, for cluster admins, the management cluster health summary derived from add-on status, with a link to the add-ons in the Topology Map. |

Deep links, such as `#preview/42`, are kept across the OIDC login round-trip.

## Schema-Driven Forms for Unmodified Charts

Catalog entries carry a JSON Schema (`catalog.yaml`). The GUI renders it with **`@rjsf/core`**, using a Shadcn widget and template registry (`ui/src/components/rjsf/`), and serializes the result directly into a standard Helm `values.yaml` override. Upstream charts are never forked or patched.

- `uiSchema` hints are honored.
- `writeOnly` / `format: password` fields use a password widget and are **masked** in the `values.yaml` preview. On submit they are SOPS-encrypted server-side.
- Raw-manifest `${var}` rules are mirrored on the client, so the form rejects values the server would reject. Server validation errors are mapped back onto the form fields.

## Role-Aware Controls

The GUI mirrors the server's authorization rules: cluster admin, tenant admin (`tenant-<id>-admin`), and tenant member (`tenant-<id>`). Controls you can't use are hidden or disabled. Examples are provider enable/disable, add-on upgrade and rollback, Merge, Force-delete, and Unlock. The server enforces every check regardless.

The GUI never fabricates state. If you are not an admin, the management-cluster row simply isn't shown, rather than being filled with placeholder data.

API errors are shown as readable messages. For 5xx errors the GUI also shows the server's correlation id, which you can match against the server log.

## SSH Web Terminal (wterm)

**Launch Browser Terminal** in the SSH modal opens an interactive shell over the authenticated WebSocket bridge (`/api/v1/stacks/{ns}/{name}/ssh/ws`, `russh` on the server). The terminal is rendered with **[wterm](https://github.com/vercel-labs/wterm)** (`@wterm/dom` 0.6, Apache-2.0, from Vercel Labs). wterm pairs a small WASM VT core with a DOM renderer: about 50 KB gzipped of JS plus about 32 KB gzipped of WASM that loads lazily, compared with about 89 KB gzipped for xterm.js. Because the output is ordinary DOM text, you can select it, search it with the browser's find, and read it with a screen reader.

`keel-server`'s Content-Security-Policy allows `'wasm-unsafe-eval'` so the terminal's WASM core can compile. It does not allow JavaScript `eval`.

The modal shows the server's host-key `SHA256:` fingerprint with a **Host key verified** or **Host key NOT verified** badge. The key is verified only when the stack publishes an expected host key. See [Security](../operations/security-and-tenancy.md#vm-ssh-access).

## Demo Mode

`?demo=true` shows fixture data without a cluster. Demo fixtures are **compiled out of production bundles**. Demo mode works only in the Vite dev server (`just dev-ui`) or in a bundle built with `VITE_ENABLE_DEMO=true`, which `just serve-demo` does for you. Run `just build-ui` afterwards to restore a production bundle. In demo mode the GUI shows a persistent **DEMO DATA** banner and makes no live API calls, so a crafted link can never make a production control plane show fabricated state.

## Screenshots

The screenshots under `docs/assets/screenshots/` are captured with `just screenshots` (Playwright) against a running server. They may predate the latest UI changes.
