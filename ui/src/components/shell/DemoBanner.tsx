import React from "react"
import { FlaskConical } from "lucide-react"

/** Persistent banner shown whenever `?demo=true` fixture mode is active (UI-9). */
export const DemoBanner: React.FC = () => {
  const exitHref = (() => {
    const url = new URL(window.location.href)
    url.searchParams.delete("demo")
    return `${url.pathname}${url.search}${url.hash}`
  })()
  return (
    <div
      role="status"
      data-testid="demo-data-banner"
      className="sticky top-0 z-50 flex flex-wrap items-center justify-center gap-x-3 gap-y-1 bg-fuchsia-700 px-4 py-1.5 text-center text-[11px] font-semibold text-white"
    >
      <span className="inline-flex items-center gap-1.5 uppercase tracking-widest">
        <FlaskConical className="h-3.5 w-3.5" />
        Demo data
      </span>
      <span className="font-normal text-fuchsia-100">
        Bundled fixtures only — no live API calls; nothing shown reflects a real
        cluster.
      </span>
      <a
        href={exitHref}
        className="underline underline-offset-2 hover:text-fuchsia-100"
      >
        Exit demo
      </a>
    </div>
  )
}
