/** Forge web links derived from API data (never credentials). */

/**
 * `http(s)` URLs only, without userinfo: forge links never render `javascript:`/`local://`
 * hrefs or embedded credentials.
 */
export function safeHttpUrl(raw?: string | null): string | undefined {
  if (!raw) return undefined
  try {
    const url = new URL(raw)
    if (url.protocol !== "https:" && url.protocol !== "http:") return undefined
    url.username = ""
    url.password = ""
    return url.toString()
  } catch {
    return undefined
  }
}

/**
 * Forge web page of the repository holding a PR, derived from its web URL
 * (Forgejo/Gitea `/pulls/<n>`, GitHub `/pull/<n>`, GitLab `/-/merge_requests/<n>`).
 */
export function repositoryWebUrl(prUrl?: string | null): string | undefined {
  const safe = safeHttpUrl(prUrl)
  if (!safe) return undefined
  const url = new URL(safe)
  const match = /^(.*?)(?:\/-\/merge_requests|\/pulls|\/pull)\/\d+\/?$/.exec(
    url.pathname
  )
  if (!match || !match[1]) return undefined
  return `${url.origin}${match[1]}`
}
