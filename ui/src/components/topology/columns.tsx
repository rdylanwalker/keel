import { createContext, useContext } from "react"
import {
  AlertTriangle,
  ExternalLink,
  Pencil,
  RotateCcw,
  RotateCw,
  Search,
  ShieldCheck,
  SlidersHorizontal,
  Terminal,
  Trash2,
} from "lucide-react"

import { CatalogItem, ReleaseSummary, StackSummary } from "../../types/api"
import { formatLastSynced } from "../../lib/format"
import { isStackSshSupported } from "../../lib/utils"
import { Authz } from "../../lib/authz"
import { BundleBadge } from "../BundleBadge"
import { DataTableColumnDef, DataTableRowProps } from "../DataTable"
import { StatusBadge } from "../StatusBadge"
import { IconAction } from "./IconAction"
import { Badge } from "../ui/badge"
import {
  MANAGEMENT_ADDON_CONFLICT_HINT,
  managementAddonId,
} from "./useManagementAddonActions"
import { TruncatedText } from "../ui/truncated-text"

export const MANAGEMENT_STACK_NAME = "management-cluster-core"

export function isManagementStack(stack: StackSummary): boolean {
  return (
    stack.type === "management" ||
    stack.name === MANAGEMENT_STACK_NAME ||
    stack.namespace === "flux-system" ||
    stack.namespace === "keel-system"
  )
}

/** Namespace a stack lives in (server value first; never fabricated beyond tenant mapping). */
export function stackNamespace(stack: StackSummary): string {
  if (stack.namespace) return stack.namespace
  if (stack.name === MANAGEMENT_STACK_NAME) return "flux-system"
  const tenant = stack.tenant || "default"
  return tenant.startsWith("tenant-") ? tenant : `tenant-${tenant}`
}

export interface StackRowActions {
  authz: Authz
  onEdit?: (namespace: string, name: string) => void
  onSsh: (stack: StackSummary, namespace: string) => void
  onInspect: (stack: StackSummary, namespace: string) => void
  onReconcile: (stack: StackSummary) => void
  onDelete: (stack: StackSummary) => void
  reconcilingKey: string | null
  deletingKey: string | null
}

export function buildStackColumns(
  headerName: string,
  includeSsh: boolean,
  actions: StackRowActions
): DataTableColumnDef<StackSummary>[] {
  return [
    {
      accessorKey: "name",
      header: headerName,
      meta: { grow: true },
      cell: ({ row }) => (
        <TruncatedText
          text={row.original.name}
          className="font-semibold text-slate-100"
        />
      ),
    },
    {
      accessorKey: "status",
      // Status collapses to a dot in narrow tables, so the header collapses with it.
      header: () => (
        <>
          <span aria-hidden="true" className="@md:hidden">
            ●
          </span>
          <span className="sr-only @md:not-sr-only">Status</span>
        </>
      ),
      cell: ({ row }) => (
        <StatusBadge status={row.original.status} collapsible />
      ),
    },
    {
      accessorKey: "tenant",
      header: "Tenant",
      meta: { hideBelow: "md" },
      cell: ({ row }) =>
        row.original.tenant ? (
          <TruncatedText
            text={row.original.tenant}
            className="max-w-[9rem] text-indigo-400"
          />
        ) : (
          <span className="text-slate-600">—</span>
        ),
    },
    {
      accessorKey: "provider",
      header: "Provider",
      meta: { hideBelow: "lg" },
      cell: ({ row }) =>
        row.original.provider ? (
          <span className="uppercase tracking-wider font-mono text-[10px]">
            {row.original.provider}
          </span>
        ) : (
          <span className="text-slate-600">—</span>
        ),
    },
    {
      accessorKey: "type",
      header: "Type",
      // Mostly redundant with the section (clusters / VMs / services); lowest priority.
      meta: { hideBelow: "3xl" },
      cell: ({ row }) => (
        <span className="text-slate-300 capitalize">{row.original.type}</span>
      ),
    },
    {
      accessorKey: "resourceCount",
      header: "Resources",
      meta: { hideBelow: "xl" },
      cell: ({ row }) => (
        <span className="font-mono text-slate-300">
          {row.original.resourceCount ?? "—"}
        </span>
      ),
    },
    {
      accessorKey: "bundle",
      header: "Bundle",
      meta: { hideBelow: "3xl" },
      cell: ({ row }) =>
        row.original.bundle ? (
          <BundleBadge
            bundle={row.original.bundle}
            bundleItem={row.original.bundleItem}
          />
        ) : null,
    },
    {
      accessorKey: "lastSynced",
      header: "Synced",
      meta: { hideBelow: "lg" },
      cell: ({ row }) => (
        <span
          className="text-slate-400 whitespace-nowrap"
          title={row.original.lastSynced || undefined}
        >
          {formatLastSynced(row.original.lastSynced)}
        </span>
      ),
    },
    {
      id: "actions",
      header: "Actions",
      enableSorting: false,
      meta: { align: "right", sticky: true },
      cell: ({ row }) => (
        <StackActionsCell
          stack={row.original}
          includeSsh={includeSsh}
          actions={actions}
        />
      ),
    },
  ]
}

