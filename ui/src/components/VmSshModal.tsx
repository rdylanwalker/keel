import React, { useCallback, useEffect, useRef, useState } from "react"
import {
  AlertCircle,
  Check,
  Key,
  Loader2,
  Maximize2,
  Minimize2,
  Play,
  ShieldAlert,
  ShieldCheck,
  Square,
  Terminal as TerminalIcon,
  X,
} from "lucide-react"
import { StackSshInfo } from "../types/api"
import { fetchStackSshInfo } from "../lib/api"
import { errorMessage } from "../lib/errors"
import { chunkTerminalInput } from "../lib/terminalInput"
import { cn } from "../lib/utils"
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "./ui/dialog"
import { Badge } from "./ui/badge"
import { Button } from "./ui/button"
import { Input } from "./ui/input"
import { Label } from "./ui/label"
import { Textarea } from "./ui/textarea"
import { TerminalHandle, WebTerminal } from "./ssh/WebTerminal"
import { CopyCommandBlock } from "./ssh/CopyCommandBlock"

export interface VmSshModalProps {
  stackName: string
  namespace: string
  tenant: string
  provider?: string
  onClose: () => void
}

/** Host key details reported by the server's `connected` frame. */
export interface SshHostKeyInfo {
  fingerprint?: string
  verified: boolean
}

/**
 * Server → client WebSocket frames (`crates/keel-server/src/ssh.rs` `SshWsServerMessage`).
 * Client → server frames: `connect` (with cols/rows/host/user/port/privateKey), raw
 * keystroke `input` (`{type:"input",data}`), PTY `resize` (`{type:"resize",cols,rows}`),
 * and `disconnect`.
 */
type SshServerFrame =
  | {
      type: "connected"
      host?: string
      user?: string
      port?: number
      message?: string
      hostKeyFingerprint?: string | null
      hostKeyVerified?: boolean
    }
  | { type: "output"; data: string }
  | { type: "error"; message?: string }
  | { type: "disconnected" }
  | { type?: string; data?: unknown; message?: unknown }

const ANSI_DIM = "\x1b[90m"
const ANSI_RED = "\x1b[31m"
const ANSI_GREEN = "\x1b[32m"
const ANSI_YELLOW = "\x1b[33m"
const ANSI_RESET = "\x1b[0m"

