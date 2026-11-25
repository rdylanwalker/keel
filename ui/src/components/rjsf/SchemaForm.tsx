import React from "react"
import Form from "@rjsf/core"
import type { CustomValidator, ErrorSchema, RJSFSchema } from "@rjsf/utils"

import { transformErrors } from "@/lib/formErrors"
import { shadcnTemplates, shadcnWidgets } from "./registry"
import validator from "./validator"

type Values = Record<string, unknown>

export interface SchemaFormProps {
  schema: RJSFSchema
  uiSchema?: Record<string, unknown>
  formData: Values
  onChange: (data: Values) => void
  onSubmit: (data: Values) => void
  customValidate?: CustomValidator<Values, RJSFSchema>
  extraErrors?: ErrorSchema<Values>
  /** Passed to templates (e.g. {@link SchemaFormContext.onToggleNotSecret}). */
  formContext?: SchemaFormContext
  children?: React.ReactNode
}

/** `formContext` understood by the Shadcn templates. */
export interface SchemaFormContext {
  /** Marks a name-heuristic secret field (dot path) as not secret, or secret again. */
  onToggleNotSecret?: (path: string, notSecret: boolean) => void
  [key: string]: unknown
}

/**
 * `@rjsf/core` form wired to the Shadcn UI widget/template registry, eval-free (CSP-safe)
 * JSON Schema validation and
 * plain-language error messages. Used for every catalog-schema-driven form.
 */
export const SchemaForm: React.FC<SchemaFormProps> = ({
  schema,
  uiSchema,
  formData,
  onChange,
  onSubmit,
  customValidate,
  extraErrors,
  formContext,
  children,
}) => (
  <div className="rjsf-container">
    <Form<Values, RJSFSchema>
      schema={schema}
      uiSchema={uiSchema}
      widgets={shadcnWidgets}
      templates={shadcnTemplates}
      validator={validator}
      formData={formData}
      transformErrors={transformErrors}
      customValidate={customValidate}
      extraErrors={extraErrors}
      formContext={formContext}
      showErrorList="top"
      focusOnFirstError
      noHtml5Validate
      onChange={(e) => e.formData && onChange(e.formData)}
      onSubmit={(e) => onSubmit(e.formData ?? formData)}
    >
      {children}
    </Form>
  </div>
)
