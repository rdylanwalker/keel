import { render, screen, fireEvent, waitFor } from "@testing-library/react"
import { describe, it, expect, vi, beforeEach } from "vitest"
import { BootstrapLoginModal } from "./BootstrapLoginModal"
import * as api from "../lib/api"

vi.mock("../lib/api", () => ({
  bootstrapLogin: vi.fn(),
}))

describe("BootstrapLoginModal component", () => {
  const onSuccess = vi.fn()
  const onClose = vi.fn()

  beforeEach(() => {
    vi.clearAllMocks()
  })

  it("does not render when isOpen is false", () => {
    const { container } = render(
      <BootstrapLoginModal
        isOpen={false}
        onSuccess={onSuccess}
        onClose={onClose}
      />
    )
    expect(container.firstChild).toBeNull()
  })

  it("renders modal form when isOpen is true", () => {
    render(
      <BootstrapLoginModal
        isOpen={true}
        onSuccess={onSuccess}
        onClose={onClose}
      />
    )
    expect(screen.getByText("Stage 0 Bootstrap Auth")).toBeInTheDocument()
    expect(
      screen.getByPlaceholderText(/Enter 256-bit token/i)
    ).toBeInTheDocument()
  })

  it("shows validation error if submitted empty", async () => {
    render(<BootstrapLoginModal isOpen={true} onSuccess={onSuccess} />)
    const submitBtn = screen.getByRole("button", {
      name: /Authenticate Token/i,
    })
    fireEvent.click(submitBtn)

    expect(
      await screen.findByText(/Please enter a valid Stage 0 bootstrap token/i)
    ).toBeInTheDocument()
    expect(api.bootstrapLogin).not.toHaveBeenCalled()
  })

  it("authenticates token successfully and calls onSuccess", async () => {
    vi.mocked(api.bootstrapLogin).mockResolvedValueOnce({ success: true })

    render(
      <BootstrapLoginModal
        isOpen={true}
        onSuccess={onSuccess}
        onClose={onClose}
      />
    )

    const input = screen.getByPlaceholderText(/Enter 256-bit token/i)
    fireEvent.change(input, { target: { value: "token-12345" } })

    const submitBtn = screen.getByRole("button", {
      name: /Authenticate Token/i,
    })
    fireEvent.click(submitBtn)

    await waitFor(() => {
      expect(api.bootstrapLogin).toHaveBeenCalledWith("token-12345")
      expect(onSuccess).toHaveBeenCalled()
    })
  })

  it("authenticates token successfully when server returns authenticated: true", async () => {
    vi.mocked(api.bootstrapLogin).mockResolvedValueOnce({
      authenticated: true,
      sub: "bootstrap-admin",
      phase: "bootstrap",
    })

    render(
      <BootstrapLoginModal
        isOpen={true}
        onSuccess={onSuccess}
        onClose={onClose}
      />
    )

    const input = screen.getByPlaceholderText(/Enter 256-bit token/i)
    fireEvent.change(input, { target: { value: "token-67890" } })

    const submitBtn = screen.getByRole("button", {
      name: /Authenticate Token/i,
    })
    fireEvent.click(submitBtn)

    await waitFor(() => {
      expect(api.bootstrapLogin).toHaveBeenCalledWith("token-67890")
      expect(onSuccess).toHaveBeenCalled()
    })
  })

  it("displays API failure message on invalid token", async () => {
    vi.mocked(api.bootstrapLogin).mockResolvedValueOnce({
      success: false,
      message: "Token expired or invalid",
    })

    render(<BootstrapLoginModal isOpen={true} onSuccess={onSuccess} />)

    const input = screen.getByPlaceholderText(/Enter 256-bit token/i)
    fireEvent.change(input, { target: { value: "wrong-token" } })

    const submitBtn = screen.getByRole("button", {
      name: /Authenticate Token/i,
    })
    fireEvent.click(submitBtn)

    expect(
      await screen.findByText("Token expired or invalid")
    ).toBeInTheDocument()
    expect(onSuccess).not.toHaveBeenCalled()
  })

  it("handles exception thrown during login call", async () => {
    vi.mocked(api.bootstrapLogin).mockRejectedValueOnce(
      new Error("Network connection error")
    )

    render(<BootstrapLoginModal isOpen={true} onSuccess={onSuccess} />)

    const input = screen.getByPlaceholderText(/Enter 256-bit token/i)
    fireEvent.change(input, { target: { value: "some-token" } })

    const submitBtn = screen.getByRole("button", {
      name: /Authenticate Token/i,
    })
    fireEvent.click(submitBtn)

    expect(
      await screen.findByText("Network connection error")
    ).toBeInTheDocument()
  })

  it("calls onClose when close or cancel button is clicked", () => {
    render(
      <BootstrapLoginModal
        isOpen={true}
        onSuccess={onSuccess}
        onClose={onClose}
      />
    )

    const closeBtn = screen.getByLabelText("Close modal")
    fireEvent.click(closeBtn)
    expect(onClose).toHaveBeenCalledTimes(1)

    const cancelBtn = screen.getByRole("button", { name: /Cancel/i })
    fireEvent.click(cancelBtn)
    expect(onClose).toHaveBeenCalledTimes(2)
  })

  it("handles string exception thrown during login call", async () => {
    vi.mocked(api.bootstrapLogin).mockRejectedValueOnce("Server unreachable")

    render(<BootstrapLoginModal isOpen={true} onSuccess={onSuccess} />)

    const input = screen.getByPlaceholderText(/Enter 256-bit token/i)
    fireEvent.change(input, { target: { value: "some-token" } })

    const submitBtn = screen.getByRole("button", {
      name: /Authenticate Token/i,
    })
    fireEvent.click(submitBtn)

    expect(
      await screen.findByText("Failed to authenticate bootstrap token")
    ).toBeInTheDocument()
  })
})
