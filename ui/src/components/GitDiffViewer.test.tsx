import { render, screen } from "@testing-library/react"
import { describe, it, expect } from "vitest"
import { GitDiffViewer } from "./GitDiffViewer"

describe("GitDiffViewer component", () => {
  it("renders diff headers, hunks, additions, deletions, and YAML syntax tokens", () => {
    const sampleDiff = [
      "diff --git a/tenants/acme/values.yaml b/tenants/acme/values.yaml",
      "index 1111111..2222222 100644",
      "--- a/tenants/acme/values.yaml",
      "+++ b/tenants/acme/values.yaml",
      "@@ -1,5 +1,6 @@",
      "# comment line",
      "replicaCount: 1",
      "-enabled: false",
      "+enabled: true",
      '+image: "ghcr.io/stefanprodan/podinfo:6.5.0"',
      "+plain unkeyed line",
      "-# removed comment",
    ].join("\n")

    render(<GitDiffViewer diff={sampleDiff} />)

    const viewer = screen.getByTestId("git-diff-viewer")
    expect(viewer).toBeInTheDocument()
    expect(
      screen.getByText(
        "diff --git a/tenants/acme/values.yaml b/tenants/acme/values.yaml"
      )
    ).toBeInTheDocument()
    expect(screen.getByText("@@ -1,5 +1,6 @@")).toBeInTheDocument()
    expect(screen.getByText("# comment line")).toBeInTheDocument()
    expect(screen.getByText("replicaCount")).toBeInTheDocument()
    expect(screen.getByText("plain unkeyed line")).toBeInTheDocument()
    expect(screen.getByText("# removed comment")).toBeInTheDocument()
  })

  it("renders single-quoted YAML string values and multiple diff blocks", () => {
    const multiDiff = [
      "diff --git a/a.yaml b/a.yaml",
      "new file mode 100644",
      "+name: 'keel'",
      "diff --git a/b.yaml b/b.yaml",
      "deleted file mode 100644",
      "-count: 42",
    ].join("\n")

    render(<GitDiffViewer diff={multiDiff} />)

    expect(screen.getByText("diff --git a/a.yaml b/a.yaml")).toBeInTheDocument()
    expect(screen.getByText("diff --git a/b.yaml b/b.yaml")).toBeInTheDocument()
    expect(screen.getByText("'keel'", { exact: false })).toBeInTheDocument()
    expect(screen.getByText("42", { exact: false })).toBeInTheDocument()
  })
})
