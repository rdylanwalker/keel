import React, { useEffect, useRef, useState } from "react"
import {
  AlertCircle,
  Check,
  Copy,
  Key,
  Loader2,
  Maximize2,
  Minimize2,
  Play,
  Square,
  Terminal as TerminalIcon,
  X,
} from "lucide-react"
import { StackSshInfo } from "../types/api"
import { fetchStackSshInfo } from "../lib/api"

export interface VmSshModalProps {
  stackName: string
  namespace: string
  tenant: string
  provider?: string
  onClose: () => void
}

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

  // Copy status indicators
  const [copiedKey, setCopiedKey] = useState<string | null>(null)

  // Terminal state - CRITICAL: terminal and WebSocket must NEVER be opened automatically on mount!
  const [isTerminalActive, setIsTerminalActive] = useState(false)
  const [terminalConnected, setTerminalConnected] = useState(false)
  const [terminalOutput, setTerminalOutput] = useState<string>("")
  const [terminalInput, setTerminalInput] = useState("")
  const [terminalError, setTerminalError] = useState<string | null>(null)

  const wsRef = useRef<WebSocket | null>(null)
  const terminalScrollRef = useRef<HTMLDivElement | null>(null)

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
            err instanceof Error
              ? err.message
              : "Failed to load stack SSH connection info"
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

  // Auto-scroll terminal viewport on new output
  useEffect(() => {
    if (terminalScrollRef.current) {
      terminalScrollRef.current.scrollTop =
        terminalScrollRef.current.scrollHeight
    }
  }, [terminalOutput])

  const effectiveUser = userOverride.trim() || sshInfo?.sshUser || "root"
  const effectivePort = portOverride.trim()
    ? parseInt(portOverride.trim(), 10) || 22
    : sshInfo?.sshPort || 22
  const effectiveHost = hostOverride.trim() || sshInfo?.sshHost || "127.0.0.1"
  const effectiveTenant = tenant || sshInfo?.tenant || "default"
  const effectiveProvider = provider || sshInfo?.provider || "proxmox"

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

  const handleLaunchTerminal = () => {
    if (isTerminalActive && wsRef.current) return

    setIsTerminalActive(true)
    setTerminalConnected(false)
    setTerminalError(null)
    setTerminalOutput(
      `[Connecting to ${effectiveUser}@${effectiveHost}:${effectivePort}...]\r\n`
    )

    try {
      const wsProtocol = window.location.protocol === "https:" ? "wss:" : "ws:"
      const host = window.location.host || "localhost"
      const url = `${wsProtocol}//${host}/api/v1/stacks/${encodeURIComponent(namespace)}/${encodeURIComponent(stackName)}/ssh/ws`

      const ws = new WebSocket(url)
      wsRef.current = ws

      ws.onopen = () => {
        setTerminalConnected(true)
        const connectFrame = {
          type: "connect",
          cols: 80,
          rows: 24,
          host: effectiveHost,
          user: effectiveUser,
          port: effectivePort,
          ...(privateKey.trim() ? { privateKey: privateKey.trim() } : {}),
        }
        ws.send(JSON.stringify(connectFrame))
      }

      ws.onmessage = (event) => {
        try {
          const msg = JSON.parse(event.data)
          if (msg.type === "output" && typeof msg.data === "string") {
            setTerminalOutput((prev) => prev + msg.data)
          } else if (msg.type === "connected") {
            setTerminalConnected(true)
            setTerminalOutput((prev) => prev + `[Session Connected]\r\n`)
          } else if (msg.type === "error") {
            const errorMsg = msg.message || "SSH Terminal error"
            setTerminalError(errorMsg)
            setTerminalOutput((prev) => prev + `\r\n[Error: ${errorMsg}]\r\n`)
          } else if (typeof msg.data === "string") {
            setTerminalOutput((prev) => prev + msg.data)
          }
        } catch {
          // If non-JSON raw stream output
          setTerminalOutput((prev) => prev + String(event.data))
        }
      }

      ws.onerror = () => {
        setTerminalError("WebSocket connection error")
        setTerminalOutput(
          (prev) => prev + `\r\n[WebSocket connection error]\r\n`
        )
      }

      ws.onclose = () => {
        setTerminalConnected(false)
        setTerminalOutput((prev) => prev + `\r\n[Session Disconnected]\r\n`)
      }
    } catch (err) {
      const msg =
        err instanceof Error ? err.message : "Failed to open WebSocket"
      setTerminalError(msg)
      setTerminalOutput(
        (prev) => prev + `\r\n[Failed to launch terminal: ${msg}]\r\n`
      )
    }
  }

  const handleDisconnectTerminal = () => {
    if (wsRef.current) {
      try {
        wsRef.current.close()
      } catch {
        // ignore
      }
      wsRef.current = null
    }
    setTerminalConnected(false)
    setIsTerminalActive(false)
  }

  const handleSendTerminalInput = (e?: React.FormEvent) => {
    if (e) e.preventDefault()
    if (!wsRef.current || wsRef.current.readyState !== WebSocket.OPEN) return

    const data = terminalInput + "\n"
    wsRef.current.send(JSON.stringify({ type: "input", data }))
    setTerminalInput("")
  }

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={`SSH Connect - ${stackName}`}
      className={`fixed inset-0 z-50 flex items-center justify-center bg-black/80 backdrop-blur-sm ${
        isFullscreen ? "p-0" : "p-4"
      }`}
    >
      <div
        className={`w-full flex flex-col bg-slate-900 border border-slate-700/80 shadow-2xl overflow-hidden ${
          isFullscreen
            ? "h-screen max-w-none rounded-none border-0"
            : "max-w-4xl max-h-[92vh] rounded-2xl"
        }`}
      >
        {/* Header */}
        <div className="flex items-center justify-between px-6 py-4 border-b border-slate-800 bg-slate-950/70">
          <div className="flex items-center gap-3">
            <div className="p-2 rounded-xl bg-emerald-500/10 border border-emerald-500/20 text-emerald-400">
              <TerminalIcon className="h-5 w-5" />
            </div>
            <div>
              <div className="flex items-center gap-2">
                <h3 className="text-base font-bold text-white font-mono">
                  SSH Connect: {stackName}
                </h3>
                <span className="text-[10px] font-semibold uppercase px-2 py-0.5 rounded-full bg-slate-800 text-slate-300 border border-slate-700">
                  {effectiveProvider}
                </span>
                {effectiveTenant && (
                  <span className="text-[10px] font-mono px-2 py-0.5 rounded-full bg-indigo-500/10 text-indigo-300 border border-indigo-500/20">
                    tenant: {effectiveTenant}
                  </span>
                )}
              </div>
              <p className="text-xs text-slate-400 mt-0.5">
                Target VM connection info, direct SSH commands, and optional
                in-browser web terminal
              </p>
            </div>
          </div>

          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={() => setIsFullscreen((prev) => !prev)}
              aria-label={
                isFullscreen ? "Exit fullscreen" : "Expand fullscreen"
              }
              className="p-1.5 rounded-lg text-slate-400 hover:text-slate-200 hover:bg-slate-800 transition-colors"
            >
              {isFullscreen ? (
                <Minimize2 className="h-4 w-4" />
              ) : (
                <Maximize2 className="h-4 w-4" />
              )}
            </button>
            <button
              type="button"
              data-testid="vm-ssh-modal-close-btn"
              onClick={onClose}
              aria-label="Close"
              className="p-1.5 rounded-lg text-slate-400 hover:text-slate-200 hover:bg-slate-800 transition-colors"
            >
              <X className="h-4 w-4" />
            </button>
          </div>
        </div>

        {/* Body */}
        <div className="flex-1 overflow-y-auto p-6 space-y-6">
          {loading && (
            <div className="flex items-center justify-center py-12 text-slate-400 gap-2 text-sm">
              <Loader2 className="h-5 w-5 animate-spin text-emerald-400" />
              <span>Resolving VM network endpoints and SSH secrets...</span>
            </div>
          )}

          {error && (
            <div className="p-4 rounded-xl bg-red-500/10 border border-red-500/30 text-red-300 text-xs flex items-start gap-2.5">
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
                  <div>
                    <label
                      htmlFor="ssh-host-display"
                      className="block text-slate-400 mb-1"
                    >
                      Host
                    </label>
                    <input
                      id="ssh-host-display"
                      type="text"
                      value={hostOverride}
                      onChange={(e) => setHostOverride(e.target.value)}
                      data-testid="ssh-host-display"
                      placeholder="e.g. 192.168.1.150"
                      className="w-full bg-slate-900 border border-slate-700 rounded-lg px-2.5 py-1.5 font-mono text-white focus:outline-none focus:ring-1 focus:ring-emerald-500"
                    />
                  </div>

                  <div>
                    <label
                      htmlFor="ssh-user-input"
                      className="block text-slate-400 mb-1"
                    >
                      User
                    </label>
                    <input
                      id="ssh-user-input"
                      type="text"
                      value={userOverride}
                      onChange={(e) => setUserOverride(e.target.value)}
                      data-testid="ssh-user-input"
                      placeholder="e.g. debian"
                      className="w-full bg-slate-900 border border-slate-700 rounded-lg px-2.5 py-1.5 font-mono text-white focus:outline-none focus:ring-1 focus:ring-emerald-500"
                    />
                  </div>

                  <div>
                    <label
                      htmlFor="ssh-port-input"
                      className="block text-slate-400 mb-1"
                    >
                      Port
                    </label>
                    <input
                      id="ssh-port-input"
                      type="number"
                      value={portOverride}
                      onChange={(e) => setPortOverride(e.target.value)}
                      data-testid="ssh-port-input"
                      placeholder="22"
                      className="w-full bg-slate-900 border border-slate-700 rounded-lg px-2.5 py-1.5 font-mono text-white focus:outline-none focus:ring-1 focus:ring-emerald-500"
                    />
                  </div>

                  <div>
                    <span className="block text-slate-400 mb-1">
                      Key Storage
                    </span>
                    <div className="flex items-center gap-1.5 py-1.5">
                      {sshInfo?.hasSavedSshKey ? (
                        <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded-full text-[11px] bg-emerald-500/10 text-emerald-400 border border-emerald-500/20 font-medium">
                          <Check className="h-3 w-3" />
                          Saved in Secret
                        </span>
                      ) : (
                        <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded-full text-[11px] bg-amber-500/10 text-amber-400 border border-amber-500/20 font-medium">
                          Ephemeral / Not Saved
                        </span>
                      )}
                    </div>
                  </div>
                </div>

                {/* Collapsible Private Key Input */}
                <div className="pt-2 border-t border-slate-800/80">
                  <div className="flex items-center justify-between">
                    <button
                      type="button"
                      onClick={() => setShowPrivateKeyInput((v) => !v)}
                      className="text-xs text-indigo-400 hover:text-indigo-300 font-medium flex items-center gap-1.5 transition-colors cursor-pointer"
                    >
                      <Key className="h-3.5 w-3.5" />
                      <span>
                        {showPrivateKeyInput
                          ? "Hide Ephemeral Private Key"
                          : "Provide Ephemeral Private Key (Optional)"}
                      </span>
                    </button>
                    {privateKey.trim() && (
                      <span className="text-[11px] text-emerald-400 font-mono">
                        Key provided ({privateKey.trim().length} chars)
                      </span>
                    )}
                  </div>
                  {showPrivateKeyInput && (
                    <div className="mt-2">
                      <textarea
                        data-testid="ssh-private-key-input"
                        rows={4}
                        value={privateKey}
                        onChange={(e) => setPrivateKey(e.target.value)}
                        placeholder="-----BEGIN OPENSSH PRIVATE KEY-----&#10;...&#10;-----END OPENSSH PRIVATE KEY-----"
                        className="w-full bg-slate-900 border border-slate-800 rounded-lg p-2.5 font-mono text-xs text-slate-200 placeholder-slate-600 focus:outline-none focus:ring-1 focus:ring-emerald-500"
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

                {/* Standard SSH Command */}
                <div className="bg-slate-950 border border-slate-800 rounded-xl p-3 space-y-1.5">
                  <div className="flex items-center justify-between text-xs text-slate-400">
                    <span className="font-medium">1. Standard SSH Client</span>
                    <button
                      type="button"
                      data-testid="copy-ssh-raw-btn"
                      onClick={() => handleCopy(rawSshCommand, "ssh-raw")}
                      className="flex items-center gap-1 text-[11px] text-slate-300 hover:text-white px-2 py-0.5 rounded bg-slate-800 hover:bg-slate-700 transition-colors"
                    >
                      {copiedKey === "ssh-raw" ? (
                        <>
                          <Check className="h-3 w-3 text-emerald-400" />
                          <span className="text-emerald-400">Copied!</span>
                        </>
                      ) : (
                        <>
                          <Copy className="h-3 w-3" />
                          <span>Copy</span>
                        </>
                      )}
                    </button>
                  </div>
                  <pre
                    data-testid="ssh-raw-command"
                    className="font-mono text-xs text-emerald-400 bg-slate-900/90 rounded-lg p-2.5 overflow-x-auto select-all"
                  >
                    {rawSshCommand}
                  </pre>
                </div>

                {/* Keel CLI Print Command */}
                <div className="bg-slate-950 border border-slate-800 rounded-xl p-3 space-y-1.5">
                  <div className="flex items-center justify-between text-xs text-slate-400">
                    <span className="font-medium">
                      2. Keel CLI (Print Connection Info)
                    </span>
                    <button
                      type="button"
                      data-testid="copy-cli-print-btn"
                      onClick={() => handleCopy(cliPrintCommand, "cli-print")}
                      className="flex items-center gap-1 text-[11px] text-slate-300 hover:text-white px-2 py-0.5 rounded bg-slate-800 hover:bg-slate-700 transition-colors"
                    >
                      {copiedKey === "cli-print" ? (
                        <>
                          <Check className="h-3 w-3 text-emerald-400" />
                          <span className="text-emerald-400">Copied!</span>
                        </>
                      ) : (
                        <>
                          <Copy className="h-3 w-3" />
                          <span>Copy</span>
                        </>
                      )}
                    </button>
                  </div>
                  <pre
                    data-testid="ssh-cli-print-command"
                    className="font-mono text-xs text-indigo-300 bg-slate-900/90 rounded-lg p-2.5 overflow-x-auto select-all"
                  >
                    {cliPrintCommand}
                  </pre>
                </div>

                {/* Keel CLI Interactive Exec Command */}
                <div className="bg-slate-950 border border-slate-800 rounded-xl p-3 space-y-1.5">
                  <div className="flex items-center justify-between text-xs text-slate-400">
                    <span className="font-medium">
                      3. Keel CLI (Interactive Direct Exec)
                    </span>
                    <button
                      type="button"
                      data-testid="copy-cli-exec-btn"
                      onClick={() => handleCopy(cliExecCommand, "cli-exec")}
                      className="flex items-center gap-1 text-[11px] text-slate-300 hover:text-white px-2 py-0.5 rounded bg-slate-800 hover:bg-slate-700 transition-colors"
                    >
                      {copiedKey === "cli-exec" ? (
                        <>
                          <Check className="h-3 w-3 text-emerald-400" />
                          <span className="text-emerald-400">Copied!</span>
                        </>
                      ) : (
                        <>
                          <Copy className="h-3 w-3" />
                          <span>Copy</span>
                        </>
                      )}
                    </button>
                  </div>
                  <pre
                    data-testid="ssh-cli-exec-command"
                    className="font-mono text-xs text-amber-300 bg-slate-900/90 rounded-lg p-2.5 overflow-x-auto select-all"
                  >
                    {cliExecCommand}
                  </pre>
                </div>
              </div>

              {/* Opt-In In-Browser Terminal Section */}
              <div className="pt-2 border-t border-slate-800/80 space-y-3">
                <div className="flex items-center justify-between">
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
                    <button
                      type="button"
                      data-testid="launch-browser-terminal-btn"
                      onClick={handleLaunchTerminal}
                      className="inline-flex items-center gap-1.5 px-3.5 py-1.5 rounded-lg text-xs font-semibold bg-emerald-600 hover:bg-emerald-500 text-white transition-colors cursor-pointer shadow-lg shadow-emerald-900/30"
                    >
                      <Play className="h-3.5 w-3.5 fill-current" />
                      <span>Launch Browser Terminal</span>
                    </button>
                  ) : (
                    <button
                      type="button"
                      data-testid="disconnect-browser-terminal-btn"
                      onClick={handleDisconnectTerminal}
                      className="inline-flex items-center gap-1.5 px-3 py-1.5 rounded-lg text-xs font-semibold bg-red-600/20 hover:bg-red-600/30 text-red-300 border border-red-500/30 transition-colors cursor-pointer"
                    >
                      <Square className="h-3.5 w-3.5 fill-current" />
                      <span>Disconnect Terminal</span>
                    </button>
                  )}
                </div>

                {isTerminalActive && (
                  <div className="border border-slate-800 rounded-xl bg-slate-950 overflow-hidden shadow-2xl">
                    <div className="flex items-center justify-between px-3 py-1.5 bg-slate-900 border-b border-slate-800 text-[11px] text-slate-400 font-mono">
                      <div className="flex items-center gap-2">
                        <span
                          className={`h-2 w-2 rounded-full ${
                            terminalConnected
                              ? "bg-emerald-400 animate-pulse"
                              : "bg-amber-400"
                          }`}
                        />
                        <span>
                          {effectiveUser}@{effectiveHost}:{effectivePort}
                        </span>
                      </div>
                      <span>
                        {terminalConnected ? "Connected" : "Connecting..."}
                      </span>
                    </div>

                    {/* Terminal Output Viewport */}
                    <div
                      data-testid="browser-ssh-terminal"
                      ref={terminalScrollRef}
                      className="p-3 font-mono text-xs text-slate-200 bg-slate-950 h-64 overflow-y-auto whitespace-pre-wrap select-text leading-relaxed"
                    >
                      {terminalOutput}
                      {terminalError && (
                        <span className="text-red-400">
                          {`\r\n`}[Error: {terminalError}]
                        </span>
                      )}
                    </div>

                    {/* Command Input Row */}
                    <form
                      onSubmit={handleSendTerminalInput}
                      className="flex items-center border-t border-slate-800 bg-slate-900/80 px-3 py-2 gap-2"
                    >
                      <span className="text-emerald-400 font-mono text-xs font-bold">
                        $
                      </span>
                      <input
                        type="text"
                        data-testid="terminal-command-input"
                        value={terminalInput}
                        onChange={(e) => setTerminalInput(e.target.value)}
                        placeholder="Type a command and press Enter..."
                        className="flex-1 bg-transparent font-mono text-xs text-white placeholder-slate-500 focus:outline-none"
                      />
                      <button
                        type="submit"
                        data-testid="terminal-send-btn"
                        disabled={!terminalConnected}
                        className="px-2.5 py-1 rounded text-[11px] font-medium bg-emerald-600/30 hover:bg-emerald-600/40 text-emerald-300 border border-emerald-500/30 disabled:opacity-40 disabled:cursor-not-allowed"
                      >
                        Send
                      </button>
                    </form>
                  </div>
                )}
              </div>
            </>
          )}
        </div>
      </div>
    </div>
  )
}
