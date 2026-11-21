import React from "react"
import { Boxes, GitPullRequest, Layers, Server } from "lucide-react"

import { ActiveTab } from "../../lib/routes"
import { cn } from "../../lib/utils"

export const NAV_ITEMS: ReadonlyArray<{
  tab: ActiveTab
  label: string
  icon: React.ComponentType<{ className?: string }>
}> = [
  { tab: "topology", label: "Topology Map", icon: Boxes },
  { tab: "catalog", label: "Catalog & Form Wizard", icon: Layers },
  { tab: "preview", label: "PR & Preview Inspector", icon: GitPullRequest },
  { tab: "providers", label: "Control Plane & Providers", icon: Server },
]

/** Primary navigation list (used in the desktop sidebar and the mobile drawer). */
export const NavMenu: React.FC<{
  activeTab: ActiveTab
  onNavigate: (tab: ActiveTab) => void
}> = ({ activeTab, onNavigate }) => (
  <nav aria-label="Primary" className="space-y-1">
    {NAV_ITEMS.map(({ tab, label, icon: Icon }) => (
      <button
        key={tab}
        type="button"
        onClick={() => onNavigate(tab)}
        aria-current={activeTab === tab ? "page" : undefined}
        className={cn(
          "w-full flex items-center gap-3 px-3.5 py-2.5 rounded-xl text-xs font-semibold transition-all cursor-pointer focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-indigo-400",
          activeTab === tab
            ? "bg-indigo-600 text-white shadow-lg shadow-indigo-600/20"
            : "text-slate-400 hover:text-slate-200 hover:bg-slate-800/60"
        )}
      >
        <Icon className="h-4 w-4" />
        <span>{label}</span>
      </button>
    ))}
  </nav>
)
