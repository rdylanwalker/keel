import { render, screen, fireEvent } from "@testing-library/react"
import { describe, it, expect, vi } from "vitest"
import { PrNotificationBanner } from "./PrNotificationBanner"

describe("PrNotificationBanner component", () => {
  it("renders default banner variant with message and handles dismiss", () => {
    const onDismiss = vi.fn()
    render(
      <PrNotificationBanner
        message="PR #42 opened on branch feature/test"
        onDismiss={onDismiss}
      />
    )

    expect(
      screen.getByText("PR #42 opened on branch feature/test")
    ).toBeInTheDocument()
    expect(screen.getByRole("status")).toBeInTheDocument()

    fireEvent.click(screen.getByText("Dismiss"))
    expect(onDismiss).toHaveBeenCalledTimes(1)
  })

  it("renders banner variant with Go to PR button and prUrl", () => {
    const onNavigate = vi.fn()
    render(
      <PrNotificationBanner
        message="PR #100 created"
        prId="100"
        prUrl="https://forge.example.com/pr/100"
        onNavigateToPreview={onNavigate}
        onDismiss={vi.fn()}
      />
    )

    const goBtn = screen.getByRole("button", { name: /Go to PR →/i })
    expect(goBtn).toBeInTheDocument()
    fireEvent.click(goBtn)
    expect(onNavigate).toHaveBeenCalledTimes(1)

    const forgeLink = screen.getByRole("link", { name: /View on Forge/i })
    expect(forgeLink).toHaveAttribute(
      "href",
      "https://forge.example.com/pr/100"
    )
  })

  it("renders snackbar variant with floating toast style and handles dismiss", () => {
    const onDismiss = vi.fn()
    render(
      <PrNotificationBanner
        message="PR #2 merged cleanly!"
        variant="snackbar"
        onDismiss={onDismiss}
      />
    )

    expect(screen.getByText("PR #2 merged cleanly!")).toBeInTheDocument()
    const snackbar = screen.getByRole("status")
    expect(snackbar.className).toContain("fixed")
    expect(snackbar.className).toContain("bottom-6")
    expect(snackbar.className).toContain("right-6")

    const dismissBtn = screen.getByRole("button", {
      name: /Dismiss notification/i,
    })
    fireEvent.click(dismissBtn)
    expect(onDismiss).toHaveBeenCalledTimes(1)
  })

  it("renders snackbar variant with action links and buttons", () => {
    const onNavigate = vi.fn()
    render(
      <PrNotificationBanner
        message="PR #55 merged cleanly!"
        prId="55"
        prUrl="https://forge.example.com/pr/55"
        onNavigateToPreview={onNavigate}
        variant="snackbar"
        onDismiss={vi.fn()}
      />
    )

    expect(
      screen.getByRole("button", { name: /Go to PR →/i })
    ).toBeInTheDocument()
    expect(
      screen.getByRole("link", { name: /View on Forge/i })
    ).toBeInTheDocument()
  })
})
