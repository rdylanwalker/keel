import React from "react"
import {
  AlertTriangle,
  ArrowRight,
  CheckCircle2,
  Clock,
  ExternalLink,
  MinusCircle,
} from "lucide-react"

import {
  CreateTenantResult,
  Proposal,
  ProposalRef,
  TenantStepOutcome,
} from "../../types/api"
import { safeHttpUrl } from "../../lib/forgeLinks"
import { Button } from "../ui/button"

export type StepTone = "done" | "pending" | "warning" | "skipped"

export interface ProvisioningStep {
  id: string
  label: string
  tone: StepTone
  detail: React.ReactNode
}

const TONE_ICON: Record<StepTone, React.ReactNode> = {
  done: <CheckCircle2 className="h-4 w-4 text-emerald-400" aria-hidden />,
  pending: <Clock className="h-4 w-4 text-amber-300" aria-hidden />,
  warning: <AlertTriangle className="h-4 w-4 text-amber-400" aria-hidden />,
  skipped: <MinusCircle className="h-4 w-4 text-slate-500" aria-hidden />,
}

const TONE_LABEL: Record<StepTone, string> = {
  done: "done",
  pending: "pending",
  warning: "needs attention",
  skipped: "not applicable",
}

/** Human description of the tenant repository read credential (never its value). */
export function credentialKind(transport?: string): string {
  if (transport === "ssh") return "Read-only SSH deploy key"
  if (transport === "https") return "Repository-scoped HTTPS read token"
  return "Repository read credential"
}

function runtimeStep(
  outcome: TenantStepOutcome,
  namespace: string,
  what: string
): { tone: StepTone; detail: string } {
  switch (outcome) {
    case "pendingNamespace":
      return {
        tone: "pending",
        detail: `Written to ${namespace} once the platform PR is merged and the namespace exists.`,
      }
    case "created":
      return { tone: "done", detail: `${what} created in ${namespace}.` }
    case "existed":
      return {
        tone: "done",
        detail: `${what} already present in ${namespace}.`,
      }
    case "rotated":
      return {
        tone: "done",
        detail: `An unusable ${what.toLowerCase()} in ${namespace} was replaced.`,
      }
    case "notApplicable":
      return { tone: "skipped", detail: "Not used by this layout." }
  }
}

/** Ordered provisioning steps of a `POST /api/v1/tenants` result. */
export function tenantProvisioningSteps(
  result: CreateTenantResult
): ProvisioningStep[] {
  const { repository: repo, tenant } = result
  const namespace = tenant.namespace || `tenant-${tenant.id}`
  const steps: ProvisioningStep[] = []

  if (repo.layout === "monorepo") {
    steps.push({
      id: "repository",
      label: "Repository",
      tone: "warning",
      detail:
        "Shared platform repository (Local Git provider, development only): no per-tenant repository isolation.",
    })
  } else {
    const fullName =
      repo.owner && repo.name ? `${repo.owner}/${repo.name}` : repo.name
    steps.push({
      id: "repository",
      label: "Tenant repository",
      tone: "done",
      detail: (
        <>
          <code className="font-mono text-slate-200">
            {fullName ?? "tenant repository"}
          </code>{" "}
          {repo.created === true
            ? "created."
            : repo.created === false
              ? "already existed and was adopted."
              : "ready."}
          {repo.url && (
            <span className="block truncate font-mono text-[10px] text-slate-500">
              Flux/PKO source: {repo.url}
            </span>
          )}
        </>
      ),
    })
    if (repo.mainProtection) {
      const unsupported = repo.mainProtection === "unsupportedPlan"
      steps.push({
        id: "protection",
        label: "Branch protection on main",
        tone: unsupported ? "warning" : "done",
        detail: unsupported
          ? "The forge plan does not offer branch protection for this repository; direct pushes to main are not blocked by the forge."
          : repo.mainProtection === "unchanged"
            ? "Already protected with Keel's settings."
            : "Protected: changes land through reviewed PRs only.",
      })
    }
    if (repo.forks) {
      const needsAdmin = repo.forks.result === "requiresAdminSetting"
      steps.push({
        id: "forks",
        label: "Forking disabled",
        tone: needsAdmin ? "warning" : "done",
        detail: needsAdmin
          ? (repo.forks.detail ??
            "Forking is governed by an instance or organisation setting.")
          : repo.forks.result === "alreadyDisabled"
            ? "Forking was already disabled."
            : "Forking disabled for the repository.",
      })
    }
    const credential = runtimeStep(
      repo.cluster.credential,
      namespace,
      "Credentials Secret keel-gitops-credentials"
    )
    steps.push({
      id: "credential",
      label: credentialKind(repo.transport),
      tone: credential.tone,
      detail: `Single-repository, read-only. ${credential.detail}`,
    })
  }

  const sops = runtimeStep(
    repo.cluster.sopsAge,
    namespace,
    "SOPS age key Secret sops-age"
  )
  steps.push({
    id: "sops",
    label: "SOPS age key",
    tone: sops.tone,
    detail: sops.detail,
  })
  return steps
}

