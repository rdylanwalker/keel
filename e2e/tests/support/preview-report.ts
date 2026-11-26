/**
 * Classifies a `GET /api/v1/tenants/<tenant>/proposals/<id>/preview` response
 * (`ProposalPreviewResponse`, camelCase) for live specs that must prove a real PKO preview ran.
 *
 * Every response carries a `previewKey` and a `report`, so matching on the raw body text accepts
 * failed or never-executed previews. This helper inspects `report.source`, `report.run.phase` and
 * `report.resources` instead:
 *
 * - `pending` / `running` (or no run yet) -> keep polling;
 * - `succeeded` from `source: "pko"` with at least one actionable resource change -> done;
 * - `failed` / `notExecuted`, or a final report that is not a real PKO diff -> fail fast.
 */

/** Wire shape of the fields read from `ProposalPreviewResponse.report` (`keel-pko` `PreviewReport`). */
export interface PreviewReportWire {
  source?: string;
  run?: { phase?: string; message?: string };
  resources?: { op?: string }[];
}

export type PreviewVerdict =
  | { state: 'waiting'; status: string }
  | { state: 'succeeded'; status: string; changes: number }
  | { state: 'failed'; status: string; reason: string };

/** Upper bound on the server-provided failure message echoed into test output. */
const MAX_REASON_CHARS = 500;

/** `ChangeOp` values (lowercase on the wire) that represent an actual change. */
const ACTIONABLE_OPS = new Set(['create', 'update', 'delete', 'replace', 'drift']);

function describeReason(phase: string, message: string | undefined): string {
  const trimmed = message?.trim();
  if (!trimmed) return `phase ${phase} (no reason reported)`;
  const clipped =
    trimmed.length > MAX_REASON_CHARS ? `${trimmed.slice(0, MAX_REASON_CHARS)}...` : trimmed;
  return `phase ${phase}: ${clipped}`;
}

/** Classifies a parsed preview response body (or a bare report, like spec 07's `b.report ?? b`). */
export function classifyPreviewResponse(body: unknown): PreviewVerdict {
  const wrapper = (body ?? {}) as { report?: PreviewReportWire };
  const report: PreviewReportWire = wrapper.report ?? (body as PreviewReportWire) ?? {};
  const source = report.source ?? 'unknown';
  const phase = report.run?.phase ?? 'unknown';
  const status = `${source}/${phase}`;

  if (phase === 'failed' || phase === 'notExecuted') {
    return { state: 'failed', status, reason: describeReason(phase, report.run?.message) };
  }
  if (phase !== 'succeeded') {
    return { state: 'waiting', status };
  }
  if (source !== 'pko') {
    return {
      state: 'failed',
      status,
      reason: `phase succeeded but report source is '${source}', not a real PKO preview`,
    };
  }
  const changes = (report.resources ?? []).filter((r) => ACTIONABLE_OPS.has(r.op ?? '')).length;
  if (changes === 0) {
    return {
      state: 'failed',
      status,
      // Keep the server's explanation (e.g. the plan could not be read from the workspace logs).
      reason: describeReason(phase, report.run?.message ?? 'reported no resource changes'),
    };
  }
  return { state: 'succeeded', status, changes };
}
