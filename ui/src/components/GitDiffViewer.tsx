import React from "react"

interface GitDiffViewerProps {
  diff: string
}

function highlightYamlContent(content: string, tone: "add" | "del" | "ctx") {
  const trimmed = content.trimStart()
  if (trimmed.startsWith("#")) {
    return <span className="text-slate-500 italic">{content}</span>
  }

  const keyValMatch = content.match(/^(\s*(?:-\s+)?)([A-Za-z0-9_.-]+)(:)(.*)$/)
  if (!keyValMatch) {
    return <span>{content}</span>
  }

  const [, indent, key, colon, rest] = keyValMatch
  const keyClass =
    tone === "add"
      ? "text-emerald-200 font-medium"
      : tone === "del"
        ? "text-rose-200 font-medium"
        : "text-sky-300 font-medium"

  const trimmedRest = rest.trim()
  let valNode: React.ReactNode = rest
  if (
    trimmedRest === "true" ||
    trimmedRest === "false" ||
    trimmedRest === "null" ||
    /^-?\d+(\.\d+)?$/.test(trimmedRest)
  ) {
    valNode = <span className="text-amber-300">{rest}</span>
  } else if (
    (trimmedRest.startsWith('"') && trimmedRest.endsWith('"')) ||
    (trimmedRest.startsWith("'") && trimmedRest.endsWith("'"))
  ) {
    valNode = <span className="text-emerald-200">{rest}</span>
  }

  return (
    <>
      <span>{indent}</span>
      <span className={keyClass}>{key}</span>
      <span className="text-slate-500">{colon}</span>
      {valNode}
    </>
  )
}

export const GitDiffViewer: React.FC<GitDiffViewerProps> = ({ diff }) => {
  const lines = diff.split("\n")

  return (
    <div
      data-testid="git-diff-viewer"
      className="bg-slate-950 border border-slate-800 rounded-xl overflow-x-auto max-h-80 overflow-y-auto text-xs font-mono leading-relaxed py-2"
    >
      {lines.map((line, idx) => {
        if (line.startsWith("diff --git ")) {
          return (
            <div
              key={idx}
              className={`px-3 py-1.5 bg-slate-900/90 text-indigo-300 font-semibold border-y border-slate-800 whitespace-pre ${
                idx > 0 ? "mt-2" : ""
              }`}
            >
              {line}
            </div>
          )
        }

        if (
          line.startsWith("index ") ||
          line.startsWith("new file mode ") ||
          line.startsWith("deleted file mode ") ||
          line.startsWith("--- ") ||
          line.startsWith("+++ ")
        ) {
          return (
            <div key={idx} className="px-3 text-slate-500 whitespace-pre">
              {line}
            </div>
          )
        }

        if (line.startsWith("@@")) {
          return (
            <div
              key={idx}
              className="px-3 py-0.5 bg-cyan-500/10 text-cyan-300 whitespace-pre"
            >
              {line}
            </div>
          )
        }

        if (line.startsWith("+")) {
          return (
            <div
              key={idx}
              className="px-3 bg-emerald-500/10 text-emerald-300 border-l-2 border-emerald-500/60 whitespace-pre"
            >
              <span className="select-none text-emerald-400 mr-1">+</span>
              {highlightYamlContent(line.slice(1), "add")}
            </div>
          )
        }

        if (line.startsWith("-")) {
          return (
            <div
              key={idx}
              className="px-3 bg-rose-500/10 text-rose-300 border-l-2 border-rose-500/60 whitespace-pre"
            >
              <span className="select-none text-rose-400 mr-1">-</span>
              {highlightYamlContent(line.slice(1), "del")}
            </div>
          )
        }

        return (
          <div key={idx} className="px-3 text-slate-300 whitespace-pre">
            {highlightYamlContent(line, "ctx")}
          </div>
        )
      })}
    </div>
  )
}
