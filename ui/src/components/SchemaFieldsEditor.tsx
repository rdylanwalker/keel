import React from "react"
import { Trash2, Plus } from "lucide-react"
import type { SchemaFieldDraft, SchemaFieldType } from "../lib/schemaBuilder"

interface SchemaFieldsEditorProps {
  fields: SchemaFieldDraft[]
  onChange: (next: SchemaFieldDraft[]) => void
  pathPrefix?: string
}

export const SchemaFieldsEditor: React.FC<SchemaFieldsEditorProps> = ({
  fields,
  onChange,
  pathPrefix = "",
}) => {
  if (fields.length === 0) return null

  const updateFieldAt = (
    idx: number,
    updater: (field: SchemaFieldDraft) => SchemaFieldDraft
  ) => {
    onChange(fields.map((f, i) => (i === idx ? updater(f) : f)))
  }

  const removeFieldAt = (idx: number) => {
    onChange(fields.filter((_, i) => i !== idx))
  }

  return (
    <div className="space-y-1.5">
      {fields.map((field, idx) => {
        const pathLabel = pathPrefix ? `${pathPrefix}.${idx + 1}` : `${idx + 1}`
        const fieldLabel = `Field ${pathLabel}`
        const isObject = field.type === "object"
        const isArray = field.type === "array"
        const itemsType = field.itemsType ?? "string"
        const hasNestedObjectFields =
          isObject || (isArray && itemsType === "object")

        return (
          <div
            key={idx}
            className="bg-slate-950/70 border border-slate-800/80 rounded-lg p-1.5"
          >
            <div className="grid grid-cols-12 gap-1.5 items-center">
              <input
                type="text"
                aria-label={`${fieldLabel} name`}
                placeholder="fieldName"
                value={field.name}
                onChange={(e) =>
                  updateFieldAt(idx, (f) => ({ ...f, name: e.target.value }))
                }
                className="col-span-3 px-2 py-1 bg-slate-900 border border-slate-800 rounded text-[11px] text-slate-100 font-mono"
              />
              <input
                type="text"
                aria-label={`${fieldLabel} title`}
                placeholder="Label"
                value={field.title}
                onChange={(e) =>
                  updateFieldAt(idx, (f) => ({ ...f, title: e.target.value }))
                }
                className="col-span-3 px-2 py-1 bg-slate-900 border border-slate-800 rounded text-[11px] text-slate-100"
              />
              <select
                aria-label={`${fieldLabel} type`}
                value={field.type}
                onChange={(e) => {
                  const nextType = e.target.value as SchemaFieldType
                  updateFieldAt(idx, (f) => {
                    if (nextType === "object") {
                      return {
                        ...f,
                        type: "object",
                        defaultValue: "",
                        secret: false,
                        children: f.children ?? [],
                      }
                    }
                    if (nextType === "array") {
                      return {
                        ...f,
                        type: "array",
                        secret: false,
                        itemsType: f.itemsType ?? "string",
                        children: f.children,
                      }
                    }
                    return {
                      ...f,
                      type: nextType,
                    }
                  })
                }}
                className="col-span-2 px-1.5 py-1 bg-slate-900 border border-slate-800 rounded text-[11px] text-slate-100"
              >
                <option value="string">string</option>
                <option value="integer">integer</option>
                <option value="number">number</option>
                <option value="boolean">boolean</option>
                <option value="object">object</option>
                <option value="array">array</option>
              </select>

              {isObject ? (
                <div className="col-span-2 px-2 py-1 text-[10px] text-slate-400 truncate">
                  {(field.children ?? []).length}{" "}
                  {(field.children ?? []).length === 1 ? "nested" : "nested"}
                </div>
              ) : isArray ? (
                <select
                  aria-label={`${fieldLabel} item type`}
                  value={itemsType}
                  onChange={(e) => {
                    const nextItemsType = e.target.value as SchemaFieldType
                    updateFieldAt(idx, (f) => ({
                      ...f,
                      itemsType: nextItemsType,
                      children:
                        nextItemsType === "object"
                          ? (f.children ?? [])
                          : f.children,
                    }))
                  }}
                  className="col-span-2 px-1.5 py-1 bg-slate-900 border border-slate-800 rounded text-[11px] text-indigo-300"
                >
                  <option value="string">string[]</option>
                  <option value="integer">integer[]</option>
                  <option value="number">number[]</option>
                  <option value="boolean">boolean[]</option>
                  <option value="object">object[]</option>
                </select>
              ) : (
                <input
                  type="text"
                  aria-label={`${fieldLabel} default`}
                  placeholder="default"
                  value={field.defaultValue}
                  onChange={(e) =>
                    updateFieldAt(idx, (f) => ({
                      ...f,
                      defaultValue: e.target.value,
                    }))
                  }
                  className="col-span-2 px-2 py-1 bg-slate-900 border border-slate-800 rounded text-[11px] text-slate-100"
                />
              )}

              <div className="col-span-2 flex items-center justify-end gap-2">
                <label
                  className="flex items-center gap-0.5 text-[10px] text-slate-400 cursor-pointer"
                  title="Required"
                >
                  <input
                    type="checkbox"
                    checked={field.required}
                    onChange={(e) =>
                      updateFieldAt(idx, (f) => ({
                        ...f,
                        required: e.target.checked,
                      }))
                    }
                  />
                  <span>Req</span>
                </label>
                {!isObject && !isArray && (
                  <label
                    className="flex items-center gap-0.5 text-[10px] text-amber-400 cursor-pointer"
                    title="Secret (SOPS + age encrypted, bound to envRefs)"
                  >
                    <input
                      type="checkbox"
                      checked={field.secret}
                      onChange={(e) =>
                        updateFieldAt(idx, (f) => ({
                          ...f,
                          secret: e.target.checked,
                        }))
                      }
                    />
                    <span>Sec</span>
                  </label>
                )}
                <button
                  type="button"
                  aria-label={`Remove field ${pathLabel}`}
                  onClick={() => removeFieldAt(idx)}
                  className="text-slate-500 hover:text-red-400 cursor-pointer"
                >
                  <Trash2 className="h-3.5 w-3.5" />
                </button>
              </div>
            </div>

            {isArray && itemsType !== "object" && (
              <div className="ml-3 pl-2.5 border-l-2 border-slate-800 mt-1.5 flex items-center gap-2">
                <span className="text-[10px] text-slate-400 shrink-0">
                  Default items
                </span>
                <input
                  type="text"
                  aria-label={`${fieldLabel} default`}
                  placeholder="item1, item2 or JSON array"
                  value={field.defaultValue}
                  onChange={(e) =>
                    updateFieldAt(idx, (f) => ({
                      ...f,
                      defaultValue: e.target.value,
                    }))
                  }
                  className="flex-1 px-2 py-1 bg-slate-900 border border-slate-800 rounded text-[11px] text-slate-100"
                />
              </div>
            )}

            {hasNestedObjectFields && (
              <div className="ml-3 pl-2.5 border-l-2 border-indigo-500/30 mt-2 space-y-1.5">
                <div className="flex items-center justify-between">
                  <span className="text-[10px] font-medium text-slate-400">
                    {isObject ? "Nested Fields" : "Item Object Fields"}
                  </span>
                  <button
                    type="button"
                    aria-label={`Add nested field to ${pathLabel}`}
                    onClick={() =>
                      updateFieldAt(idx, (f) => ({
                        ...f,
                        children: [
                          ...(f.children ?? []),
                          {
                            name: "",
                            title: "",
                            type: "string",
                            defaultValue: "",
                            required: false,
                            secret: false,
                          },
                        ],
                      }))
                    }
                    className="inline-flex items-center gap-1 text-[10px] font-medium text-indigo-400 hover:text-indigo-300 cursor-pointer"
                  >
                    <Plus className="h-3 w-3" />
                    <span>Add Nested Field</span>
                  </button>
                </div>
                <SchemaFieldsEditor
                  fields={field.children ?? []}
                  onChange={(nextChildren) =>
                    updateFieldAt(idx, (f) => ({
                      ...f,
                      children: nextChildren,
                    }))
                  }
                  pathPrefix={pathLabel}
                />
              </div>
            )}
          </div>
        )
      })}
    </div>
  )
}
