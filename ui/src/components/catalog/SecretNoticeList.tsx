import React from "react"
import { KeyRound } from "lucide-react"

import type { SecretNotice } from "@/types/api"

/**
 * Secret-handling decisions the server reported for a proposal (`secretNotices`): values stored
 * encrypted because their key looks like a secret, and values the user marked not secret.
 * Paths and reasons only; the server never returns the values.
 */
export const SecretNoticeList: React.FC<{ notices?: SecretNotice[] }> = ({
  notices,
}) => {
  if (!notices || notices.length === 0) return null
  return (
    <div
      aria-label="Secret handling"
      data-testid="secret-notices"
      className="rounded-xl border border-amber-500/30 bg-amber-950/30 p-3 text-[11px] text-amber-200 space-y-1.5"
    >
      <p className="flex items-center gap-1.5 font-semibold">
        <KeyRound className="h-3.5 w-3.5 text-amber-400" aria-hidden="true" />
        Secret handling
      </p>
      <ul className="space-y-1">
        {notices.map((notice) => (
          <li
            key={`${notice.path}-${notice.reason}`}
            data-reason={notice.reason}
          >
            <code className="font-mono text-slate-100 break-all">
              {notice.path}
            </code>
            : {notice.message}
          </li>
        ))}
      </ul>
    </div>
  )
}
