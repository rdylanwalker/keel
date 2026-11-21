import React, { useState } from "react"
import {
  AlertCircle,
  Loader2,
  Maximize2,
  Minimize2,
  Plus,
  X,
} from "lucide-react"

export interface CreateTenantModalProps {
  isOpen: boolean
  newTenantId: string
  onNewTenantIdChange: (value: string) => void
  tenantLoading: boolean
  tenantError: string | null
  onSubmit: (e: React.FormEvent) => void
  onClose: () => void
  onCancel: () => void
}

export const CreateTenantModal: React.FC<CreateTenantModalProps> = ({
  isOpen,
  newTenantId,
  onNewTenantIdChange,
  tenantLoading,
  tenantError,
  onSubmit,
  onClose,
  onCancel,
}) => {
  const [isTenantModalFullscreen, setIsTenantModalFullscreen] = useState(false)

  if (!isOpen) return null

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Create New Tenant"
      className={`fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm ${
        isTenantModalFullscreen ? "p-0" : "p-4"
      }`}
    >
      <div
        data-testid="new-tenant-modal"
        className={`bg-slate-900 border-slate-800 flex flex-col shadow-2xl ${
          isTenantModalFullscreen
            ? "w-full h-full rounded-none border-0 p-6"
            : "border rounded-2xl max-w-4xl w-full max-h-[90vh] p-6"
        }`}
      >
        <div className="flex items-center justify-between border-b border-slate-800 pb-3 shrink-0">
          <h3 className="text-base font-bold text-white flex items-center gap-2">
            <Plus className="h-4 w-4 text-indigo-400" />
            Create New Tenant
          </h3>
          <div className="flex items-center gap-1.5">
            <button
              type="button"
              onClick={() => setIsTenantModalFullscreen((prev) => !prev)}
              aria-label={
                isTenantModalFullscreen
                  ? "Exit full screen"
                  : "Enter full screen"
              }
              title={
                isTenantModalFullscreen ? "Exit full screen" : "Full screen"
              }
              className="p-1 rounded-lg text-slate-400 hover:text-white hover:bg-slate-800 transition-colors cursor-pointer"
            >
              {isTenantModalFullscreen ? (
                <Minimize2 className="h-4 w-4" />
              ) : (
                <Maximize2 className="h-4 w-4" />
              )}
            </button>
            <button
              type="button"
              onClick={() => {
                setIsTenantModalFullscreen(false)
                onClose()
              }}
              aria-label="Close modal"
              className="p-1 rounded-lg text-slate-400 hover:text-white hover:bg-slate-800 transition-colors cursor-pointer"
            >
              <X className="h-5 w-5" />
            </button>
          </div>
        </div>

        <form onSubmit={onSubmit} className="flex-1 min-h-0 flex flex-col mt-4">
          <div className="flex-1 min-h-0 overflow-y-auto space-y-4 pr-1">
            <div>
              <label
                htmlFor="new-tenant-id"
                className="block text-xs text-slate-300 font-medium mb-1"
              >
                Tenant ID
              </label>
              <input
                id="new-tenant-id"
                type="text"
                required
                placeholder="e.g. acme"
                value={newTenantId}
                onChange={(e) => onNewTenantIdChange(e.target.value)}
                className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
              />
            </div>

            {tenantError && (
              <div className="p-3 bg-red-950/40 border border-red-500/30 rounded-lg text-red-300 text-xs flex items-center gap-2">
                <AlertCircle className="h-4 w-4 text-red-400 shrink-0" />
                <span>{tenantError}</span>
              </div>
            )}
          </div>

          <div className="flex items-center justify-end gap-3 pt-4 border-t border-slate-800 shrink-0 mt-4">
            <button
              type="button"
              onClick={onCancel}
              className="px-4 py-2 bg-slate-800 hover:bg-slate-700 text-slate-300 text-xs font-medium rounded-lg transition-colors"
            >
              Cancel
            </button>
            <button
              type="submit"
              disabled={tenantLoading || !newTenantId.trim()}
              className="px-4 py-2 bg-indigo-600 hover:bg-indigo-500 text-white text-xs font-semibold rounded-lg transition-colors disabled:opacity-50 flex items-center gap-1.5"
            >
              {tenantLoading ? (
                <>
                  <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  <span>Creating...</span>
                </>
              ) : (
                <span>Create Tenant PR</span>
              )}
            </button>
          </div>
        </form>
      </div>
    </div>
  )
}
