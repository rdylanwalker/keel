import React from "react"
import { FileCode2 } from "lucide-react"

export const ValuesYamlPreview: React.FC<{
  yaml: string
  fullscreen: boolean
}> = ({ yaml, fullscreen }) => (
  <div
    className={
      fullscreen ? "flex flex-col h-full min-h-0 space-y-2" : "space-y-2"
    }
  >
    <div className="flex flex-wrap items-center justify-between gap-1 shrink-0">
      <h4 className="text-xs font-semibold text-slate-300 flex items-center gap-1.5 uppercase tracking-wider">
        <FileCode2 className="h-4 w-4 text-emerald-400" />
        Live values.yaml Preview
      </h4>
      <span className="text-[10px] text-slate-500 font-mono">
        SOPS + age encrypted on secret fields before git commit
      </span>
    </div>
    <pre
      aria-label="values.yaml preview"
      className={`bg-slate-950 border border-slate-800 rounded-xl p-4 text-xs font-mono text-emerald-400 overflow-auto leading-relaxed ${
        fullscreen ? "flex-1 min-h-0" : "max-h-56"
      }`}
    >
      {yaml}
    </pre>
  </div>
)
