import type { RegistryWidgetsType } from "@rjsf/utils"

import {
  ShadcnBaseInputTemplate,
  ShadcnCheckboxWidget,
  ShadcnSelectWidget,
  ShadcnTextareaWidget,
} from "./widgets"
import {
  RjsfAddButton,
  RjsfCopyButton,
  RjsfMoveDownButton,
  RjsfMoveUpButton,
  RjsfRemoveButton,
  ShadcnDescriptionField,
  ShadcnErrorList,
  ShadcnFieldErrorTemplate,
  ShadcnFieldTemplate,
} from "./templates"

/** `@rjsf/core` widget registry rendering every input through Shadcn UI primitives. */
export const shadcnWidgets = {
  TextareaWidget: ShadcnTextareaWidget,
  SelectWidget: ShadcnSelectWidget,
  CheckboxWidget: ShadcnCheckboxWidget,
} as unknown as RegistryWidgetsType

/**
 * `@rjsf/core` template registry (field layout, errors, array buttons) using Shadcn UI.
 * `BaseInputTemplate` backs the text, password, email, url, number/updown and range widgets.
 */
export const shadcnTemplates = {
  BaseInputTemplate: ShadcnBaseInputTemplate,
  FieldTemplate: ShadcnFieldTemplate,
  FieldErrorTemplate: ShadcnFieldErrorTemplate,
  DescriptionFieldTemplate: ShadcnDescriptionField,
  ErrorListTemplate: ShadcnErrorList,
  ButtonTemplates: {
    AddButton: RjsfAddButton,
    RemoveButton: RjsfRemoveButton,
    MoveUpButton: RjsfMoveUpButton,
    MoveDownButton: RjsfMoveDownButton,
    CopyButton: RjsfCopyButton,
  },
}
