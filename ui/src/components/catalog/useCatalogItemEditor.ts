import React, { useState } from "react"
import type { RJSFSchema } from "@rjsf/utils"

import { createCatalogApp, updateCatalogItem } from "@/lib/api"
import { DEFAULT_STARTER_MANIFEST } from "@/lib/starterManifest"
import { errorMessage } from "@/lib/errors"
import {
  SchemaFieldDraft,
  extractManifestVariables,
  fieldsToSchemaJson,
  humanizeFieldName,
  schemaJsonToFields,
} from "@/lib/schemaBuilder"
import { CatalogItem, CreateCatalogItemPayload, Proposal } from "@/types/api"
import {
  buildCatalogPayload,
  catalogItemToForm,
  emptyCatalogForm,
  isCatalogFormSubmittable,
  toUpdatePayload,
} from "./catalogForm"

/** State + actions for the add/edit catalog item dialog. */
export function useCatalogItemEditor(
  onProposal: (proposal: Proposal | null) => void
) {
  const [isOpen, setIsOpen] = useState(false)
  const [editingCatalogItem, setEditingCatalogItem] =
    useState<CatalogItem | null>(null)
  const [appForm, setAppForm] =
    useState<CreateCatalogItemPayload>(emptyCatalogForm)
  const [schemaJsonText, setSchemaJsonText] = useState("")
  const [schemaFields, setSchemaFields] = useState<SchemaFieldDraft[]>([])
  const [showSchemaEditor, setShowSchemaEditor] = useState(false)
  const [appLoading, setAppLoading] = useState(false)
  const [appError, setAppError] = useState<string | null>(null)

  const openCreate = (defaults?: Partial<CreateCatalogItemPayload>) => {
    setEditingCatalogItem(null)
    setAppError(null)
    setSchemaJsonText("")
    setSchemaFields([])
    setShowSchemaEditor(false)
    setAppForm({ ...emptyCatalogForm(), ...defaults })
    setIsOpen(true)
  }

  const openEdit = (item: CatalogItem) => {
    setEditingCatalogItem(item)
    setAppError(null)
    const json = JSON.stringify(
      item.schema ?? { type: "object", properties: {} },
      null,
      2
    )
    setSchemaJsonText(json)
    setSchemaFields(schemaJsonToFields(json) ?? [])
    setShowSchemaEditor(false)
    setAppForm(catalogItemToForm(item))
    setIsOpen(true)
  }

  const close = () => {
    setIsOpen(false)
    setEditingCatalogItem(null)
    setAppError(null)
  }

  const submit = async (e: React.FormEvent) => {
    e.preventDefault()
    if (!isCatalogFormSubmittable(appForm)) return
    let parsedSchema: RJSFSchema | undefined
    if (showSchemaEditor && schemaJsonText.trim()) {
      try {
        parsedSchema = JSON.parse(schemaJsonText) as RJSFSchema
      } catch (err) {
        setAppError(
          `Invalid JSON Schema: ${err instanceof Error ? err.message : "parse error"}`
        )
        return
      }
    }
    setAppLoading(true)
    setAppError(null)
    onProposal(null)
    try {
      const payload = buildCatalogPayload(
        appForm,
        editingCatalogItem,
        parsedSchema
      )
      const proposal = editingCatalogItem
        ? await updateCatalogItem(
            editingCatalogItem.type,
            editingCatalogItem.id,
            toUpdatePayload(payload)
          )
        : await createCatalogApp(payload)
      onProposal(proposal)
      setIsOpen(false)
      setEditingCatalogItem(null)
      setSchemaJsonText("")
      setShowSchemaEditor(false)
      setAppForm(emptyCatalogForm())
    } catch (err) {
      setAppError(
        errorMessage(
          err,
          editingCatalogItem
            ? "Failed to update catalog item proposal"
            : "Failed to create catalog app proposal"
        )
      )
    } finally {
      setAppLoading(false)
    }
  }

  const extractManifestVars = () => {
    const yaml = appForm.manifestsYaml || DEFAULT_STARTER_MANIFEST
    const existing = new Set(schemaFields.map((f) => f.name))
    const added: SchemaFieldDraft[] = []
    for (const name of extractManifestVariables(yaml)) {
      if (existing.has(name)) continue
      added.push({
        name,
        title: humanizeFieldName(name),
        type: "string",
        defaultValue: "",
        required: true,
        secret: /secret|token|password|key/i.test(name),
      })
      existing.add(name)
    }
    const updated = [...schemaFields, ...added]
    setSchemaFields(updated)
    setSchemaJsonText(fieldsToSchemaJson(updated, schemaJsonText))
    setShowSchemaEditor(true)
  }

  return {
    modalProps: {
      isOpen,
      editingCatalogItem,
      appForm,
      setAppForm,
      schemaJsonText,
      setSchemaJsonText,
      schemaFields,
      setSchemaFields,
      showSchemaEditor,
      setShowSchemaEditor,
      appLoading,
      appError,
      onSubmit: submit,
      onExtractManifestVars: extractManifestVars,
      onClose: close,
      onCancel: close,
    },
    openCreate,
    openEdit,
  }
}
