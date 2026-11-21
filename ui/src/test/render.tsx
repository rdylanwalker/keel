import React from "react"
import { render, RenderOptions } from "@testing-library/react"

import { ADMIN_AUTHZ, Authz } from "../lib/authz"
import { AuthzProvider } from "../lib/authz-context"

/** Renders `ui` inside an {@link AuthzProvider} (cluster admin by default). */
export function renderWithAuthz(
  ui: React.ReactElement,
  authz: Authz = ADMIN_AUTHZ,
  options?: RenderOptions
) {
  const Wrapper: React.FC<{ children: React.ReactNode }> = ({ children }) => (
    <AuthzProvider value={authz}>{children}</AuthzProvider>
  )
  return render(ui, { wrapper: Wrapper, ...options })
}
