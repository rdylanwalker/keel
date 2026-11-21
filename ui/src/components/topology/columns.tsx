import { Pencil, RotateCw, Search, Terminal, Trash2 } from "lucide-react"

import { ReleaseSummary, StackSummary } from "../../types/api"
import { formatLastSynced } from "../../lib/format"
import { isStackSshSupported } from "../../lib/utils"
import { Authz } from "../../lib/authz"
import { BundleBadge } from "../BundleBadge"
import { DataTableColumnDef } from "../DataTable"
import { StatusBadge } from "../StatusBadge"
import { IconAction } from "./IconAction"
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

export interface ReleaseRowActions {
  authz: Authz
  onEdit?: (namespace: string, name: string) => void
  onInspect: (rel: ReleaseSummary, namespace: string) => void
  onReconcile: (rel: ReleaseSummary) => void
  onDelete: (rel: ReleaseSummary) => void
  reconcilingKey: string | null
  deletingKey: string | null
}

export function buildReleaseColumns(
  actions: ReleaseRowActions
): DataTableColumnDef<ReleaseSummary>[] {
  return [
    {
      accessorKey: "name",
      header: "Release Name",
      meta: { grow: true },
      cell: ({ row }) => {
        const rel = row.original
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
      cell: ({ row }) => (
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
      accessorFn: (row) => row.version || "latest",
      header: "Version",
      meta: { hideBelow: "lg" },
      cell: ({ row }) => (
        <TruncatedText
          text={row.original.version || "latest"}
          className="max-w-[8rem] text-slate-400"
        />
      ),
    },
    {
      id: "runtime",
      accessorFn: (row) => row.runtime || row.type || "helm",
      header: "Runtime",
      meta: { hideBelow: "2xl" },
      cell: ({ row }) => (
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
      cell: ({ row }) => (
        <ReleaseActionsCell rel={row.original} actions={actions} />
      ),
    },
  ]
}

function ReleaseActionsCell({
  rel,
  actions,
}: {
  rel: ReleaseSummary
  actions: ReleaseRowActions
}) {
  const ns = rel.namespace || "tenant-default"
  const key = `${ns}/${rel.name}`
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
        disabled={!canOperate || actions.reconcilingKey === key}
        onClick={() => actions.onReconcile(rel)}
        tone="blue"
      >
        <RotateCw
          className={`h-3.5 w-3.5 ${actions.reconcilingKey === key ? "animate-spin" : ""}`}
        />
      </IconAction>
      <IconAction
        label={`Delete release ${rel.name}`}
        title={canOperate ? "Delete (Open PR)" : noAccess}
        disabled={!canOperate || actions.deletingKey === `release:${key}`}
        onClick={() => actions.onDelete(rel)}
        tone="red"
      >
        <Trash2 className="h-3.5 w-3.5" />
      </IconAction>
    </div>
  )
}
