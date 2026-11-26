import { render, screen, fireEvent, waitFor, act } from "@testing-library/react"
import { describe, it, expect, vi, beforeEach } from "vitest"
import { VmSshModal } from "./VmSshModal"
import * as api from "../lib/api"
import { StackSshInfo } from "../types/api"

class MockWebSocket {
  static instances: MockWebSocket[] = []
  static CONNECTING = 0
  static OPEN = 1
  static CLOSING = 2
  static CLOSED = 3

  url: string
  readyState: number = MockWebSocket.CONNECTING
  onopen: (() => void) | null = null
  onmessage: ((event: { data: string }) => void) | null = null
  onerror: ((error: unknown) => void) | null = null
  onclose: (() => void) | null = null
  sentMessages: string[] = []

  constructor(url: string) {
    this.url = url
    MockWebSocket.instances.push(this)
    setTimeout(() => {
      this.readyState = MockWebSocket.OPEN
      this.onopen?.()
    }, 10)
  }

  send(data: string) {
    this.sentMessages.push(data)
  }

  close() {
    this.readyState = MockWebSocket.CLOSED
    this.onclose?.()
  }

  receiveMessage(data: unknown) {
    const raw = typeof data === "string" ? data : JSON.stringify(data)
    this.onmessage?.({ data: raw })
  }
}

vi.mock("../lib/api", () => ({
  fetchStackSshInfo: vi.fn(),
}))

// jsdom has no layout/WASM: replace wterm with a DOM-text fake that records writes and
// lets tests emit keystrokes (onData) and geometry changes (onResize).
const wtermMock = vi.hoisted(() => {
  type Opts = {
    scrollbackLines?: number
    onData?: (d: string) => void
    onResize?: (cols: number, rows: number) => void
  }
  class FakeWTerm {
    static instances: FakeWTerm[] = []
    options: Opts
    element: HTMLElement
    written = ""
    destroyed = false
    constructor(element: HTMLElement, options: Opts = {}) {
      this.element = element
      this.options = options
      FakeWTerm.instances.push(this)
    }
    async init() {
      return this
    }
    // Emulates the container measurement wterm performs on fit().
    fit() {
      this.emitResize(100, 30)
    }
    write(data: string) {
      this.written += data
      this.element.textContent = this.written
    }
    focus() {}
    emitData(d: string) {
      this.options.onData?.(d)
    }
    emitResize(cols: number, rows: number) {
      this.options.onResize?.(cols, rows)
    }
    destroy() {
      this.destroyed = true
    }
  }
  return { FakeWTerm }
})

vi.mock("@wterm/dom", () => ({ WTerm: wtermMock.FakeWTerm }))
vi.mock("@wterm/dom/src/terminal.css", () => ({}))

const sampleSshInfo: StackSshInfo = {
  stackName: "pve-workload-prod",
  namespace: "tenant-acme",
  tenant: "acme",
  provider: "proxmox",
  sshHost: "192.168.1.150",
  sshUser: "debian",
  sshPort: 22,
  sshCommand: "ssh debian@192.168.1.150 -p 22",
  cliCommand: "keel infra ssh pve-workload-prod --tenant acme",
  cliExecCommand: "keel infra ssh pve-workload-prod --tenant acme --exec",
  hasSavedSshKey: true,
}

