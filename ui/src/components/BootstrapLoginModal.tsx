import React, { useState } from "react"
import { Shield, Key, AlertCircle, Loader2, X } from "lucide-react"
import { bootstrapLogin } from "../lib/api"

interface BootstrapLoginModalProps {
  isOpen: boolean
  onSuccess: () => void
  onClose?: () => void
}

export const BootstrapLoginModal: React.FC<BootstrapLoginModalProps> = ({
  isOpen,
  onSuccess,
  onClose,
}) => {
  const [token, setToken] = useState("")
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  if (!isOpen) return null

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault()
    if (!token.trim()) {
      setError("Please enter a valid Stage 0 bootstrap token")
      return
    }

    setError(null)
    setLoading(true)

    try {
      const res = await bootstrapLogin(token.trim())
      if (res.authenticated || res.success) {
        onSuccess()
      } else {
        setError(res.message || "Invalid bootstrap token")
      }
    } catch (err) {
      setError(
        err instanceof Error
          ? err.message
          : "Failed to authenticate bootstrap token"
      )
    } finally {
      setLoading(false)
    }
  }

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="bootstrap-modal-title"
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/70 backdrop-blur-sm p-4"
    >
      <div className="w-full max-w-md bg-slate-900 border border-slate-800 rounded-2xl shadow-2xl overflow-hidden animate-in fade-in zoom-in-95 duration-200">
        {/* Header */}
        <div className="flex items-center justify-between px-6 py-4 border-b border-slate-800 bg-slate-900/50">
          <div className="flex items-center space-x-3">
            <div className="p-2 bg-indigo-500/10 rounded-lg text-indigo-400 border border-indigo-500/20">
              <Shield className="h-5 w-5" />
            </div>
            <div>
              <h2
                id="bootstrap-modal-title"
                className="text-base font-semibold text-white tracking-tight"
              >
                Stage 0 Bootstrap Auth
              </h2>
              <p className="text-xs text-slate-400">
                256-bit CLI Single-Use Token Login
              </p>
            </div>
          </div>
          {onClose && (
            <button
              onClick={onClose}
              aria-label="Close modal"
              className="p-1 text-slate-400 hover:text-white rounded-lg hover:bg-slate-800 transition-colors"
            >
              <X className="h-5 w-5" />
            </button>
          )}
        </div>

        {/* Body */}
        <form onSubmit={handleSubmit} className="p-6 space-y-4">
          <div className="space-y-2">
            <label
              htmlFor="bootstrap-token-input"
              className="block text-xs font-medium text-slate-300"
            >
              Bootstrap Token
            </label>
            <div className="relative">
              <div className="absolute inset-y-0 left-0 pl-3 flex items-center pointer-events-none text-slate-500">
                <Key className="h-4 w-4" />
              </div>
              <input
                id="bootstrap-token-input"
                type="password"
                value={token}
                onChange={(e) => setToken(e.target.value)}
                placeholder="Enter 256-bit token generated at initialization..."
                className="w-full pl-10 pr-4 py-2 bg-slate-950 border border-slate-800 rounded-lg text-sm text-slate-100 placeholder-slate-600 focus:outline-none focus:ring-2 focus:ring-indigo-500 focus:border-transparent font-mono"
                disabled={loading}
              />
            </div>
            <p className="text-[11px] text-slate-500">
              Token stored in{" "}
              <code className="text-slate-400">
                keel-system/bootstrap-token
              </code>
              . Invalidated upon OIDC activation.
            </p>
          </div>

          {error && (
            <div
              role="alert"
              className="flex items-start space-x-2 p-3 bg-red-950/40 border border-red-500/30 rounded-lg text-red-300 text-xs"
            >
              <AlertCircle className="h-4 w-4 text-red-400 shrink-0 mt-0.5" />
              <span>{error}</span>
            </div>
          )}

          <div className="flex items-center justify-end space-x-3 pt-2">
            {onClose && (
              <button
                type="button"
                onClick={onClose}
                className="px-4 py-2 text-xs font-medium text-slate-400 hover:text-slate-200 transition-colors"
                disabled={loading}
              >
                Cancel
              </button>
            )}
            <button
              type="submit"
              disabled={loading}
              className="flex items-center justify-center space-x-2 px-5 py-2 bg-indigo-600 hover:bg-indigo-500 text-white rounded-lg text-xs font-medium transition-colors disabled:opacity-50 disabled:cursor-not-allowed shadow-lg shadow-indigo-600/20"
            >
              {loading ? (
                <>
                  <Loader2 className="h-4 w-4 animate-spin" />
                  <span>Authenticating...</span>
                </>
              ) : (
                <span>Authenticate Token</span>
              )}
            </button>
          </div>
        </form>
      </div>
    </div>
  )
}
