import React from "react"

import { PreviewSource } from "../../types/api"
import { Badge } from "../ui/badge"

/** Provenance of a preview report: executed PKO `pulumi preview` vs. control-plane synthesis. */
export const PreviewSourceBadge: React.FC<{ source?: PreviewSource }> = ({
  source,
}) => {
  if (source === "pko") {
    return (
      <Badge
        variant="success"
        data-testid="preview-source-badge"
        data-source="pko"
        title="Diff produced by an ephemeral PKO v2 Stack (spec.preview: true)"
      >
        PKO preview
      </Badge>
    )
  }
  if (source === "synthesized") {
    return (
      <Badge
        variant="warning"
        data-testid="preview-source-badge"
        data-source="synthesized"
        title="Diff synthesized from the proposed manifests; no pulumi preview was executed"
      >
        Synthesized (not executed)
      </Badge>
    )
  }
  return (
    <Badge
      variant="secondary"
      data-testid="preview-source-badge"
      data-source="unknown"
      title="The server did not report how this preview was produced"
    >
      Unknown source
    </Badge>
  )
}

/** Section title reflecting the preview kind (UI-14). */
export function previewTitle(source?: PreviewSource): string {
  return source === "pko" ? "PKO Preview" : "Preview"
}
