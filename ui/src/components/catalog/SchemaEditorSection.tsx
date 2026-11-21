import React from "react"

import { Textarea } from "@/components/ui/textarea"
import {
  PROVIDER_CREDENTIAL_PRESETS,
  SchemaFieldDraft,
  deriveFieldsFromPulumiYaml,
  fieldsToSchemaJson,
  schemaJsonToFields,
} from "@/lib/schemaBuilder"
import { SchemaFieldsEditor } from "../SchemaFieldsEditor"
import { Field } from "./Field"
import { CatalogFormSectionProps } from "./types"

export interface SchemaEditorSectionProps extends CatalogFormSectionProps {
  schemaJsonText: string
  setSchemaJsonText: React.Dispatch<React.SetStateAction<string>>
  schemaFields: SchemaFieldDraft[]
  setSchemaFields: React.Dispatch<React.SetStateAction<SchemaFieldDraft[]>>
  showSchemaEditor: boolean
  setShowSchemaEditor: React.Dispatch<React.SetStateAction<boolean>>
  onExtractManifestVars: () => void
}

const linkBtn =
  "text-[11px] font-medium transition-colors cursor-pointer focus-visible:outline-none focus-visible:underline"

/** Visual form-field builder + raw JSON Schema editor for a catalog item. */
export const SchemaEditorSection: React.FC<SchemaEditorSectionProps> = ({
  appForm,
  schemaJsonText,
  setSchemaJsonText,
  schemaFields,
  setSchemaFields,
  showSchemaEditor,
  setShowSchemaEditor,
  onExtractManifestVars,
}) => {
  const applyFields = (next: SchemaFieldDraft[], reveal = false) => {
    setSchemaFields(next)
    setSchemaJsonText(fieldsToSchemaJson(next, schemaJsonText))
    if (reveal) setShowSchemaEditor(true)
  }
  const presets = PROVIDER_CREDENTIAL_PRESETS[appForm.provider || ""]
  return (
    <div className="pt-1 border-t border-slate-800/80">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <button
          type="button"
          onClick={() => {
            if (!showSchemaEditor && !schemaJsonText) {
              setSchemaJsonText(
                JSON.stringify({ type: "object", properties: {} }, null, 2)
              )
              setSchemaFields([])
            }
            setShowSchemaEditor((prev) => !prev)
          }}
          className={`${linkBtn} text-indigo-400 hover:text-indigo-300`}
        >
          {showSchemaEditor
            ? "Hide JSON Schema Editor"
            : "Edit JSON Schema (Optional)"}
        </button>
        {appForm.kind === "infra" && (
          <div className="flex flex-wrap items-center gap-2">
            {appForm.pulumiYaml?.trim() && (
              <button
                type="button"
                onClick={() => {
                  const derived = deriveFieldsFromPulumiYaml(
                    appForm.pulumiYaml || ""
                  )
                  const existingSecrets = schemaFields.filter(
                    (f) => f.secret && !derived.some((d) => d.name === f.name)
                  )
                  applyFields([...derived, ...existingSecrets], true)
                }}
                className={`${linkBtn} text-emerald-400 hover:text-emerald-300`}
              >
                Derive from Pulumi.yaml
              </button>
            )}
            {presets && (
              <button
                type="button"
                onClick={() => {
                  const existing = new Set(schemaFields.map((f) => f.name))
                  applyFields(
                    [
                      ...schemaFields,
                      ...presets.filter((p) => !existing.has(p.name)),
                    ],
                    true
                  )
                }}
                className={`${linkBtn} text-amber-400 hover:text-amber-300`}
              >
                + Add {appForm.provider} Credentials
              </button>
            )}
          </div>
        )}
        {appForm.kind === "app" && appForm.runtime === "manifest" && (
          <button
            type="button"
            onClick={onExtractManifestVars}
            className={`${linkBtn} text-emerald-400 hover:text-emerald-300`}
          >
            Auto-Extract `${`var`}` Fields
          </button>
        )}
      </div>
      {showSchemaEditor && (
        <div className="mt-2.5 space-y-3">
          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <span className="text-xs text-slate-300 font-medium">
                Form Fields
              </span>
              <button
                type="button"
                onClick={() =>
                  applyFields([
                    ...schemaFields,
                    {
                      name: "",
                      title: "",
                      type: "string",
                      defaultValue: "",
                      required: false,
                      secret: false,
                    },
                  ])
                }
                className={`${linkBtn} text-indigo-400 hover:text-indigo-300`}
              >
                + Add Field
              </button>
            </div>
            {schemaFields.length > 0 && (
              <div className="max-h-80 overflow-y-auto pr-1">
                <SchemaFieldsEditor
                  fields={schemaFields}
                  onChange={(next) => applyFields(next)}
                />
              </div>
            )}
          </div>
          <Field id="catalog-schema-json" label="JSON Schema">
            <Textarea
              id="catalog-schema-json"
              rows={6}
              spellCheck={false}
              value={schemaJsonText}
              onChange={(e) => {
                const text = e.target.value
                setSchemaJsonText(text)
                const parsed = schemaJsonToFields(text)
                if (parsed) setSchemaFields(parsed)
              }}
              className="text-[11px]"
            />
          </Field>
        </div>
      )}
    </div>
  )
}
