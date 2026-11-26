/**
 * Largest number of UTF-16 code units sent in one SSH `input` frame. keel-server closes the
 * browser terminal on WebSocket messages over 64 KiB (`SSH_WS_MAX_MESSAGE_BYTES` in
 * `crates/keel-server/src/ssh.rs`); 4096 code units stay below that even when every one is a
 * control character JSON-escaped to six bytes (`\u0000`).
 */
export const TERMINAL_INPUT_CHUNK = 4096

/**
 * Splits terminal input (a large paste) into chunks of at most `size` UTF-16 code units, never
 * between the two halves of a surrogate pair (a lone surrogate is invalid JSON for the server).
 */
export function chunkTerminalInput(
  data: string,
  size: number = TERMINAL_INPUT_CHUNK
): string[] {
  if (data.length <= size) return data ? [data] : []
  const chunks: string[] = []
  let start = 0
  while (start < data.length) {
    let end = Math.min(start + size, data.length)
    const last = data.charCodeAt(end - 1)
    if (
      end < data.length &&
      end - start > 1 &&
      last >= 0xd800 &&
      last <= 0xdbff
    ) {
      end -= 1
    }
    chunks.push(data.slice(start, end))
    start = end
  }
  return chunks
}