export const VmSshModal: React.FC<VmSshModalProps> = ({
  stackName,
  namespace,
  tenant,
  provider,
  onClose,
}) => {
  const [sshInfo, setSshInfo] = useState<StackSshInfo | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [isFullscreen, setIsFullscreen] = useState(false)

  // User connection parameter overrides
  const [hostOverride, setHostOverride] = useState("")
  const [userOverride, setUserOverride] = useState("")
  const [portOverride, setPortOverride] = useState("")
  const [privateKey, setPrivateKey] = useState("")
  const [showPrivateKeyInput, setShowPrivateKeyInput] = useState(false)

  const [copiedKey, setCopiedKey] = useState<string | null>(null)

  // Terminal state - CRITICAL: terminal and WebSocket must NEVER be opened automatically on mount!
  const [isTerminalActive, setIsTerminalActive] = useState(false)
  const [terminalConnected, setTerminalConnected] = useState(false)
  const [sessionEstablished, setSessionEstablished] = useState(false)
  const [hostKey, setHostKey] = useState<SshHostKeyInfo | null>(null)

  const wsRef = useRef<WebSocket | null>(null)
  const termRef = useRef<TerminalHandle | null>(null)
  const pendingWritesRef = useRef<string[]>([])

  useEffect(() => {
    let cancelled = false

    fetchStackSshInfo(namespace, stackName)
      .then((info) => {
        if (!cancelled) {
          setSshInfo(info)
          setHostOverride(info.sshHost || "127.0.0.1")
          setUserOverride(info.sshUser)
          setPortOverride(info.sshPort ? String(info.sshPort) : "22")
          if (!info.hasSavedSshKey) {
            setShowPrivateKeyInput(true)
          }
        }
      })
      .catch((err) => {
        if (!cancelled) {
          setError(
            errorMessage(err, "Failed to load stack SSH connection info")
          )
        }
      })
      .finally(() => {
        if (!cancelled) {
          setLoading(false)
        }
      })

    return () => {
      cancelled = true
    }
  }, [namespace, stackName])

  // Cleanup WebSocket on modal unmount
  useEffect(() => {
    return () => {
      if (wsRef.current) {
        try {
          wsRef.current.close()
        } catch {
          // ignore close error on unmount
        }
        wsRef.current = null
      }
    }
  }, [])

  const effectiveUser = userOverride.trim() || sshInfo?.sshUser || "root"
  const effectivePort = portOverride.trim()
    ? parseInt(portOverride.trim(), 10) || 22
    : sshInfo?.sshPort || 22
  const effectiveHost = hostOverride.trim() || sshInfo?.sshHost || "127.0.0.1"
  const effectiveTenant = tenant || sshInfo?.tenant || "default"
  const effectiveProvider = provider || sshInfo?.provider || "unknown"

  const rawSshCommand = `ssh ${effectiveUser}@${effectiveHost} -p ${effectivePort}`
  const cliPrintCommand = `keel infra ssh ${stackName} --tenant ${effectiveTenant}`
  const cliExecCommand = `keel infra ssh ${stackName} --tenant ${effectiveTenant} --exec`

  const handleCopy = (text: string, key: string) => {
    if (typeof navigator !== "undefined" && navigator.clipboard?.writeText) {
      navigator.clipboard.writeText(text).catch(() => {})
    }
    setCopiedKey(key)
    window.setTimeout(() => {
      setCopiedKey((curr) => (curr === key ? null : curr))
    }, 2000)
  }

  const writeTerm = useCallback((data: string) => {
    if (termRef.current) {
      termRef.current.write(data)
    } else {
      pendingWritesRef.current.push(data)
    }
  }, [])

  const sendFrame = useCallback((frame: Record<string, unknown>) => {
    const ws = wsRef.current
    if (!ws || ws.readyState !== WebSocket.OPEN) return false
    ws.send(JSON.stringify(frame))
    return true
  }, [])

  const handleTerminalReady = useCallback((handle: TerminalHandle) => {
    termRef.current = handle
    const pending = pendingWritesRef.current
    pendingWritesRef.current = []
    pending.forEach((chunk) => handle.write(chunk))
  }, [])

  const handleTerminalData = useCallback(
    (data: string) => {
      // Large pastes go out as several frames: the server ends sessions on messages > 64 KiB.
      for (const chunk of chunkTerminalInput(data)) {
        sendFrame({ type: "input", data: chunk })
      }
    },
    [sendFrame]
  )

  const handleTerminalResize = useCallback(
    (cols: number, rows: number) => {
      sendFrame({ type: "resize", cols, rows })
    },
    [sendFrame]
  )

  const handleServerFrame = (raw: unknown) => {
    let msg: SshServerFrame
    try {
      msg = JSON.parse(String(raw)) as SshServerFrame
    } catch {
      // Non-JSON raw stream output
      writeTerm(String(raw))
      return
    }
    if (msg.type === "output" && typeof msg.data === "string") {
      writeTerm(msg.data)
    } else if (msg.type === "connected") {
      const connected = msg as Extract<SshServerFrame, { type: "connected" }>
      setTerminalConnected(true)
      setSessionEstablished(true)
      setHostKey({
        fingerprint: connected.hostKeyFingerprint || undefined,
        verified: Boolean(connected.hostKeyVerified),
      })
      writeTerm(
        `${ANSI_GREEN}[Session Connected]${ANSI_RESET} ${ANSI_DIM}${connected.message ?? ""}${ANSI_RESET}\r\n`
      )
      termRef.current?.focus()
    } else if (msg.type === "error") {
      const errorMsg =
        typeof msg.message === "string" && msg.message
          ? msg.message
          : "SSH Terminal error"
      writeTerm(`\r\n${ANSI_RED}[Error: ${errorMsg}]${ANSI_RESET}\r\n`)
    } else if (msg.type === "disconnected") {
      setSessionEstablished(false)
    } else {
      const loose = msg as { data?: unknown }
      if (typeof loose.data === "string") {
        writeTerm(loose.data)
      }
    }
  }

  const handleLaunchTerminal = () => {
    if (isTerminalActive && wsRef.current) return

    pendingWritesRef.current = []
    setIsTerminalActive(true)
    setTerminalConnected(false)
    setSessionEstablished(false)
    setHostKey(null)
    writeTerm(
      `${ANSI_YELLOW}[Connecting to ${effectiveUser}@${effectiveHost}:${effectivePort}...]${ANSI_RESET}\r\n`
    )

    try {
      const wsProtocol = window.location.protocol === "https:" ? "wss:" : "ws:"
      const host = window.location.host || "localhost"
      const url = `${wsProtocol}//${host}/api/v1/stacks/${encodeURIComponent(namespace)}/${encodeURIComponent(stackName)}/ssh/ws`

      const ws = new WebSocket(url)
      wsRef.current = ws

      ws.onopen = () => {
        setTerminalConnected(true)
        const { cols, rows } = termRef.current?.size() ?? {
          cols: 80,
          rows: 24,
        }
        ws.send(
          JSON.stringify({
            type: "connect",
            cols,
            rows,
            host: effectiveHost,
            user: effectiveUser,
            port: effectivePort,
            ...(privateKey.trim() ? { privateKey: privateKey.trim() } : {}),
          })
        )
      }

      ws.onmessage = (event) => handleServerFrame(event.data)

      ws.onerror = () => {
        writeTerm(
          `\r\n${ANSI_RED}[WebSocket connection error]${ANSI_RESET}\r\n`
        )
      }

      ws.onclose = () => {
        setTerminalConnected(false)
        setSessionEstablished(false)
        writeTerm(`\r\n${ANSI_DIM}[Session Disconnected]${ANSI_RESET}\r\n`)
      }
    } catch (err) {
      const msg = errorMessage(err, "Failed to open WebSocket")
      writeTerm(
        `\r\n${ANSI_RED}[Failed to launch terminal: ${msg}]${ANSI_RESET}\r\n`
      )
    }
  }

  const handleDisconnectTerminal = () => {
    const ws = wsRef.current
    if (ws) {
      try {
        if (ws.readyState === WebSocket.OPEN) {
          ws.send(JSON.stringify({ type: "disconnect" }))
        }
        ws.close()
      } catch {
        // ignore
      }
      wsRef.current = null
    }
    termRef.current = null
    pendingWritesRef.current = []
    setTerminalConnected(false)
    setSessionEstablished(false)
    setIsTerminalActive(false)
  }

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) onClose()
      }}
    >
      <DialogContent
        size="xl"
        hideClose
        aria-label={`SSH Connect - ${stackName}`}
        className={cn(
          isFullscreen &&
            "w-screen h-dvh max-h-dvh sm:max-w-none rounded-none border-0"
        )}
      >
        <DialogHeader className="pr-5 bg-slate-950/70">
          <div className="flex items-start justify-between gap-3">
            <div className="flex items-start gap-3 min-w-0">
              <div className="p-2 rounded-xl bg-emerald-500/10 border border-emerald-500/20 text-emerald-400 shrink-0">
                <TerminalIcon className="h-5 w-5" />
              </div>
              <div className="min-w-0">
                <DialogTitle
                  className="truncate font-mono"
                  title={`SSH Connect: ${stackName}`}
                >
                  SSH Connect: {stackName}
                </DialogTitle>
                <div className="flex flex-wrap items-center gap-1.5 mt-1">
                  <Badge variant="secondary" className="uppercase">
                    {effectiveProvider}
                  </Badge>
                  {effectiveTenant && (
                    <Badge className="font-mono">
                      tenant: {effectiveTenant}
                    </Badge>
                  )}
                </div>
                <DialogDescription className="mt-1">
                  Target VM connection info, direct SSH commands, and optional
                  in-browser web terminal
                </DialogDescription>
              </div>
            </div>

            <div className="flex items-center gap-1 shrink-0">
              <Button
                variant="ghost"
                size="icon-sm"
                onClick={() => setIsFullscreen((prev) => !prev)}
                aria-label={
                  isFullscreen ? "Exit fullscreen" : "Expand fullscreen"
                }
              >
                {isFullscreen ? (
                  <Minimize2 className="h-4 w-4" />
                ) : (
                  <Maximize2 className="h-4 w-4" />
                )}
              </Button>
              <Button
                variant="ghost"
                size="icon-sm"
                data-testid="vm-ssh-modal-close-btn"
                onClick={onClose}
                aria-label="Close"
              >
                <X className="h-4 w-4" />
              </Button>
            </div>
          </div>
        </DialogHeader>

        <DialogBody className="space-y-6">
          {loading && (
            <div
              role="status"
              className="flex items-center justify-center py-12 text-slate-400 gap-2 text-sm"
            >
              <Loader2 className="h-5 w-5 animate-spin text-emerald-400" />
              <span>Resolving VM network endpoints and SSH secrets...</span>
            </div>
          )}

          {error && (
            <div
              role="alert"
              className="p-4 rounded-xl bg-red-500/10 border border-red-500/30 text-red-300 text-xs flex items-start gap-2.5"
            >
              <AlertCircle className="h-4 w-4 shrink-0 mt-0.5" />
              <div>
                <p className="font-semibold">Unable to resolve VM SSH info</p>
                <p className="mt-0.5 opacity-90">{error}</p>
              </div>
            </div>
          )}

          {!loading && (
            <>
              {/* Connection Parameters & Overrides */}
              <div className="bg-slate-950/60 border border-slate-800/80 rounded-xl p-4 space-y-3">
                <div className="text-xs font-semibold text-slate-300 uppercase tracking-wider">
                  Target Endpoint &amp; Credentials
                </div>
                <div className="grid grid-cols-1 sm:grid-cols-4 gap-3 text-xs">
                  <div className="space-y-1">
                    <Label htmlFor="ssh-host-display">Host</Label>
                    <Input
                      id="ssh-host-display"
                      type="text"
                      value={hostOverride}
                      onChange={(e) => setHostOverride(e.target.value)}
                      data-testid="ssh-host-display"
                      placeholder="e.g. 192.168.1.150"
                    />
                  </div>
                  <div className="space-y-1">
                    <Label htmlFor="ssh-user-input">User</Label>
                    <Input
                      id="ssh-user-input"
                      type="text"
                      value={userOverride}
                      onChange={(e) => setUserOverride(e.target.value)}
                      data-testid="ssh-user-input"
                      placeholder="e.g. debian"
                    />
                  </div>
                  <div className="space-y-1">
                    <Label htmlFor="ssh-port-input">Port</Label>
                    <Input
                      id="ssh-port-input"
                      type="number"
                      value={portOverride}
                      onChange={(e) => setPortOverride(e.target.value)}
                      data-testid="ssh-port-input"
                      placeholder="22"
                    />
                  </div>
                  <div className="space-y-1">
                    <span className="block text-xs font-medium text-slate-300">
                      Key Storage
                    </span>
                    <div className="flex items-center gap-1.5 py-1.5">
                      {sshInfo?.hasSavedSshKey ? (
                        <Badge variant="success" className="text-[11px]">
                          <Check className="h-3 w-3" />
                          Saved in Secret
                        </Badge>
                      ) : (
                        <Badge variant="warning" className="text-[11px]">
                          Ephemeral / Not Saved
                        </Badge>
                      )}
                    </div>
                  </div>
                </div>

                {/* Collapsible Private Key Input */}
                <div className="pt-2 border-t border-slate-800/80">
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <Button
                      variant="link"
                      size="sm"
                      className="px-0"
                      aria-expanded={showPrivateKeyInput}
                      onClick={() => setShowPrivateKeyInput((v) => !v)}
                    >
                      <Key className="h-3.5 w-3.5" />
                      <span>
                        {showPrivateKeyInput
                          ? "Hide Ephemeral Private Key"
                          : "Provide Ephemeral Private Key (Optional)"}
                      </span>
                    </Button>
                    {privateKey.trim() && (
                      <span className="text-[11px] text-emerald-400 font-mono">
                        Key provided ({privateKey.trim().length} chars)
                      </span>
                    )}
                  </div>
                  {showPrivateKeyInput && (
                    <div className="mt-2">
                      <Textarea
                        data-testid="ssh-private-key-input"
                        aria-label="Ephemeral SSH private key"
                        rows={4}
                        value={privateKey}
                        autoComplete="off"
                        spellCheck={false}
                        onChange={(e) => setPrivateKey(e.target.value)}
                        placeholder={
                          "-----BEGIN OPENSSH PRIVATE KEY-----\n...\n-----END OPENSSH PRIVATE KEY-----"
                        }
                      />
                      <p className="text-[11px] text-slate-500 mt-1">
                        Ephemeral private key passed securely over WebSocket for
                        in-browser terminal session authentication. Never
                        persisted.
                      </p>
                    </div>
                  )}
                </div>
              </div>

              {/* Copyable Connection Commands */}
              <div className="space-y-3">
                <div className="text-xs font-semibold text-slate-300 uppercase tracking-wider">
                  Copyable Connection Commands
                </div>
                <CopyCommandBlock
                  label="1. Standard SSH Client"
                  command={rawSshCommand}
                  copied={copiedKey === "ssh-raw"}
                  onCopy={() => handleCopy(rawSshCommand, "ssh-raw")}
                  copyTestId="copy-ssh-raw-btn"
                  commandTestId="ssh-raw-command"
                  tone="emerald"
                />
                <CopyCommandBlock
                  label="2. Keel CLI (Print Connection Info)"
                  command={cliPrintCommand}
                  copied={copiedKey === "cli-print"}
                  onCopy={() => handleCopy(cliPrintCommand, "cli-print")}
                  copyTestId="copy-cli-print-btn"
                  commandTestId="ssh-cli-print-command"
                  tone="indigo"
                />
                <CopyCommandBlock
                  label="3. Keel CLI (Interactive Direct Exec)"
                  command={cliExecCommand}
                  copied={copiedKey === "cli-exec"}
                  onCopy={() => handleCopy(cliExecCommand, "cli-exec")}
                  copyTestId="copy-cli-exec-btn"
                  commandTestId="ssh-cli-exec-command"
                  tone="amber"
                />
              </div>

              {/* Opt-In In-Browser Terminal Section */}
              <div className="pt-2 border-t border-slate-800/80 space-y-3">
                <div className="flex flex-wrap items-center justify-between gap-3">
                  <div>
                    <h4 className="text-xs font-semibold text-slate-200 uppercase tracking-wider">
                      Opt-In In-Browser Terminal
                    </h4>
                    <p className="text-[11px] text-slate-400 mt-0.5">
                      Launches an interactive SSH terminal session directly in
                      your browser over WebSocket
                    </p>
                  </div>
                  {!isTerminalActive ? (
                    <Button
                      variant="success"
                      data-testid="launch-browser-terminal-btn"
                      onClick={handleLaunchTerminal}
                    >
                      <Play className="h-3.5 w-3.5 fill-current" />
                      <span>Launch Browser Terminal</span>
                    </Button>
                  ) : (
                    <Button
                      variant="destructive-subtle"
                      data-testid="disconnect-browser-terminal-btn"
                      onClick={handleDisconnectTerminal}
                    >
                      <Square className="h-3.5 w-3.5 fill-current" />
                      <span>Disconnect Terminal</span>
                    </Button>
                  )}
                </div>

                {isTerminalActive && (
                  <div className="border border-slate-800 rounded-xl bg-slate-950 overflow-hidden shadow-2xl">
                    <div className="flex flex-wrap items-center justify-between gap-2 px-3 py-1.5 bg-slate-900 border-b border-slate-800 text-[11px] text-slate-400 font-mono">
                      <div className="flex items-center gap-2 min-w-0">
                        <span
                          aria-hidden="true"
                          className={`h-2 w-2 rounded-full shrink-0 ${
                            sessionEstablished
                              ? "bg-emerald-400 animate-pulse"
                              : "bg-amber-400"
                          }`}
                        />
                        <span className="truncate">
                          {effectiveUser}@{effectiveHost}:{effectivePort}
                        </span>
                      </div>
                      <span data-testid="ssh-terminal-status" role="status">
                        {sessionEstablished
                          ? "Connected"
                          : terminalConnected
                            ? "Authenticating..."
                            : "Connecting..."}
                      </span>
                    </div>

                    {hostKey && (
                      <div
                        data-testid="ssh-host-key-info"
                        className={cn(
                          "flex flex-wrap items-center gap-2 px-3 py-1.5 border-b text-[11px]",
                          hostKey.verified
                            ? "border-slate-800 bg-emerald-500/5"
                            : "border-amber-500/30 bg-amber-500/10"
                        )}
                      >
                        {hostKey.verified ? (
                          <Badge variant="success">
                            <ShieldCheck className="h-3 w-3" />
                            Host key verified
                          </Badge>
                        ) : (
                          <Badge
                            variant="warning"
                            data-testid="ssh-host-key-unverified"
                          >
                            <ShieldAlert className="h-3 w-3" />
                            Host key NOT verified
                          </Badge>
                        )}
                        <code
                          className="font-mono text-slate-300 break-all"
                          data-wrap-ok
                          data-testid="ssh-host-key-fingerprint"
                        >
                          {hostKey.fingerprint ?? "fingerprint unavailable"}
                        </code>
                        {!hostKey.verified && (
                          <span className="text-amber-200/90">
                            No expected host key is published in the stack
                            outputs. Compare this fingerprint out-of-band before
                            entering secrets.
                          </span>
                        )}
                      </div>
                    )}

                    <WebTerminal
                      data-testid="browser-ssh-terminal"
                      className={cn(
                        "p-2 bg-slate-950",
                        isFullscreen ? "h-[60dvh]" : "h-72"
                      )}
                      onData={handleTerminalData}
                      onResize={handleTerminalResize}
                      onReady={handleTerminalReady}
                    />
                  </div>
                )}
              </div>
            </>
          )}
        </DialogBody>
      </DialogContent>
    </Dialog>
  )
}
