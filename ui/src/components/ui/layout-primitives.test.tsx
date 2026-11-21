import { fireEvent, render, screen } from "@testing-library/react"
import { describe, expect, it, vi } from "vitest"

import { PageHeader } from "./page-header"
import { TruncatedText } from "./truncated-text"
import {
  Dialog,
  DialogContent,
  DialogFullscreenToggle,
  DialogHeader,
  DialogTitle,
} from "./dialog"

describe("TruncatedText", () => {
  it("renders single-line text with the full value as a tooltip", () => {
    render(<TruncatedText text="e2e-lab-podinfo-canary" className="max-w-20" />)
    const el = screen.getByText("e2e-lab-podinfo-canary")
    expect(el).toHaveAttribute("title", "e2e-lab-podinfo-canary")
    expect(el.className).toContain("whitespace-nowrap")
    expect(el.className).toContain("text-ellipsis")
    expect(el.className).toContain("max-w-20")
    expect(el.className).not.toMatch(/break-all|break-words/)
  })

  it("supports a custom or suppressed tooltip", () => {
    const { rerender } = render(<TruncatedText text="abc" title="full abc" />)
    expect(screen.getByText("abc")).toHaveAttribute("title", "full abc")
    rerender(<TruncatedText text="abc" title="" />)
    expect(screen.getByText("abc")).not.toHaveAttribute("title")
  })
})

describe("PageHeader", () => {
  it("renders title, description, truncating meta and grouped actions", () => {
    render(
      <PageHeader
        icon={<svg data-testid="icon" />}
        title="PR #42: Deploy"
        description="Review proposed changes"
        meta={[
          { label: "Branch", value: "keel/tenant-acme/db-pg-42" },
          { label: "Author", value: "dylan@acme.internal" },
        ]}
        actions={
          <>
            <button type="button">Reject PR</button>
            <button type="button">Merge PR</button>
          </>
        }
      />
    )
    const heading = screen.getByRole("heading", { level: 2 })
    expect(heading).toHaveTextContent("PR #42: Deploy")
    expect(screen.getByText("Review proposed changes")).toBeInTheDocument()
    const branch = screen.getByText("keel/tenant-acme/db-pg-42")
    expect(branch).toHaveAttribute("title", "keel/tenant-acme/db-pg-42")
    expect(branch.className).toContain("whitespace-nowrap")
    expect(branch.closest("li")).toHaveTextContent(
      "Branch: keel/tenant-acme/db-pg-42"
    )
    const actions = document.querySelector('[data-slot="page-header-actions"]')
    expect(actions).toContainElement(
      screen.getByRole("button", { name: "Merge PR" })
    )
    expect(actions).toContainElement(
      screen.getByRole("button", { name: "Reject PR" })
    )
  })

  it("uses an h3 without card chrome for the section variant", () => {
    render(<PageHeader variant="section" title="Releases" inlineAt="xl" />)
    expect(screen.getByRole("heading", { level: 3 })).toHaveTextContent(
      "Releases"
    )
    const root = document.querySelector('[data-slot="page-header"]')
    expect(root?.className).not.toContain("rounded-2xl")
    expect(
      document.querySelector('[data-slot="page-header-actions"]')
    ).toBeNull()
  })
})

describe("Dialog header controls", () => {
  it("groups the fullscreen toggle with the close button and reserves header room", () => {
    const onToggle = vi.fn()
    render(
      <Dialog open>
        <DialogContent
          closeLabel="Close modal"
          controls={
            <DialogFullscreenToggle fullscreen={false} onToggle={onToggle} />
          }
        >
          <DialogHeader>
            <DialogTitle>Add Catalog Item</DialogTitle>
          </DialogHeader>
        </DialogContent>
      </Dialog>
    )
    const group = document.querySelector('[data-slot="dialog-controls"]')
    const toggle = screen.getByRole("button", { name: "Enter full screen" })
    const close = screen.getByRole("button", { name: "Close modal" })
    expect(group).toContainElement(toggle)
    expect(group).toContainElement(close)
    expect(
      document.querySelector('[data-slot="dialog-content"]')
    ).toHaveAttribute("data-controls", "2")
    fireEvent.click(toggle)
    expect(onToggle).toHaveBeenCalledTimes(1)
  })

  it("labels the toggle for exiting fullscreen", () => {
    render(
      <Dialog open>
        <DialogContent
          hideClose
          controls={<DialogFullscreenToggle fullscreen onToggle={() => {}} />}
        >
          <DialogTitle>T</DialogTitle>
        </DialogContent>
      </Dialog>
    )
    expect(
      screen.getByRole("button", { name: "Exit full screen" })
    ).toBeInTheDocument()
    expect(
      document.querySelector('[data-slot="dialog-content"]')
    ).toHaveAttribute("data-controls", "1")
  })
})
