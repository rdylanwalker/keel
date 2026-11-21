import React, { createContext, useContext } from "react"

import { ANONYMOUS_AUTHZ, Authz } from "./authz"

const AuthzContext = createContext<Authz>(ANONYMOUS_AUTHZ)

export const AuthzProvider: React.FC<{
  value: Authz
  children: React.ReactNode
}> = ({ value, children }) => (
  <AuthzContext.Provider value={value}>{children}</AuthzContext.Provider>
)

/** Current caller's role-derived permissions (see {@link Authz}). */
// eslint-disable-next-line react-refresh/only-export-components
export function useAuthz(): Authz {
  return useContext(AuthzContext)
}
