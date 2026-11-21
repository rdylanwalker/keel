import React from "react"
import { Plus, Trash2 } from "lucide-react"

import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Input } from "@/components/ui/input"
import { NativeSelect } from "@/components/ui/native-select"
import { CatalogBundleItem, CatalogItem } from "@/types/api"
import { Field } from "./Field"
import { CatalogFormSectionProps } from "./types"
import { isInfraItem } from "./catalogForm"

const BundleStepRow: React.FC<{
  idx: number
  item: CatalogBundleItem
  items: CatalogBundleItem[]
  catalog: CatalogItem[]
  onUpdate: (patch: Partial<CatalogBundleItem>) => void
  onRemove: () => void
}> = ({ idx, item, items, catalog, onUpdate, onRemove }) => {
  const availableRefs = catalog.filter((c) =>
    item.kind === "infra" ? isInfraItem(c) : c.type === "app"
  )
  const selectedApp =
    item.kind === "app"
      ? catalog.find((c) => c.type === "app" && c.id === item.catalogRef)
      : undefined
  const priorItems = items.filter((_, i) => i !== idx)
  const infraDeps = priorItems.filter(
    (p) => p.kind === "infra" && (item.dependsOn ?? []).includes(p.id)
  )
  const step = `Step ${idx + 1}`
  return (
    <div className="p-3 bg-slate-950 border border-slate-800 rounded-xl space-y-2">
      <div className="grid grid-cols-12 gap-2 items-center">
        <Input
          aria-label={`${step} ID`}
          placeholder="step id"
          value={item.id}
          onChange={(e) => onUpdate({ id: e.target.value })}
          className="col-span-12 sm:col-span-3 px-2 py-1.5 text-[11px]"
        />
        <div className="col-span-5 sm:col-span-3">
          <NativeSelect
            aria-label={`${step} Kind`}
            value={item.kind}
            onChange={(e) => {
              const nextKind = e.target.value as "infra" | "app"
              const firstRef = catalog.find((c) =>
                nextKind === "infra" ? isInfraItem(c) : c.type === "app"
              )?.id
              onUpdate({
                kind: nextKind,
                catalogRef: firstRef ?? "",
                targetClusterFrom: undefined,
                targetNamespace: undefined,
                canary: undefined,
              })
            }}
            className="px-2 py-1.5 text-[11px]"
          >
            <option value="infra">infra</option>
            <option value="app">app</option>
          </NativeSelect>
        </div>
        <div className="col-span-6 sm:col-span-5">
          <NativeSelect
            aria-label={`${step} Catalog Ref`}
            value={item.catalogRef}
            onChange={(e) => {
              const nextRef = e.target.value
              const nextApp = catalog.find(
                (c) => c.type === "app" && c.id === nextRef
              )
              onUpdate({
                catalogRef: nextRef,
                canary: nextApp?.flaggerSupported ? item.canary : undefined,
              })
            }}
            className="px-2 py-1.5 text-[11px]"
          >
            <option value="">Select item...</option>
            {availableRefs.map((c) => (
              <option key={c.id} value={c.id}>
                {c.id} ({c.name})
              </option>
            ))}
          </NativeSelect>
        </div>
        <Button
          variant="ghost"
          size="icon-sm"
          aria-label={`Remove step ${idx + 1}`}
          onClick={onRemove}
          className="col-span-1 hover:text-red-400"
        >
          <Trash2 className="h-3.5 w-3.5" />
        </Button>
      </div>

      {priorItems.length > 0 && (
        <div className="flex flex-wrap items-center gap-2 text-[11px] text-slate-400">
          <span>Depends on:</span>
          {priorItems.map((p) => {
            const checked = (item.dependsOn ?? []).includes(p.id)
            const cbId = `bundle-step-${idx}-dep-${p.id}`
            return (
              <span key={p.id} className="inline-flex items-center gap-1">
                <Checkbox
                  id={cbId}
                  checked={checked}
                  onCheckedChange={(v) => {
                    const current = item.dependsOn ?? []
                    const next =
                      v === true
                        ? [...current, p.id]
                        : current.filter((d) => d !== p.id)
                    onUpdate({
                      dependsOn: next,
                      targetClusterFrom:
                        item.targetClusterFrom &&
                        !next.includes(item.targetClusterFrom)
                          ? undefined
                          : item.targetClusterFrom,
                    })
                  }}
                />
                <label
                  htmlFor={cbId}
                  className="cursor-pointer font-mono text-slate-300"
                >
                  {p.id}
                </label>
              </span>
            )
          })}
        </div>
      )}

      {item.kind === "app" && (
        <div className="space-y-2">
          <div className="grid grid-cols-1 sm:grid-cols-2 gap-2">
            <NativeSelect
              aria-label={`${step} Target Cluster From`}
              value={item.targetClusterFrom || ""}
              onChange={(e) =>
                onUpdate({ targetClusterFrom: e.target.value || undefined })
              }
              className="px-2 py-1 text-[11px]"
            >
              <option value="">Local / Tenant Cluster</option>
              {infraDeps.map((inf) => (
                <option key={inf.id} value={inf.id}>
                  From infra: {inf.id}
                </option>
              ))}
            </NativeSelect>
            <Input
              aria-label={`${step} Target Namespace`}
              placeholder="Target namespace (optional)"
              value={item.targetNamespace || ""}
              onChange={(e) =>
                onUpdate({ targetNamespace: e.target.value || undefined })
              }
              className="px-2 py-1 text-[11px]"
            />
          </div>
          {selectedApp?.flaggerSupported && (
            <span className="inline-flex items-center gap-2 text-[11px] text-slate-300">
              <Checkbox
                id={`bundle-step-${idx}-canary`}
                aria-label={`${step} Enable Flagger Canary`}
                checked={!!item.canary}
                onCheckedChange={(v) =>
                  onUpdate({ canary: v === true || undefined })
                }
              />
              <label
                htmlFor={`bundle-step-${idx}-canary`}
                className="cursor-pointer select-none"
              >
                Enable Flagger Canary
              </label>
            </span>
          )}
        </div>
      )}
    </div>
  )
}