function StackActionsCell({
  stack,
  includeSsh,
  actions,
}: {
  stack: StackSummary
  includeSsh: boolean
  actions: StackRowActions
}) {
  const ns = stackNamespace(stack)
  const key = `${ns}/${stack.name}`
  const isProtected = isManagementStack(stack) || Boolean(stack.protect)
  const canOperate = actions.authz.canAccessTenant(ns)
  const sshSupported = includeSsh && isStackSshSupported(stack)
  const noAccess = "You don't have access to this tenant"
  const protectedTitle = "Protected stack (protect: true) — managed via PR only"
  return (
    <div className="inline-flex items-center justify-end gap-1 @md:gap-1.5">
      {actions.onEdit && (
        <IconAction
          label={`Edit stack ${stack.name}`}
          title={isProtected ? protectedTitle : canOperate ? "Edit" : noAccess}
          disabled={isProtected || !canOperate}
          onClick={() => actions.onEdit?.(ns, stack.name)}
          tone="indigo"
        >
          <Pencil className="h-3.5 w-3.5" />
        </IconAction>
      )}
      {includeSsh && (
        <IconAction
          testId={`stack-ssh-btn-${stack.name}`}
          label={`SSH connect to stack ${stack.name}`}
          title={
            sshSupported
              ? "SSH Connect"
              : "SSH is not supported for this infrastructure"
          }
          disabled={!sshSupported}
          onClick={() => actions.onSsh(stack, ns)}
          tone="emerald"
        >
          <Terminal className="h-3.5 w-3.5" />
        </IconAction>
      )}
      <IconAction
        label={`Inspect stack ${stack.name}`}
        title="Inspect"
        onClick={() => actions.onInspect(stack, ns)}
        tone="indigo-soft"
      >
        <Search className="h-3.5 w-3.5" />
      </IconAction>
      <IconAction
        label={`Reconcile stack ${stack.name}`}
        title={canOperate ? "Reconcile Now" : noAccess}
        disabled={!canOperate || actions.reconcilingKey === key}
        onClick={() => actions.onReconcile(stack)}
        tone="blue"
      >
        <RotateCw
          className={`h-3.5 w-3.5 ${actions.reconcilingKey === key ? "animate-spin" : ""}`}
        />
      </IconAction>
      <IconAction
        label={`Delete stack ${stack.name}`}
        title={
          isProtected
            ? protectedTitle
            : canOperate
              ? "Delete (Open PR)"
              : noAccess
        }
        disabled={
          isProtected || !canOperate || actions.deletingKey === `stack:${key}`
        }
        onClick={() => actions.onDelete(stack)}
        tone="red"
      >
        <Trash2 className="h-3.5 w-3.5" />
      </IconAction>
    </div>
  )
}

