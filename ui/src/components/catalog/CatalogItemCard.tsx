import React from "react"
import { Box, Pencil, Plus, Trash2 } from "lucide-react"

import { Button } from "@/components/ui/button"
import { ConfirmDialog } from "@/components/ui/confirm-dialog"
import { CatalogItem } from "@/types/api"

export interface CatalogItemCardProps {
  item: CatalogItem
  canManage: boolean
  deleting: boolean
  onDelete: () => void
  onEdit: () => void
  onCreate: () => void
}

/** Catalog blueprint card with role-aware Edit / Delete and a Create (deploy) action. */
export const CatalogItemCard: React.FC<CatalogItemCardProps> = ({
  item,
  canManage,
  deleting,
  onDelete,
  onEdit,
  onCreate,
}) => {
  const [confirmingDelete, setConfirmingDelete] = React.useState(false)
  const deleteTitle = !canManage
    ? item.tenant
      ? `Requires tenant administrator privileges for '${item.tenant}'`
      : "Requires cluster administrator privileges"
    : item.protected
      ? "Protected catalog item — disable protection in Edit to delete"
      : "Delete catalog item via Git PR"
  return (
    <div className="p-5 rounded-2xl border border-slate-800 bg-slate-900 flex flex-col justify-between gap-4 text-left">
      <div className="space-y-2.5 min-w-0">
        <div className="flex items-start justify-between gap-2">
          <div className="flex flex-col gap-1 min-w-0">
            <span className="text-sm font-bold text-white flex items-center gap-2">
              <Box className="h-4 w-4 text-indigo-400 shrink-0" />
              <span className="break-words">{item.name}</span>
            </span>
            <div className="flex flex-wrap items-center gap-2 mt-1">
              {item.protected && (
                <span
                  data-testid={`catalog-protected-badge-${item.id}`}
                  className="text-[9px] uppercase font-bold px-1.5 py-0.5 rounded bg-amber-900/30 text-amber-500 border border-amber-900/50"
                >
                  Protected
                </span>
              )}
              {item.tenant ? (
                <span className="text-[9px] uppercase font-bold px-1.5 py-0.5 rounded bg-slate-800 text-slate-400 border border-slate-700">
                  Tenant: {item.tenant}
                </span>
              ) : (
                <span className="text-[9px] uppercase font-bold px-1.5 py-0.5 rounded bg-indigo-900/30 text-indigo-400 border border-indigo-900/50">
                  Global
                </span>
              )}
            </div>
          </div>
          <span className="text-[10px] font-mono px-2 py-0.5 rounded bg-slate-800 text-slate-400 shrink-0">
            {item.type === "bundle"
              ? "bundle"
              : `v${item.version.replace(/^v/i, "")}`}
          </span>
        </div>
        <p className="text-xs text-slate-400 line-clamp-2">
          {item.description}
        </p>
        <div className="flex flex-wrap items-center justify-between gap-2 text-[10px] pt-1">
          <span className="px-2 py-0.5 rounded-full bg-slate-800 text-slate-300 font-medium">
            {item.category}
          </span>
          <div className="flex items-center gap-2 min-w-0">
            {item.provider && (
              <span className="uppercase font-mono text-indigo-400">
                {item.provider}
              </span>
            )}
            <span className="font-mono text-slate-500 truncate">{item.id}</span>
          </div>
        </div>
      </div>

      <div className="flex items-center justify-between gap-2 pt-3 border-t border-slate-800/80">
        <Button
          variant="ghost"
          size="icon"
          data-testid={`delete-catalog-item-${item.id}`}
          aria-label={`Delete catalog item ${item.name}`}
          title={deleteTitle}
          disabled={!canManage || Boolean(item.protected) || deleting}
          onClick={() => setConfirmingDelete(true)}
          className="hover:text-red-400 disabled:opacity-30"
        >
          <Trash2 className="h-4 w-4" />
        </Button>
        <div className="flex items-center gap-2">
          {canManage && (
            <Button
              variant="secondary"
              onClick={onEdit}
              title="Edit catalog item definition via Git PR"
            >
              <Pencil className="h-3.5 w-3.5" />
              <span>Edit</span>
            </Button>
          )}
          <Button onClick={onCreate}>
            <Plus className="h-3.5 w-3.5" />
            <span>Create</span>
          </Button>
        </div>
      </div>

      <ConfirmDialog
        open={confirmingDelete}
        onOpenChange={setConfirmingDelete}
        title={`Delete catalog item ${item.name}?`}
        description={
          <>
            This opens a Git pull request removing{" "}
            <code className="text-slate-300">{item.id}</code> from the catalog.
            Existing deployments are not affected until the PR is merged.
          </>
        }
        confirmLabel="Delete via PR"
        confirmTestId={`confirm-delete-catalog-item-${item.id}`}
        destructive
        onConfirm={() => {
          setConfirmingDelete(false)
          onDelete()
        }}
      />
    </div>
  )
}
