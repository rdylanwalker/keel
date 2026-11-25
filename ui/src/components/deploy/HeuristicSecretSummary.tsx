import React from "react"
import { AlertTriangle } from "lucide-react"

import { Checkbox } from "@/components/ui/checkbox"
import { Label } from "@/components/ui/label"
import type { HeuristicSecretField } from "@/lib/secretFields"

/**
 * Name-heuristic secret matches the form cannot annotate inline (keys added under an open
 * `additionalProperties` object): each is stored encrypted with SOPS unless marked not secret.
 */
export const HeuristicSecretSummary: React.FC<{
  fields: HeuristicSecretField[]
  onToggle: (path: string, notSecret: boolean) => void
}> = ({ fields, onToggle }) => (
  <div
    role="note"
    aria-label="Values that look like secrets"
    data-testid="heuristic-secret-summary"
    className="rounded-xl border border-amber-500/30 bg-amber-950/30 p-3 text-xs text-amber-200 space-y-2"
  >
    <p className="flex items-start gap-2">
      <AlertTriangle className="h-4 w-4 shrink-0 text-amber-400" />
      <span>
        These values look like secrets and are stored encrypted with SOPS. Tick{" "}
        <strong>Not a secret</strong> to keep one in plaintext Git.
      </span>
    </p>
    <ul className="space-y-1.5 pl-6">
      {fields.map((field, i) => {
        const id = `heuristic-secret-summary-${i}`
        return (
          <li
            key={field.path}
            data-path={field.path}
            className="flex flex-wrap items-center justify-between gap-2"
          >
            <code className="font-mono text-slate-100 break-all">
              {field.path}
            </code>
            <span className="flex items-center gap-1.5">
              <Checkbox
                id={id}
                checked={field.notSecret}
                onCheckedChange={(checked) =>
                  onToggle(field.path, checked === true)
                }
                aria-label={`Not a secret: ${field.path}`}
              />
              <Label htmlFor={id} className="text-[11px] cursor-pointer">
                Not a secret
              </Label>
            </span>
          </li>
        )
      })}
    </ul>
  </div>
)