/** Row actions specific to protected management add-on rows (cluster admins only). */
export interface ManagementRowActions {
  /** `catalog/core` entries by id; Upgrade is disabled for add-ons without one. */
  catalogById: ReadonlyMap<string, CatalogItem>
  onUpgrade: (addon: CatalogItem) => void
  onRollback: (id: string) => void
}

export interface ReleaseRowActions {
  authz: Authz
  onEdit?: (namespace: string, name: string) => void
  onInspect: (rel: ReleaseSummary, namespace: string) => void
  onReconcile: (rel: ReleaseSummary) => void
  onDelete: (rel: ReleaseSummary) => void
  /** Enables the management add-on actions (Diagnostics / Reconcile / Upgrade / Rollback). */
  management?: ManagementRowActions
}

/**
 * In-flight release row actions. Read by the action cells through context (not baked into the
 * column defs) so starting a reconcile / rollback / delete doesn't rebuild the columns and
 * re-mount the cells, which would drop keyboard focus returning from a confirmation dialog.
 */
export interface ReleaseRowBusyState {
  reconcilingKey: string | null
  deletingKey: string | null
  rollingBackId: string | null
}

export const ReleaseRowBusyContext = createContext<ReleaseRowBusyState>({
  reconcilingKey: null,
  deletingKey: null,
  rollingBackId: null,
})

/** Stable table row id; conflicting add-ons share namespace/name, so include the Flux kind. */
export function releaseRowId(rel: ReleaseSummary): string {
  if (rel.managementAddon)
    return `${rel.namespace}-${rel.name}-${rel.addonKind ?? ""}`
  return rel.id || `${rel.namespace}-${rel.name}`
}

/** Text matched by the table filter for a release name cell ("management"/"protected" find add-ons). */
function releaseNameFilterText(
  rel: ReleaseSummary,
  catalogById?: ReadonlyMap<string, CatalogItem>
): string {
  if (!rel.managementAddon) return rel.name
  const display = catalogById?.get(managementAddonId(rel))?.name
  return [rel.name, display, "management protected", rel.addonKind]
    .filter(Boolean)
    .join(" ")
}

/** Version shown for a release (the desired Git version for management add-ons). */
function releaseVersion(rel: ReleaseSummary): string {
  return (rel.managementAddon && rel.desiredVersion) || rel.version || "latest"
}

/** Row styling and test id distinguishing protected management add-on rows. */
export function releaseRowProps(
  rel: ReleaseSummary
): DataTableRowProps | undefined {
  if (!rel.managementAddon) return undefined
  return {
    "data-testid": `management-addon-row-${managementAddonId(rel)}`,
    className:
      "bg-indigo-950/20 shadow-[inset_3px_0_0_0_rgba(129,140,248,0.55)]",
  }
}

/** Management add-ons first (tenant releases keep their order); the table's default order. */
export function sortManagementFirst(
  releases: ReleaseSummary[]
): ReleaseSummary[] {
  return [
    ...releases.filter((r) => r.managementAddon),
    ...releases.filter((r) => !r.managementAddon),
  ]
}

