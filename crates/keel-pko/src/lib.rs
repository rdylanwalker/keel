//! `keel-pko`
//!
//! Pulumi Kubernetes Operator (PKO v2) `Stack` & Flux CD `GitRepository` synthesis engine,
//! `pulumi preview --json` diff parser & Markdown reporter, and ephemeral PR preview garbage-collection planner.

pub mod gc;
pub mod preview;
pub mod stack;

pub use gc::{
    GcPlan, PreviewResourceCandidate, PreviewResourceKind, ResourceDeletionTarget,
    plan_orphan_preview_gc, plan_preview_gc,
};
pub use preview::{
    ChangeOp, PreviewReport, PreviewSummary, ResourceDiff, format_markdown_report,
    parse_preview_json, parse_preview_value,
};
pub use stack::{
    CrObjectMeta, DEFAULT_FLUX_POLL_INTERVAL, DEFAULT_MANAGEMENT_SERVICE_ACCOUNT,
    DEFAULT_PREVIEW_SERVICE_ACCOUNT, EphemeralPreviewParams, FLUX_GIT_REPO_API_VERSION,
    FLUX_GIT_REPO_KIND, FluxGitRepository, FluxGitRepositoryRef, FluxGitRepositorySpec,
    HelmValuesFromRef, IN_CLUSTER_S3_BACKEND_URL, KEEL_PREVIEWS_NAMESPACE, LocalObjectReference,
    MANAGEMENT_CORE_STACK_NAME, ManagementCoreStackParams, PKO_STACK_API_VERSION, PKO_STACK_KIND,
    PkoEnvRef, PkoFluxSource, PkoFluxSourceRef, PkoStack, PkoStackSpec, PkoWorkspaceTemplate,
    PreviewStack, TenantStackParams, attach_s3_credentials_env_refs, build_preview_branch,
    synthesize_ephemeral_preview, synthesize_management_core_stack, synthesize_tenant_stack,
    validate_preview_branch,
};

/// Errors produced by `keel-pko` synthesis and preview diff parsing.
#[derive(Debug, thiserror::Error)]
pub enum PkoError {
    #[error("Synthesis error: {0}")]
    Synthesis(String),
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("Preview diff parse error: {0}")]
    PreviewParse(String),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("YAML serialization error: {0}")]
    Yaml(#[from] serde_yml::Error),
}
