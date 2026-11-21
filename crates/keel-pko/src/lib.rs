//! `keel-pko`
//!
//! Pulumi Kubernetes Operator (PKO v2) `Stack` & Flux CD `GitRepository` synthesis engine,
//! `pulumi preview --json` diff parser & Markdown reporter, and ephemeral PR preview garbage-collection planner.

pub mod gc;
pub mod preview;
pub mod run;
pub mod stack;

pub use gc::{
    GcPlan, PreviewResourceCandidate, PreviewResourceKind, ResourceDeletionTarget,
    plan_orphan_preview_gc, plan_preview_gc,
};
pub use preview::{
    ChangeOp, PreviewReport, PreviewSource, PreviewSummary, ResourceDiff, format_markdown_report,
    parse_preview_json, parse_preview_value,
};
pub use run::{
    ANNOTATION_HEAD_SHA, ANNOTATION_REPORTED_COMMIT, PREVIEW_LOG_TAIL_LINES,
    PREVIEW_REPORT_CONFIGMAP_KEY, PrLifecycle, PreviewPhase, PreviewReconcilePlan, PreviewRecord,
    PreviewRunStatus, build_pko_preview_report, map_pko_preview_status, normalize_pko_commit,
    parse_pulumi_console_preview, plan_preview_reconcile, select_update_name,
};
pub use stack::{
    CrObjectMeta, DEFAULT_FLUX_POLL_INTERVAL, DEFAULT_MANAGEMENT_SERVICE_ACCOUNT,
    DEFAULT_PREVIEW_SERVICE_ACCOUNT, EphemeralPreviewParams, FLUX_GIT_REPO_API_VERSION,
    FLUX_GIT_REPO_KIND, FluxGitRepository, FluxGitRepositoryRef, FluxGitRepositorySpec,
    HelmValuesFromRef, IN_CLUSTER_S3_BACKEND_URL, KEEL_PREVIEWS_NAMESPACE, LABEL_PROVIDER,
    LocalObjectReference, MANAGEMENT_CORE_STACK_NAME, ManagementCoreStackParams,
    PKO_STACK_API_VERSION, PKO_STACK_KIND, PULUMI_CONFIG_PASSPHRASE_ENV,
    PULUMI_PASSPHRASE_SECRET_KEY, PULUMI_PASSPHRASE_SECRET_NAME, PkoEnvRef, PkoFluxSource,
    PkoFluxSourceRef, PkoStack, PkoStackSpec, PkoWorkspaceTemplate, PreviewSecretCopy,
    PreviewStack, TenantStackParams, WorkspaceEnvVar, WorkspaceEnvVarSource,
    attach_github_token_env_ref, attach_s3_credentials_env_refs, build_preview_branch,
    is_tenant_scoped_backend, preview_secret_copy_name, synthesize_ephemeral_preview,
    synthesize_k8s_manifest_pulumi_yaml, synthesize_management_core_stack, synthesize_tenant_stack,
    tenant_scoped_preview_backend, validate_preview_branch,
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