function ManagementNameCell({
  rel,
  addon,
}: {
  rel: ReleaseSummary
  addon?: CatalogItem
}) {
  const id = managementAddonId(rel)
  const displayName = addon?.name && addon.name !== rel.name ? addon.name : null
  return (
    <div className="flex min-w-0 flex-col gap-1">
      <div className="flex min-w-0 items-center gap-1.5">
        <ShieldCheck
          className="h-3.5 w-3.5 shrink-0 text-indigo-400"
          aria-hidden="true"
        />
        <TruncatedText
          text={rel.name}
          className="font-semibold text-slate-100"
        />
      </div>
      {displayName && (
        <TruncatedText
          text={displayName}
          className="font-sans text-[10px] text-slate-500"
        />
      )}
      {addon?.description && (
        <span className="sr-only">{addon.description}</span>
      )}
      <div className="flex min-w-0 flex-wrap items-center gap-1.5 font-sans">
        <Badge title="Management-plane add-on (keel.io/protect: true) — changes open reviewed PRs">
          Protected
        </Badge>
        {rel.conflict && (
          <Badge
            variant="warning"
            role="note"
            data-testid={`management-addon-conflict-${id}`}
            title={MANAGEMENT_ADDON_CONFLICT_HINT}
            aria-label={`Conflict: ${MANAGEMENT_ADDON_CONFLICT_HINT}`}
          >
            <AlertTriangle className="h-3 w-3" aria-hidden="true" />
            Conflict
          </Badge>
        )}
        {rel.conflict && rel.addonKind && (
          <span
            className="text-[10px] text-slate-500"
            data-testid={`management-addon-kind-${id}`}
          >
            {rel.addonKind}
          </span>
        )}
        {addon?.serviceUrl && (
          <a
            href={addon.serviceUrl}
            target="_blank"
            rel="noreferrer"
            aria-label={addon.serviceUrl}
            title={`Open ${addon.name || id}: ${addon.serviceUrl}`}
            className="inline-flex items-center gap-1 text-[10px] text-indigo-400 hover:text-indigo-300"
          >
            <ExternalLink className="h-3 w-3" aria-hidden="true" />
            <span className="hidden @3xl:inline">Service</span>
          </a>
        )}
      </div>
    </div>
  )
}