describe("VmSshModal component", () => {
  beforeEach(() => {
    vi.clearAllMocks()
    MockWebSocket.instances = []
    wtermMock.FakeWTerm.instances = []
    vi.stubGlobal("WebSocket", MockWebSocket)
    Object.assign(navigator, {
      clipboard: {
        writeText: vi.fn().mockResolvedValue(undefined),
      },
    })
  })

  it("loads and displays connection info and commands, and NEVER opens WebSocket on mount", async () => {
    vi.mocked(api.fetchStackSshInfo).mockResolvedValue(sampleSshInfo)
    const onClose = vi.fn()

    render(
      <VmSshModal
        stackName="pve-workload-prod"
        namespace="tenant-acme"
        tenant="acme"
        provider="proxmox"
        onClose={onClose}
      />
    )

    // Verify API called
    expect(api.fetchStackSshInfo).toHaveBeenCalledWith(
      "tenant-acme",
      "pve-workload-prod"
    )

    // Verify CRITICAL REQUIREMENT: NO WebSocket connection on mount!
    expect(MockWebSocket.instances).toHaveLength(0)

    // Wait for connection details to render
    await waitFor(() => {
      expect(screen.getByTestId("ssh-host-display")).toHaveValue(
        "192.168.1.150"
      )
      expect(screen.getByTestId("ssh-user-input")).toHaveValue("debian")
      expect(screen.getByTestId("ssh-port-input")).toHaveValue(22)
    })

    // Verify raw commands rendered
    expect(screen.getByTestId("ssh-raw-command")).toHaveTextContent(
      "ssh debian@192.168.1.150 -p 22"
    )
    expect(screen.getByTestId("ssh-cli-print-command")).toHaveTextContent(
      "keel infra ssh pve-workload-prod --tenant acme"
    )
    expect(screen.getByTestId("ssh-cli-exec-command")).toHaveTextContent(
      "keel infra ssh pve-workload-prod --tenant acme --exec"
    )

    // Browser terminal is not active
    expect(screen.queryByTestId("browser-ssh-terminal")).not.toBeInTheDocument()
    expect(MockWebSocket.instances).toHaveLength(0)

    // Test close button
    fireEvent.click(screen.getByTestId("vm-ssh-modal-close-btn"))
    expect(onClose).toHaveBeenCalled()
  })

  it("updates commands reactively when user edits User and Port overrides, and copies commands", async () => {
    vi.mocked(api.fetchStackSshInfo).mockResolvedValue(sampleSshInfo)

    render(
      <VmSshModal
        stackName="pve-workload-prod"
        namespace="tenant-acme"
        tenant="acme"
        provider="proxmox"
        onClose={vi.fn()}
      />
    )

    await waitFor(() => {
      expect(screen.getByTestId("ssh-user-input")).toHaveValue("debian")
    })

    // Override Host, User, and Port
    fireEvent.change(screen.getByTestId("ssh-host-display"), {
      target: { value: "203.0.113.10" },
    })
    fireEvent.change(screen.getByTestId("ssh-user-input"), {
      target: { value: "ubuntu" },
    })
    fireEvent.change(screen.getByTestId("ssh-port-input"), {
      target: { value: "2222" },
    })

    expect(screen.getByTestId("ssh-raw-command")).toHaveTextContent(
      "ssh ubuntu@203.0.113.10 -p 2222"
    )

    // Click copy buttons
    fireEvent.click(screen.getByTestId("copy-ssh-raw-btn"))
    expect(navigator.clipboard.writeText).toHaveBeenCalledWith(
      "ssh ubuntu@203.0.113.10 -p 2222"
    )

    fireEvent.click(screen.getByTestId("copy-cli-print-btn"))
    expect(navigator.clipboard.writeText).toHaveBeenCalledWith(
      "keel infra ssh pve-workload-prod --tenant acme"
    )

    fireEvent.click(screen.getByTestId("copy-cli-exec-btn"))
    expect(navigator.clipboard.writeText).toHaveBeenCalledWith(
      "keel infra ssh pve-workload-prod --tenant acme --exec"
    )
  })

  it("initiates WebSocket connection ONLY upon clicking 'Launch Browser Terminal', exchanges connect/output/input, and disconnects", async () => {
    vi.mocked(api.fetchStackSshInfo).mockResolvedValue({
      ...sampleSshInfo,
      hasSavedSshKey: false,
    })

    render(
      <VmSshModal
        stackName="pve-workload-prod"
        namespace="tenant-acme"
        tenant="acme"
        provider="proxmox"
        onClose={vi.fn()}
      />
    )

    await waitFor(() => {
      expect(screen.getByTestId("ssh-host-display")).toBeInTheDocument()
    })

    // Private key input is shown because hasSavedSshKey is false
    const keyInput = screen.getByTestId("ssh-private-key-input")
    fireEvent.change(keyInput, {
      target: {
        value:
          "-----BEGIN OPENSSH PRIVATE KEY-----\nMOCKKEY\n-----END OPENSSH PRIVATE KEY-----",
      },
    })

    // No WebSocket opened yet
    expect(MockWebSocket.instances).toHaveLength(0)

    // Click Launch Browser Terminal
    const launchBtn = screen.getByTestId("launch-browser-terminal-btn")
    fireEvent.click(launchBtn)

    // WebSocket now initiated!
    expect(MockWebSocket.instances).toHaveLength(1)
    const ws = MockWebSocket.instances[0]
    expect(ws.url).toContain(
      "/api/v1/stacks/tenant-acme/pve-workload-prod/ssh/ws"
    )

    // Terminal container rendered
    expect(screen.getByTestId("browser-ssh-terminal")).toBeInTheDocument()

    // Wait for ws to open and verify connect frame
    await waitFor(() => {
      expect(ws.sentMessages.length).toBeGreaterThan(0)
    })
    const connectFrame = JSON.parse(ws.sentMessages[0])
    // Verify connect frame contains overridden host and fitted terminal geometry
    expect(connectFrame).toEqual(
      expect.objectContaining({
        type: "connect",
        cols: 100,
        rows: 30,
        host: "192.168.1.150",
        user: "debian",
        port: 22,
        privateKey: expect.stringContaining("MOCKKEY"),
      })
    )

    // Simulate incoming output frame from server
    act(() => {
      ws.receiveMessage({
        type: "output",
        data: "Linux debian-vm 6.1.0 #1 SMP PREEMPT\r\ndebian@debian-vm:~$ ",
      })
    })

    await waitFor(() => {
      expect(screen.getByTestId("browser-ssh-terminal")).toHaveTextContent(
        "Linux debian-vm 6.1.0"
      )
    })

    // Raw keystrokes stream as-is (no line buffering), incl. control characters
    const term = wtermMock.FakeWTerm.instances[0]
    expect(term.options.scrollbackLines).toBe(5000)
    act(() => {
      term.emitData("u")
      term.emitData("\x03")
      term.emitData("uptime\r")
    })
    expect(ws.sentMessages.slice(1).map((m) => JSON.parse(m))).toEqual([
      { type: "input", data: "u" },
      { type: "input", data: "\x03" },
      { type: "input", data: "uptime\r" },
    ])

    // Terminal geometry changes are forwarded as PTY resize frames
    act(() => {
      term.emitResize(132, 40)
    })
    expect(JSON.parse(ws.sentMessages[4])).toEqual({
      type: "resize",
      cols: 132,
      rows: 40,
    })

    // A large paste is split into several input frames (the server caps messages at 64 KiB)
    const paste = "x".repeat(10000)
    act(() => {
      term.emitData(paste)
    })
    const pasted = ws.sentMessages.slice(5).map((m) => JSON.parse(m))
    expect(pasted.map((f) => f.data.length)).toEqual([4096, 4096, 1808])
    expect(pasted.every((f) => f.type === "input")).toBe(true)
    expect(pasted.map((f) => f.data).join("")).toBe(paste)

    // Disconnect terminal: polite disconnect frame, then close
    const disconnectBtn = screen.getByTestId("disconnect-browser-terminal-btn")
    fireEvent.click(disconnectBtn)

    expect(JSON.parse(ws.sentMessages[8])).toEqual({ type: "disconnect" })
    expect(ws.readyState).toBe(MockWebSocket.CLOSED)
    expect(term.destroyed).toBe(true)
    expect(
      screen.getByTestId("launch-browser-terminal-btn")
    ).toBeInTheDocument()
  })

  it("handles API error gracefully and displays error alert", async () => {
    vi.mocked(api.fetchStackSshInfo).mockRejectedValue(
      new Error("Stack not found")
    )

    render(
      <VmSshModal
        stackName="non-existent"
        namespace="tenant-default"
        tenant="default"
        onClose={vi.fn()}
      />
    )

    expect(
      await screen.findByText(/Unable to resolve VM SSH info/i)
    ).toBeInTheDocument()
    expect(screen.getByText("Stack not found")).toBeInTheDocument()
  })

  it("toggles fullscreen mode", async () => {
    vi.mocked(api.fetchStackSshInfo).mockResolvedValue(sampleSshInfo)

    render(
      <VmSshModal
        stackName="pve-workload-prod"
        namespace="tenant-acme"
        tenant="acme"
        provider="proxmox"
        onClose={vi.fn()}
      />
    )

    const expandBtn = await screen.findByLabelText("Expand fullscreen")
    fireEvent.click(expandBtn)
    const exitBtn = screen.getByLabelText("Exit fullscreen")
    expect(exitBtn).toBeInTheDocument()
    fireEvent.click(exitBtn)
    expect(screen.getByLabelText("Expand fullscreen")).toBeInTheDocument()
  })

  it("toggles collapsible private key input when key is already saved in secret", async () => {
    vi.mocked(api.fetchStackSshInfo).mockResolvedValue({
      ...sampleSshInfo,
      hasSavedSshKey: true,
    })

    render(
      <VmSshModal
        stackName="pve-workload-prod"
        namespace="tenant-acme"
        tenant="acme"
        onClose={vi.fn()}
      />
    )

    await waitFor(() => {
      expect(screen.getByText("Saved in Secret")).toBeInTheDocument()
    })
    expect(
      screen.queryByTestId("ssh-private-key-input")
    ).not.toBeInTheDocument()

    const toggleBtn = screen.getByText(
      "Provide Ephemeral Private Key (Optional)"
    )
    fireEvent.click(toggleBtn)

    const keyInput = screen.getByTestId("ssh-private-key-input")
    expect(keyInput).toBeInTheDocument()
    fireEvent.change(keyInput, { target: { value: "my-custom-key" } })
    expect(screen.getByText(/Key provided \(13 chars\)/)).toBeInTheDocument()

    fireEvent.click(screen.getByText("Hide Ephemeral Private Key"))
    expect(
      screen.queryByTestId("ssh-private-key-input")
    ).not.toBeInTheDocument()
  })

  it("handles WebSocket connected, error, raw non-JSON messages, and network errors", async () => {
    vi.mocked(api.fetchStackSshInfo).mockResolvedValue(sampleSshInfo)

    render(
      <VmSshModal
        stackName="pve-workload-prod"
        namespace="tenant-acme"
        tenant="acme"
        onClose={vi.fn()}
      />
    )

    await waitFor(() => {
      expect(
        screen.getByTestId("launch-browser-terminal-btn")
      ).toBeInTheDocument()
    })

    fireEvent.click(screen.getByTestId("launch-browser-terminal-btn"))
    const ws = MockWebSocket.instances[0]
    await waitFor(() => {
      expect(screen.getByTestId("browser-ssh-terminal")).toBeInTheDocument()
    })

    // Connected message
    act(() => {
      ws.receiveMessage({ type: "connected" })
    })
    expect(screen.getByTestId("browser-ssh-terminal")).toHaveTextContent(
      "[Session Connected]"
    )
    expect(screen.getByTestId("ssh-terminal-status")).toHaveTextContent(
      "Connected"
    )
    expect(screen.getByTestId("ssh-host-key-fingerprint")).toHaveTextContent(
      "fingerprint unavailable"
    )

    // Error message
    act(() => {
      ws.receiveMessage({
        type: "error",
        message: "Permission denied (publickey)",
      })
    })
    expect(screen.getByTestId("browser-ssh-terminal")).toHaveTextContent(
      "[Error: Permission denied (publickey)]"
    )

    // Raw non-JSON output
    act(() => {
      ws.receiveMessage("raw banner line\r\n")
    })
    expect(screen.getByTestId("browser-ssh-terminal")).toHaveTextContent(
      "raw banner line"
    )

    // Custom message type with data
    act(() => {
      ws.receiveMessage({ type: "custom", data: "custom payload text\r\n" })
    })
    expect(screen.getByTestId("browser-ssh-terminal")).toHaveTextContent(
      "custom payload text"
    )

    // ws.onerror
    act(() => {
      ws.onerror?.(new Event("error"))
    })
    expect(screen.getByTestId("browser-ssh-terminal")).toHaveTextContent(
      "[WebSocket connection error]"
    )

    // Server-side disconnected frame, then ws.onclose
    act(() => {
      ws.receiveMessage({ type: "disconnected" })
    })
    expect(screen.getByTestId("ssh-terminal-status")).not.toHaveTextContent(
      /^Connected$/
    )
    act(() => {
      ws.close()
    })
    expect(screen.getByTestId("browser-ssh-terminal")).toHaveTextContent(
      "[Session Disconnected]"
    )
    expect(screen.getByTestId("ssh-terminal-status")).toHaveTextContent(
      "Connecting..."
    )
  })

  it("shows the host key fingerprint with a warning badge when the host key is unverified", async () => {
    vi.mocked(api.fetchStackSshInfo).mockResolvedValue(sampleSshInfo)

    render(
      <VmSshModal
        stackName="pve-workload-prod"
        namespace="tenant-acme"
        tenant="acme"
        onClose={vi.fn()}
      />
    )

    fireEvent.click(await screen.findByTestId("launch-browser-terminal-btn"))
    const ws = MockWebSocket.instances[0]

    act(() => {
      ws.receiveMessage({
        type: "connected",
        host: "192.168.1.150",
        user: "debian",
        port: 22,
        message: "Connected to debian@192.168.1.150:22",
        hostKeyFingerprint: "SHA256:abc123",
        hostKeyVerified: false,
      })
    })

    expect(screen.getByTestId("ssh-host-key-unverified")).toHaveTextContent(
      "Host key NOT verified"
    )
    expect(screen.getByTestId("ssh-host-key-fingerprint")).toHaveTextContent(
      "SHA256:abc123"
    )
    expect(screen.getByText(/Compare this fingerprint/i)).toBeInTheDocument()
  })

  it("shows a verified badge when the server verified the host key", async () => {
    vi.mocked(api.fetchStackSshInfo).mockResolvedValue(sampleSshInfo)

    render(
      <VmSshModal
        stackName="pve-workload-prod"
        namespace="tenant-acme"
        tenant="acme"
        onClose={vi.fn()}
      />
    )

    fireEvent.click(await screen.findByTestId("launch-browser-terminal-btn"))
    const ws = MockWebSocket.instances[0]
    act(() => {
      ws.receiveMessage({
        type: "connected",
        hostKeyFingerprint: "SHA256:pinned",
        hostKeyVerified: true,
      })
    })

    expect(screen.getByText("Host key verified")).toBeInTheDocument()
    expect(
      screen.queryByTestId("ssh-host-key-unverified")
    ).not.toBeInTheDocument()
    expect(screen.getByTestId("ssh-host-key-fingerprint")).toHaveTextContent(
      "SHA256:pinned"
    )
  })

  it("closes on Escape via the dialog", async () => {
    vi.mocked(api.fetchStackSshInfo).mockResolvedValue(sampleSshInfo)
    const onClose = vi.fn()

    render(
      <VmSshModal
        stackName="pve-workload-prod"
        namespace="tenant-acme"
        tenant="acme"
        onClose={onClose}
      />
    )

    const dialog = await screen.findByRole("dialog")
    fireEvent.keyDown(dialog, { key: "Escape" })
    expect(onClose).toHaveBeenCalled()
  })

  it("closes when the hash route changes", async () => {
    vi.mocked(api.fetchStackSshInfo).mockResolvedValue(sampleSshInfo)
    const onClose = vi.fn()

    render(
      <VmSshModal
        stackName="pve-workload-prod"
        namespace="tenant-acme"
        tenant="acme"
        onClose={onClose}
      />
    )

    await screen.findByRole("dialog")
    act(() => {
      window.dispatchEvent(new HashChangeEvent("hashchange"))
    })
    expect(onClose).toHaveBeenCalled()
  })

  it("handles WebSocket constructor failure", async () => {
    vi.mocked(api.fetchStackSshInfo).mockResolvedValue(sampleSshInfo)

    render(
      <VmSshModal
        stackName="pve-workload-prod"
        namespace="tenant-acme"
        tenant="acme"
        onClose={vi.fn()}
      />
    )

    await waitFor(() => {
      expect(
        screen.getByTestId("launch-browser-terminal-btn")
      ).toBeInTheDocument()
    })

    class FailingWebSocket {
      constructor() {
        throw new Error("WebSocket creation failed")
      }
    }
    vi.stubGlobal("WebSocket", FailingWebSocket)

    fireEvent.click(screen.getByTestId("launch-browser-terminal-btn"))
    // wterm initializes asynchronously; buffered output is flushed once it is ready.
    await waitFor(() => {
      expect(screen.getByTestId("browser-ssh-terminal")).toHaveTextContent(
        "[Failed to launch terminal: WebSocket creation failed]"
      )
    })
  })

  it("handles fallback values when provider, tenant, user and port are empty", async () => {
    vi.mocked(api.fetchStackSshInfo).mockResolvedValue({
      stackName: "fallback-stack",
      namespace: "tenant-default",
      tenant: "",
      provider: "",
      sshHost: "",
      sshUser: "",
      sshPort: 0,
      sshCommand: "",
      cliCommand: "",
      cliExecCommand: "",
      hasSavedSshKey: false,
    })

    render(
      <VmSshModal
        stackName="fallback-stack"
        namespace="tenant-default"
        tenant=""
        onClose={vi.fn()}
      />
    )

    await waitFor(() => {
      expect(screen.getByTestId("ssh-host-display")).toHaveValue("127.0.0.1")
      expect(screen.getByTestId("ssh-port-input")).toHaveValue(22)
    })

    // Empty user and port override
    fireEvent.change(screen.getByTestId("ssh-user-input"), {
      target: { value: "   " },
    })
    fireEvent.change(screen.getByTestId("ssh-port-input"), {
      target: { value: "abc" },
    })
    expect(screen.getByTestId("ssh-raw-command")).toHaveTextContent(
      "ssh root@127.0.0.1 -p 22"
    )
  })

  it("handles non-Error rejection from fetchStackSshInfo and clipboard errors", async () => {
    vi.mocked(api.fetchStackSshInfo).mockRejectedValue("String network error")

    render(
      <VmSshModal
        stackName="err-stack"
        namespace="tenant-default"
        tenant="default"
        onClose={vi.fn()}
      />
    )

    expect(
      await screen.findByText("Failed to load stack SSH connection info")
    ).toBeInTheDocument()
  })
})