/** Bundle category/description plus the ordered step list with dependency wiring. */
export const BundleItemsEditor: React.FC<
  CatalogFormSectionProps & { catalog: CatalogItem[] }
> = ({ appForm, setAppForm, catalog }) => {
  const items = appForm.items ?? []
  const updateItemAt = (idx: number, patch: Partial<CatalogBundleItem>) =>
    setAppForm((prev) => ({
      ...prev,
      items: (prev.items ?? []).map((it, i) =>
        i === idx ? { ...it, ...patch } : it
      ),
    }))
  return (
    <div className="space-y-3">
      <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
        <Field id="bundle-category" label="Category" required>
          <Input
            id="bundle-category"
            required
            value={appForm.category}
            onChange={(e) =>
              setAppForm((prev) => ({ ...prev, category: e.target.value }))
            }
            className="font-sans"
          />
        </Field>
        <Field id="bundle-description" label="Description">
          <Input
            id="bundle-description"
            placeholder="Optional summary..."
            value={appForm.description || ""}
            onChange={(e) =>
              setAppForm((prev) => ({ ...prev, description: e.target.value }))
            }
            className="font-sans"
          />
        </Field>
      </div>

      <div className="space-y-2">
        <div className="flex items-center justify-between">
          <span className="text-xs font-medium text-slate-300">
            Bundle Items &amp; Dependency Chain
          </span>
          <Button
            variant="secondary"
            size="sm"
            onClick={() =>
              setAppForm((prev) => {
                const existing = prev.items ?? []
                return {
                  ...prev,
                  items: [
                    ...existing,
                    {
                      id: `step-${existing.length + 1}`,
                      kind: "app",
                      catalogRef:
                        catalog.find((c) => c.type === "app")?.id ?? "",
                      dependsOn: existing[0] ? [existing[0].id] : [],
                    },
                  ],
                }
              })
            }
          >
            <Plus className="h-3 w-3" />
            <span>Add Step</span>
          </Button>
        </div>
        <div className="space-y-2.5 max-h-72 overflow-y-auto pr-1">
          {items.map((item, idx) => (
            <BundleStepRow
              key={idx}
              idx={idx}
              item={item}
              items={items}
              catalog={catalog}
              onUpdate={(patch) => updateItemAt(idx, patch)}
              onRemove={() =>
                setAppForm((prev) => ({
                  ...prev,
                  items: (prev.items ?? []).filter((_, i) => i !== idx),
                }))
              }
            />
          ))}
        </div>
      </div>
    </div>
  )
}