export function buildReleaseColumns(
  actions: ReleaseRowActions
): DataTableColumnDef<ReleaseSummary>[] {
  const catalogById = actions.management?.catalogById
  return [
    {
      id: "name",
      accessorFn: (row) => releaseNameFilterText(row, catalogById),
      sortFn: (a, b) => a.original.name.localeCompare(b.original.name),
      header: "Release Name",
      meta: { grow: true },
      cell: ({ row }) => {
        const rel = row.original
        if (rel.managementAddon)
          return (
            <ManagementNameCell
              rel={rel}
              addon={catalogById?.get(managementAddonId(rel))}
            />
          )
        return (
          <div className="flex min-w-0 flex-col gap-1">
            <TruncatedText
              text={rel.name}
              className="font-semibold text-slate-100"
            />
            {(rel.canary ||
              rel.type === "canary" ||
              (rel.primaryVersion &&
                rel.version &&
                rel.primaryVersion !== rel.version &&
                rel.canaryPhase !== "Succeeded")) && (
              <div className="flex min-w-0 flex-wrap items-center gap-1.5">
                {(rel.canary || rel.type === "canary") && (
                  <span className="shrink-0 whitespace-nowrap text-[10px] px-1.5 py-0.5 rounded bg-purple-500/15 text-purple-300 border border-purple-500/30">
                    {rel.canaryWeight && rel.canaryWeight > 0
                      ? `Canary ${rel.canaryWeight}%`
                      : "Canary"}
                  </span>
                )}
                {rel.primaryVersion &&
                  rel.version &&
                  rel.primaryVersion !== rel.version &&
                  rel.canaryPhase !== "Succeeded" && (
                    <span
                      className="shrink-0 whitespace-nowrap text-[10px] font-mono text-amber-300"
                      title="Live primary version → desired canary version"
                    >
                      {rel.primaryVersion} → {rel.version}
                    </span>
                  )}
              </div>
            )}
          </div>
        )
      },
    },
    {
      accessorKey: "status",
      // Status collapses to a dot in narrow tables, so the header collapses with it.
      header: () => (
        <>
          <span aria-hidden="true" className="@md:hidden">
            ●
          </span>
          <span className="sr-only @md:not-sr-only">Status</span>
        </>
      ),
      cell: ({ row }) =>
        row.original.managementAddon ? (
          <span
            data-testid={`management-addon-status-${managementAddonId(row.original)}`}
          >
            <StatusBadge status={row.original.status} collapsible />
          </span>
        ) : (
          <StatusBadge status={row.original.status} collapsible />
        ),
    },
    {
      accessorKey: "namespace",
      header: "Namespace",
      meta: { hideBelow: "md" },
      cell: ({ row }) => (
        <TruncatedText
          text={row.original.namespace}
          className="max-w-[10rem] text-indigo-400 xl:max-w-[14rem]"
        />
      ),
    },
    {
      id: "targetCluster",
      accessorFn: (row) =>
        row.targetCluster
          ? row.targetCluster.replace(/-kubeconfig$/, "")
          : "local (mgmt)",
      header: "Target Cluster",
      meta: { hideBelow: "lg" },
      cell: ({ row }) =>
        row.original.targetCluster ? (
          <TruncatedText
            text={row.original.targetCluster.replace(/-kubeconfig$/, "")}
            className="inline-block max-w-[11rem] align-middle px-2 py-0.5 rounded bg-cyan-500/10 text-cyan-300 border border-cyan-500/20 text-[11px]"
          />
        ) : (
          <span className="text-slate-400 whitespace-nowrap">local (mgmt)</span>
        ),
    },
    {
      id: "bundle",
      accessorFn: (row) =>
        row.bundle
          ? `${row.bundle}${row.bundleItem ? `/${row.bundleItem}` : ""}`
          : "",
      header: "Bundle",
      meta: { hideBelow: "3xl" },
      cell: ({ row }) => (
        <BundleBadge
          bundle={row.original.bundle}
          bundleItem={row.original.bundleItem}
          fallback={<span className="text-slate-600">—</span>}
        />
      ),
    },
    {
      id: "chart",
      accessorFn: (row) => row.chart || row.name,
      header: "Chart / Source",
      meta: { hideBelow: "xl" },
      cell: ({ row }) => (
        <TruncatedText
          text={row.original.chart || row.original.name}
          className="max-w-[12rem] text-slate-300"
        />
      ),
    },
    {
      id: "version",
      accessorFn: releaseVersion,
      header: "Version",
      meta: { hideBelow: "lg" },
      cell: ({ row }) => (
        <span
          title={
            row.original.managementAddon
              ? "Desired version declared in Git"
              : undefined
          }
        >
          <TruncatedText
            text={releaseVersion(row.original)}
            className="max-w-[8rem] text-slate-400"
          />
        </span>
      ),
    },
    {
      id: "runtime",
      // Management add-ons show their Flux object kind (HelmRelease / Kustomization).
      accessorFn: (row) =>
        (row.managementAddon && row.addonKind) ||
        row.runtime ||
        row.type ||
        "helm",
      header: "Runtime",
      meta: { hideBelow: "2xl" },
      cell: ({ row }) =>
        row.original.managementAddon && row.original.addonKind ? (
          <span className="text-slate-400">{row.original.addonKind}</span>
        ) : (
          <span className="uppercase text-slate-400">
            {row.original.runtime || row.original.type || "helm"}
          </span>
        ),
    },
    {
      accessorKey: "lastSynced",
      header: "Synced",
      meta: { hideBelow: "lg" },
      cell: ({ row }) => (
        <span
          className="text-slate-400 whitespace-nowrap"
          title={row.original.lastSynced || undefined}
        >
          {formatLastSynced(row.original.lastSynced)}
        </span>
      ),
    },
    {
      id: "actions",
      header: "Actions",
      enableSorting: false,
      meta: { align: "right", sticky: true },
      cell: ({ row }) =>
        row.original.managementAddon ? (
          <ManagementActionsCell rel={row.original} actions={actions} />
        ) : (
          <ReleaseActionsCell rel={row.original} actions={actions} />
        ),
    },
  ]
}

