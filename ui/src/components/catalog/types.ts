import type React from "react"

import { CreateCatalogItemPayload } from "@/types/api"

export type SetAppForm = React.Dispatch<
  React.SetStateAction<CreateCatalogItemPayload>
>

export interface CatalogFormSectionProps {
  appForm: CreateCatalogItemPayload
  setAppForm: SetAppForm
}
