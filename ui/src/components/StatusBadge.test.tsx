import { render, screen } from "@testing-library/react"
import { describe, expect, it } from "vitest"
import { StatusBadge } from "./StatusBadge"

describe("StatusBadge", () => {
  it.each([
    ["Synced", "Synced"],
    ["ready", "Ready"],
    ["healthy", "Healthy"],
    ["progressing", "Progressing"],
    ["Reconciling", "Reconciling"],
    ["Deleting", "Deleting"],
    ["Orphaned", "Orphaned"],
    ["pending", "Pending"],
    ["failed", "Failed"],
    ["Error", "Error"],
    ["degraded", "Degraded"],
    ["drift", "Drift Detected"],
    ["Mystery", "Mystery"],
    [undefined, "Unknown"],
  ])("maps %s to %s", (status, label) => {
    render(<StatusBadge status={status} size="xs" />)
    expect(screen.getByText(label)).toBeInTheDocument()
  })

  it("explains orphaned status", () => {
    render(<StatusBadge status="orphaned" />)
    expect(screen.getByText("Orphaned").parentElement).toHaveAttribute(
      "title",
      "Live in the cluster but no longer declared in Git"
    )
  })

  it("shows the status detail as tooltip", () => {
    render(
      <StatusBadge
        status="Error"
        detail="InstallFailed: admission webhook denied the request"
        collapsible
      />
    )
    expect(screen.getByText("Error").parentElement).toHaveAttribute(
      "title",
      "Error: InstallFailed: admission webhook denied the request"
    )
  })
})
