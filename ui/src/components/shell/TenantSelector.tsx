import React from "react"

import { TenantSummary } from "../../types/api"

/** Header tenant scope picker (or a read-only label when only one tenant applies). */
export const TenantSelector: React.FC<{
  tenants: TenantSummary[]
  value: string
  onChange: (tenant: string) => void
  switchable: boolean
  fixedLabel: string
}> = ({ tenants, value, onChange, switchable, fixedLabel }) => (
  <div className="flex items-center gap-2 h-9 min-w-0 text-xs bg-slate-950 px-2.5 sm:px-3 rounded-lg border border-slate-800">
    <span className="w-2 h-2 shrink-0 rounded-full bg-emerald-400" />
    {switchable ? (
      <>
        <label
          htmlFor="header-tenant-select"
          className="hidden sm:inline text-slate-400"
        >
          Tenant:
        </label>
        <select
          id="header-tenant-select"
          aria-label="Active Tenant"
          value={value}
          onChange={(e) => onChange(e.target.value)}
          className="min-w-0 max-w-[9rem] sm:max-w-none bg-transparent font-mono font-semibold text-slate-200 focus:outline-none cursor-pointer truncate"
        >
          <option value="" className="bg-slate-900 text-slate-200">
            All Tenants
          </option>
          {tenants.map((t) => (
            <option key={t.id} value={t.id} className="bg-slate-900">
              {t.id}
            </option>
          ))}
        </select>
      </>
    ) : (
      <>
        <span className="hidden sm:inline text-slate-400">Tenant:</span>
        <span className="font-mono font-semibold text-slate-200 truncate">
          {fixedLabel}
        </span>
      </>
    )}
  </div>
)