/** Protected management add-on: Diagnostics, Reconcile, PR-driven Upgrade / Rollback; never Edit or Delete. */
function ManagementActionsCell({
  rel,
  actions,
}: {
  rel: ReleaseSummary
  actions: ReleaseRowActions
}) {
  const busy = useContext(ReleaseRowBusyContext)
  const id = managementAddonId(rel)
  const key = `${rel.namespace}/${rel.name}`
  const reconciling = busy.reconcilingKey === key
  const mgmt = actions.management
  const addon = mgmt?.catalogById.get(id)
  const conflict = Boolean(rel.conflict)
  const rollingBack = Boolean(mgmt) && busy.rollingBackId === id
  return (
    <div className="inline-flex items-center justify-end gap-1 @md:gap-1.5">
      <IconAction
        testId={`management-addon-diagnostics-${id}`}
        label={`Diagnostics for ${id}`}
        title="Diagnostics"
        onClick={() => actions.onInspect(rel, rel.namespace)}
        tone="indigo-soft"
      >
        <Search className="h-3.5 w-3.5" />
      </IconAction>
      <IconAction
        testId={`management-addon-reconcile-${id}`}
        label={`Reconcile ${id}`}
        title="Reconcile Now"
        disabled={reconciling}
        busy={reconciling}
        onClick={() => actions.onReconcile(rel)}
        tone="blue"
      >
        <RotateCw
          className={`h-3.5 w-3.5 ${reconciling ? "animate-spin" : ""}`}
        />
      </IconAction>
      {mgmt && (
        <>
          <IconAction
            testId={`management-addon-upgrade-${id}`}
            label={`Upgrade ${id}`}
            title={
              conflict
                ? MANAGEMENT_ADDON_CONFLICT_HINT
                : addon
                  ? "Upgrade (Open PR: version + values)"
                  : "No catalog/core entry for this add-on"
            }
            disabled={conflict || !addon}
            onClick={() => addon && mgmt.onUpgrade(addon)}
            tone="indigo"
          >
            <SlidersHorizontal className="h-3.5 w-3.5" />
          </IconAction>
          <IconAction
            testId={`management-addon-rollback-${id}`}
            label={`Rollback ${id}`}
            title={
              conflict
                ? MANAGEMENT_ADDON_CONFLICT_HINT
                : rollingBack
                  ? "Rolling back..."
                  : "Rollback (Open PR)"
            }
            disabled={conflict || rollingBack}
            busy={rollingBack}
            onClick={() => mgmt.onRollback(id)}
            tone="emerald"
          >
            <RotateCcw
              className={`h-3.5 w-3.5 ${rollingBack ? "animate-spin" : ""}`}
            />
          </IconAction>
        </>
      )}
    </div>
  )
}

function ReleaseActionsCell({
  rel,
  actions,
}: {
  rel: ReleaseSummary
  actions: ReleaseRowActions
}) {
  const busy = useContext(ReleaseRowBusyContext)
  const ns = rel.namespace || "tenant-default"
  const key = `${ns}/${rel.name}`
  const reconciling = busy.reconcilingKey === key
  const deleting = busy.deletingKey === `release:${key}`
  const canOperate = actions.authz.canAccessTenant(ns)
  const noAccess = "You don't have access to this tenant"
  return (
    <div className="inline-flex items-center justify-end gap-1 @md:gap-1.5">
      {actions.onEdit && (
        <IconAction
          label={`Edit release ${rel.name}`}
          title={canOperate ? "Edit" : noAccess}
          disabled={!canOperate}
          onClick={() => actions.onEdit?.(ns, rel.name)}
          tone="indigo"
        >
          <Pencil className="h-3.5 w-3.5" />
        </IconAction>
      )}
      <IconAction
        label={`Inspect release ${rel.name}`}
        title="Inspect"
        onClick={() => actions.onInspect(rel, ns)}
        tone="indigo-soft"
      >
        <Search className="h-3.5 w-3.5" />
      </IconAction>
      <IconAction
        label={`Reconcile release ${rel.name}`}
        title={canOperate ? "Reconcile Now" : noAccess}
        disabled={!canOperate || reconciling}
        busy={reconciling}
        onClick={() => actions.onReconcile(rel)}
        tone="blue"
      >
        <RotateCw
          className={`h-3.5 w-3.5 ${reconciling ? "animate-spin" : ""}`}
        />
      </IconAction>
      <IconAction
        label={`Delete release ${rel.name}`}
        title={canOperate ? "Delete (Open PR)" : noAccess}
        disabled={!canOperate || deleting}
        busy={deleting}
        onClick={() => actions.onDelete(rel)}
        tone="red"
      >
        <Trash2 className="h-3.5 w-3.5" />
      </IconAction>
    </div>
  )
}
