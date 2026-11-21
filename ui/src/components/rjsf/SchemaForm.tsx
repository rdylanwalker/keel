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
  children?: React.ReactNode
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