const ProposalLink: React.FC<{
  label: string
  proposal: Proposal
  onNavigateToPreview?: (ref: ProposalRef) => void
}> = ({ label, proposal, onNavigateToPreview }) => {
  const prUrl = safeHttpUrl(proposal.prUrl)
  return (
    <li className="flex flex-wrap items-center justify-between gap-2 rounded-lg border border-slate-800 bg-slate-950/60 p-2.5">
      <div className="min-w-0">
        <p className="text-xs font-semibold text-white">
          {label}: PR #{proposal.id}
        </p>
        <p className="truncate font-mono text-[10px] text-slate-500">
          {proposal.branch}
        </p>
      </div>
      <div className="flex shrink-0 items-center gap-2">
        {prUrl && (
          <Button asChild variant="secondary" size="sm">
            <a href={prUrl} target="_blank" rel="noreferrer">
              <ExternalLink className="h-3.5 w-3.5" />
              <span>Forge</span>
            </a>
          </Button>
        )}
        {onNavigateToPreview && (
          <Button
            size="sm"
            className="bg-purple-600 hover:bg-purple-500 shadow-none"
            onClick={() =>
              onNavigateToPreview({ scope: proposal.scope, id: proposal.id })
            }
          >
            <span>Go to PR</span>
            <ArrowRight className="h-3.5 w-3.5" />
          </Button>
        )}
      </div>
    </li>
  )
}

/** Result panel of the tenant creation dialog: repository, credential and runtime steps. */
export const TenantProvisioningSummary: React.FC<{
  result: CreateTenantResult
  onNavigateToPreview?: (ref: ProposalRef) => void
}> = ({ result, onNavigateToPreview }) => {
  const steps = tenantProvisioningSteps(result)
  const repoWebUrl = safeHttpUrl(result.repository.webUrl)
  const skeleton = result.repository.skeletonProposal
  return (
    <div data-testid="tenant-provisioning-summary" className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-xs text-slate-300">
          Tenant{" "}
          <code className="font-mono text-white">{result.tenant.id}</code>{" "}
          provisioned in{" "}
          <code className="font-mono text-white">
            {result.tenant.namespace}
          </code>
        </p>
        {repoWebUrl && (
          <Button asChild variant="secondary" size="sm">
            <a
              href={repoWebUrl}
              target="_blank"
              rel="noreferrer"
              aria-label="Open tenant repository"
            >
              <ExternalLink className="h-3.5 w-3.5" />
              <span>Open repository</span>
            </a>
          </Button>
        )}
      </div>

      <ol className="space-y-2" aria-label="Provisioning steps">
        {steps.map((step) => (
          <li
            key={step.id}
            data-testid={`tenant-step-${step.id}`}
            data-tone={step.tone}
            className="flex items-start gap-2.5 rounded-lg border border-slate-800 bg-slate-950/60 p-2.5"
          >
            <span className="mt-0.5 shrink-0" title={TONE_LABEL[step.tone]}>
              {TONE_ICON[step.tone]}
            </span>
            <div className="min-w-0 text-xs">
              <p className="font-semibold text-white">
                {step.label}{" "}
                <span className="sr-only">({TONE_LABEL[step.tone]})</span>
              </p>
              <div className="break-words text-slate-400">{step.detail}</div>
            </div>
          </li>
        ))}
      </ol>

      <div className="space-y-2">
        <h4 className="text-[11px] font-semibold uppercase tracking-wider text-slate-400">
          Pull requests to merge
        </h4>
        <ul className="space-y-2">
          {skeleton && (
            <ProposalLink
              label={
                result.proposal
                  ? "Tenant repository skeleton (merge first)"
                  : "Tenant repository skeleton"
              }
              proposal={skeleton}
              onNavigateToPreview={onNavigateToPreview}
            />
          )}
          {result.proposal ? (
            <ProposalLink
              label="Platform tenant stub"
              proposal={result.proposal}
              onNavigateToPreview={onNavigateToPreview}
            />
          ) : (
            <li className="rounded-lg border border-slate-800 bg-slate-950/60 p-2.5 text-xs text-slate-400">
              The platform tenant stub is already on main; no platform PR was
              needed.
            </li>
          )}
        </ul>
      </div>

      {result.repository.notes.length > 0 && (
        <ul
          data-testid="tenant-provisioning-notes"
          className="list-disc space-y-1 pl-5 text-[11px] text-amber-200/90"
        >
          {result.repository.notes.map((note) => (
            <li key={note}>{note}</li>
          ))}
        </ul>
      )}
    </div>
  )
}
