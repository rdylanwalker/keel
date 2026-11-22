import { act, fireEvent, render, screen } from "@testing-library/react"
import { describe, expect, it, vi } from "vitest"
import { useState } from "react"

import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "./dialog"
import { ConfirmDialog } from "./confirm-dialog"
import { Button } from "./button"
import { Badge } from "./badge"
import { Input } from "./input"
import { Textarea } from "./textarea"
import { NativeSelect } from "./native-select"
import { Label } from "./label"
import { Checkbox } from "./checkbox"

function DialogHarness({ onClose }: { onClose: () => void }) {
  const [open, setOpen] = useState(true)
  return (
    <>
      <button>trigger</button>
      <Dialog
        open={open}
        onOpenChange={(o) => {
          setOpen(o)
          if (!o) onClose()
        }}
      >
        <DialogContent size="lg">
          <DialogHeader>
            <DialogTitle>My dialog</DialogTitle>
            <DialogDescription>desc</DialogDescription>
          </DialogHeader>
          <DialogBody>
            <Input aria-label="name" />
          </DialogBody>
          <DialogFooter>
            <Button>ok</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  )
}

describe("shadcn ui primitives", () => {
  it("dialog closes on Escape", () => {
    const onClose = vi.fn()
    render(<DialogHarness onClose={onClose} />)
    const dialog = screen.getByRole("dialog", { name: "My dialog" })
    fireEvent.keyDown(dialog, { key: "Escape" })
    expect(onClose).toHaveBeenCalled()
    expect(screen.queryByRole("dialog")).toBeNull()
  })

  it("dialog closes on hash route change and via close button", () => {
    const onClose = vi.fn()
    const { unmount } = render(<DialogHarness onClose={onClose} />)
    act(() => {
      window.dispatchEvent(new HashChangeEvent("hashchange"))
    })
    expect(onClose).toHaveBeenCalledTimes(1)
    unmount()
    const onClose2 = vi.fn()
    render(<DialogHarness onClose={onClose2} />)
    fireEvent.click(screen.getByRole("button", { name: "Close" }))
    expect(onClose2).toHaveBeenCalled()
  })

  it("confirm dialog confirms and cancels", () => {
    const onConfirm = vi.fn()
    const onOpenChange = vi.fn()
    render(
      <ConfirmDialog
        open
        onOpenChange={onOpenChange}
        title="Reject?"
        description="This closes the PR"
        confirmLabel="Reject PR"
        destructive
        onConfirm={onConfirm}
      />
    )
    expect(screen.getByRole("alertdialog")).toHaveTextContent("This closes")
    fireEvent.click(screen.getByRole("button", { name: "Reject PR" }))
    expect(onConfirm).toHaveBeenCalled()
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }))
    expect(onOpenChange).toHaveBeenCalledWith(false)
  })

  it("renders form primitives", () => {
    const onChecked = vi.fn()
    render(
      <>
        <Label htmlFor="t">Label</Label>
        <Textarea id="t" />
        <NativeSelect aria-label="pick" defaultValue="b">
          <option value="a">a</option>
          <option value="b">b</option>
        </NativeSelect>
        <Checkbox aria-label="agree" onCheckedChange={onChecked} />
        <Badge variant="warning">warn</Badge>
        <Button asChild variant="link">
          <a href="#x">link</a>
        </Button>
      </>
    )
    expect(screen.getByLabelText("Label")).toBeInTheDocument()
    expect(screen.getByLabelText("pick")).toHaveValue("b")
    fireEvent.click(screen.getByRole("checkbox", { name: "agree" }))
    expect(onChecked).toHaveBeenCalledWith(true)
    expect(screen.getByText("warn")).toBeInTheDocument()
    expect(screen.getByRole("link", { name: "link" })).toHaveAttribute(
      "href",
      "#x"
    )
  })
})
