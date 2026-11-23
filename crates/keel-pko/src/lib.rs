//! `keel-pko`
//!
//! Pulumi Kubernetes Operator (PKO v2) `Stack` & Flux CD `GitRepository` synthesis engine,
//! `pulumi preview --json` diff parser & Markdown reporter, and ephemeral PR preview garbage-collection planner.

mod gc;
mod preview;
mod run;
mod stack;

pub use gc::{GcPlan, PreviewResourceCandidate, plan_preview_gc};
pub use preview::{
    ChangeOp, PreviewReport, PreviewSource, PreviewSummary, ResourceDiff, format_markdown_report,
};
pub use run::{
    ANNOTATION_HEAD_SHA, ANNOTATION_REPORTED_COMMIT, PREVIEW_LOG_TAIL_LINES,
    PREVIEW_REPORT_CONFIGMAP_KEY, PrLifecycle, PreviewPhase, PreviewRecord, PreviewRunStatus,
    build_pko_preview_report, map_pko_preview_status, plan_preview_reconcile, select_update_name,
};
pub use stack::{
    CONFIG_PROTECT_KEYS, CrObjectMeta, DEFAULT_FLUX_POLL_INTERVAL,
    DEFAULT_MANAGEMENT_SERVICE_ACCOUNT, DEFAULT_PREVIEW_SERVICE_ACCOUNT, EphemeralPreviewParams,
    FLUX_GIT_REPO_API_VERSION, FLUX_GIT_REPO_KIND, FluxGitRepository, FluxGitRepositoryRef,
    FluxGitRepositorySpec, IN_CLUSTER_S3_BACKEND_URL, KEEL_SSH_KNOWN_HOSTS_FILE,
    KEEL_SSH_KNOWN_HOSTS_MOUNT_PATH, KEEL_SSH_KNOWN_HOSTS_VOLUME_NAME, LABEL_PROTECT, LABEL_REPO,
    LocalObjectReference, MANAGEMENT_CORE_STACK_NAME, ManagementCoreStackParams,
    PKO_WORKSPACE_IMAGE, PLATFORM_REPO_LABEL, PULUMI_PASSPHRASE_SECRET_KEY,
    PULUMI_PASSPHRASE_SECRET_NAME, PkoEnvRef, PkoGitAuth, PkoGitBasicAuth, PkoGitSource,
    PkoGitSshAuth, PkoStack, PkoWorkspaceTemplate, PreviewSecretCopy, PreviewStack,
    SSH_KNOWN_HOSTS_ENV, TenantStackParams, attach_github_token_env_ref,
    attach_s3_credentials_env_refs, build_preview_branch, is_ssh_git_url, is_tenant_scoped_backend,
    parse_preview_key, pko_project_repo_url, preview_key, preview_secret_copy_name,
    preview_stack_name, synthesize_ephemeral_preview, synthesize_k8s_manifest_pulumi_yaml,
    synthesize_management_core_stack, synthesize_tenant_stack, validate_flux_git_source_url,
    validate_git_source_url,
};

/// Errors produced by `keel-pko` synthesis and preview diff parsing.
#[derive(Debug, thiserror::Error)]
pub enum PkoError {
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("Preview diff parse error: {0}")]
    PreviewParse(String),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("YAML serialization error: {0}")]
    Yaml(#[from] noyalib::Error),
}
