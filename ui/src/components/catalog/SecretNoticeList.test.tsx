import { render, screen } from "@testing-library/react"
import { describe, expect, it } from "vitest"

import { SecretNoticeList } from "./SecretNoticeList"
import { ProposalToast } from "../shell/ProposalToast"
import { ProposalCreatedNotice } from "./ProposalCreatedNotice"
import { Proposal, SecretNotice } from "@/types/api"

const notices: SecretNotice[] = [
  {
    path: "auth.password",
    reason: "heuristicSecret",
    message: "looks like a secret; stored encrypted with SOPS",
  },
  {
    path: "ingress.tlsSecret",
    reason: "markedNotSecret",
    message: "marked not-secret by you; stored in plaintext",
  },
]

const proposal: Proposal = {
  id: "9",
  title: "Deploy db",
  branch: "keel/default/db",
  tenant: "default",
  scope: "default",
  status: "open",
  createdAt: "now",
  secretNotices: notices,
}

describe("SecretNoticeList", () => {
  it("renders nothing without notices", () => {
    const { container } = render(<SecretNoticeList notices={[]} />)
    expect(container).toBeEmptyDOMElement()
    render(<SecretNoticeList />)
    expect(screen.queryByTestId("secret-notices")).toBeNull()
  })

  it("lists each path with its reason", () => {
    render(<SecretNoticeList notices={notices} />)
    const items = screen.getAllByRole("listitem")
    expect(items).toHaveLength(2)
    expect(items[0]).toHaveTextContent(
      "auth.password: looks like a secret; stored encrypted with SOPS"
    )
    expect(items[1]).toHaveAttribute("data-reason", "markedNotSecret")
  })

  it("is shown by both proposal-created notifications", () => {
    const { unmount } = render(
      <ProposalToast
        proposal={proposal}
        onDismiss={() => {}}
        onOpen={() => {}}
      />
    )
    expect(screen.getByTestId("secret-notices")).toHaveTextContent(
      "ingress.tlsSecret"
    )
    unmount()
    render(<ProposalCreatedNotice proposal={proposal} onDismiss={() => {}} />)
    expect(screen.getByTestId("secret-notices")).toHaveTextContent(
      "auth.password"
    )
  })
})
