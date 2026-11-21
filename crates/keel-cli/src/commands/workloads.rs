//! Infrastructure, application, and preview inspection command handlers
//! (`keel infra *`, `keel app *`, `keel preview *`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use keel_core::{
    CatalogApplication, CatalogInfraBlueprint, KEEL_PREVIEWS_NAMESPACE, PreparedHelmValues,
    prepare_helm_values, resolve_ssh_target_from_sources, validate_dns_label,
    validate_schema_values,
};
use keel_git::{PullRequestInfo, PullRequestState};
use keel_kube::{CrdReconcileStatus, CrdWatchEvent};
use keel_pko::{PkoStack, PreviewReport};
use keel_server::StackSummary;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cli::{
    AppInstallArgs, InfraForceDeleteArgs, InfraListArgs, InfraProvisionArgs, InfraSshArgs,
    InfraUnlockArgs, OutputFormat, PreviewStatusArgs,
};

use super::{
    CliContext, CliError, attach_auth, extract_api_error, normalize_server_url,
    validate_non_flag_input,
};

// ============================================================================
// 5. `keel infra list`
// ============================================================================

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StacksApiResponse {
    stacks: Vec<StackSummary>,
}

/// Formats PKO v2 [`StackSummary`] inventory items as either a terminal table or pretty JSON.
pub fn format_stacks_output(
    stacks: &[StackSummary],
    format: OutputFormat,
) -> Result<String, CliError> {
    match format {
        OutputFormat::Json => Ok(serde_json::to_string_pretty(stacks)?),
        OutputFormat::Table => {
            if stacks.is_empty() {
                return Ok("No PKO stacks found.".to_string());
            }

            let headers = [
                "NAMESPACE",
                "NAME",
                "TENANT",
                "PROVIDER",
                "MODE",
                "PROTECT",
                "STATUS",
            ];
            let rows: Vec<[String; 7]> = stacks
                .iter()
                .map(|s| {
                    [
                        s.namespace.clone(),
                        s.name.clone(),
                        s.tenant.clone().unwrap_or_else(|| "-".to_string()),
                        s.provider.clone().unwrap_or_else(|| "-".to_string()),
                        if s.preview {
                            "preview".to_string()
                        } else {
                            "active".to_string()
                        },
                        if s.protect {
                            "true".to_string()
                        } else {
                            "false".to_string()
                        },
                        s.status.clone(),
                    ]
                })
                .collect();

            let mut widths = headers.map(str::len);
            for row in &rows {
                for (i, cell) in row.iter().enumerate() {
                    widths[i] = widths[i].max(cell.len());
                }
            }

            let mut out = String::new();
            out.push_str(&format_fixed_row(&headers, &widths));
            out.push('\n');
            let dividers: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
            let div_refs: [&str; 7] = [
                &dividers[0],
                &dividers[1],
                &dividers[2],
                &dividers[3],
                &dividers[4],
                &dividers[5],
                &dividers[6],
            ];
            out.push_str(&format_fixed_row(&div_refs, &widths));

            for row in &rows {
                out.push('\n');
                let refs: [&str; 7] = [
                    &row[0], &row[1], &row[2], &row[3], &row[4], &row[5], &row[6],
                ];
                out.push_str(&format_fixed_row(&refs, &widths));
            }
            Ok(out)
        }
    }
}

fn format_fixed_row<const N: usize>(cells: &[&str; N], widths: &[usize; N]) -> String {
    let mut line = String::new();
    for (idx, cell) in cells.iter().enumerate() {
        if idx + 1 == N {
            line.push_str(cell);
        } else {
            line.push_str(&format!("{:<width$}  ", cell, width = widths[idx]));
        }
    }
    line
}

/// Executes `keel infra list` querying `/api/v1/stacks` and rendering table or JSON output.
pub async fn execute_infra_list(
    args: &InfraListArgs,
    ctx: &CliContext,
) -> Result<String, CliError> {
    if let Some(tenant) = &args.tenant {
        validate_dns_label(tenant, "tenant")?;
    }
    if let Some(provider) = &args.provider {
        validate_dns_label(provider, "provider")?;
    }

    let base_url = normalize_server_url(&args.server)?;
    let endpoint = format!("{base_url}/api/v1/stacks");

    let mut query: Vec<(&str, &str)> = Vec::new();
    if let Some(t) = args.tenant.as_deref() {
        query.push(("tenant", t));
    }
    if let Some(p) = args.provider.as_deref() {
        query.push(("provider", p));
    }

    let req = attach_auth(ctx.http.get(&endpoint).query(&query), ctx, &base_url);
    let resp = req.send().await?;
    if !resp.status().is_success() {
        return Err(extract_api_error(resp).await);
    }

    let payload: StacksApiResponse = resp.json().await?;
    format_stacks_output(&payload.stacks, args.output)
}

// ============================================================================
// 6 & 7. `keel infra provision`, `keel app install`, `keel infra ssh`, `force-delete`, `unlock`
// ============================================================================

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogApiResponse {
    applications: Vec<CatalogApplication>,
    blueprints: Vec<CatalogInfraBlueprint>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateProposalApiResponse {
    proposal: PullRequestInfo,
    branch: String,
    preview_stack: PkoStack,
    report: PreviewReport,
}

/// Outcome of opening an infrastructure provisioning or application installation PR (`AGENTS.md` §3.6).
#[derive(Debug, Clone)]
pub struct ProposalCommandOutcome {
    pub proposal: PullRequestInfo,
    pub branch: String,
    pub preview_stack: PkoStack,
    pub report: PreviewReport,
    pub output: String,
}

/// Reads and parses a YAML or JSON values file into a JSON object (`serde_json::Value`).
pub async fn load_values_file(path: &Path) -> Result<Value, CliError> {
    let raw = tokio::fs::read_to_string(path).await.map_err(|e| {
        CliError::Validation(format!(
            "Failed to read values file '{}': {e}",
            path.display()
        ))
    })?;
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    let val: Value = serde_yml::from_str(&raw)?;
    if !val.is_object() {
        return Err(CliError::Validation(format!(
            "Values file '{}' must deserialize to a YAML/JSON mapping object",
            path.display()
        )));
    }
    Ok(val)
}

/// Validates input parameters against a [`CatalogInfraBlueprint`] JSONSchema (`SDD.md` §4, §7.2).
pub fn validate_blueprint_values(
    blueprint: &CatalogInfraBlueprint,
    values: &Value,
) -> Result<(), CliError> {
    blueprint.validate()?;
    validate_schema_values(&blueprint.spec.schema, values)?;
    Ok(())
}

/// Validates input values and progressive-delivery flags against a [`CatalogApplication`] JSONSchema (`SDD.md` §5.1, §5.2).
pub fn validate_application_values(
    app: &CatalogApplication,
    values: &Value,
    canary: bool,
) -> Result<PreparedHelmValues, CliError> {
    app.validate()?;
    if canary && !app.spec.progressive_delivery.flagger_supported {
        return Err(CliError::Validation(format!(
            "Catalog application '{}' does not support Flagger canary rollouts (flaggerSupported=false)",
            app.metadata.id
        )));
    }
    Ok(prepare_helm_values(app, values)?)
}

/// Executes `keel infra provision`: validates values against the blueprint JSONSchema and opens a PR (`AGENTS.md` §3.6).
pub async fn execute_infra_provision(
    args: &InfraProvisionArgs,
    ctx: &CliContext,
) -> Result<ProposalCommandOutcome, CliError> {
    validate_dns_label(&args.blueprint, "blueprint")?;
    validate_dns_label(&args.tenant, "tenant")?;
    let values = load_values_file(&args.values_file).await?;
    let base_url = normalize_server_url(&args.server)?;

    // Fetch catalog to validate blueprint JSONSchema client-side before opening PR
    let catalog = fetch_catalog(&base_url, ctx).await?;
    let blueprint = catalog
        .blueprints
        .iter()
        .find(|bp| bp.metadata.id == args.blueprint)
        .ok_or_else(|| {
            CliError::Validation(format!(
                "Infrastructure blueprint '{}' not found or provider disabled in catalog",
                args.blueprint
            ))
        })?;
    validate_blueprint_values(blueprint, &values)?;

    submit_proposal_request(
        &base_url,
        ctx,
        &args.tenant,
        &args.blueprint,
        "infra",
        &values,
        false,
        None,
    )
    .await
}

/// Executes `keel app install`: validates Helm/Kustomize/Manifest values against catalog JSONSchema and opens a PR (`AGENTS.md` §3.6).
pub async fn execute_app_install(
    args: &AppInstallArgs,
    ctx: &CliContext,
) -> Result<ProposalCommandOutcome, CliError> {
    validate_dns_label(&args.app, "app")?;
    validate_dns_label(&args.tenant, "tenant")?;
    if let Some(ref eng) = args.engine {
        eng.parse::<keel_core::ManifestEngine>()?;
    }
    let values = load_values_file(&args.values_file).await?;
    let base_url = normalize_server_url(&args.server)?;

    // Fetch catalog to validate application JSONSchema and canary capability client-side
    let catalog = fetch_catalog(&base_url, ctx).await?;
    let app = catalog
        .applications
        .iter()
        .find(|a| a.metadata.id == args.app)
        .ok_or_else(|| {
            CliError::Validation(format!(
                "Catalog application '{}' not found in catalog",
                args.app
            ))
        })?;
    let _prepared = validate_application_values(app, &values, args.canary)?;

    submit_proposal_request(
        &base_url,
        ctx,
        &args.tenant,
        &args.app,
        "application",
        &values,
        args.canary,
        args.engine.as_deref(),
    )
    .await
}

async fn fetch_catalog(base_url: &str, ctx: &CliContext) -> Result<CatalogApiResponse, CliError> {
    let endpoint = format!("{base_url}/api/v1/catalog");
    let resp = attach_auth(ctx.http.get(&endpoint), ctx, base_url)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(extract_api_error(resp).await);
    }
    Ok(resp.json().await?)
}

#[allow(clippy::too_many_arguments)]
async fn submit_proposal_request(
    base_url: &str,
    ctx: &CliContext,
    tenant: &str,
    catalog_id: &str,
    kind: &str,
    values: &Value,
    canary: bool,
    action_engine: Option<&str>,
) -> Result<ProposalCommandOutcome, CliError> {
    let endpoint = format!("{base_url}/api/v1/proposals");
    let mut req_body = json!({
        "tenant": tenant,
        "catalogId": catalog_id,
        "kind": kind,
        "values": values,
        "canary": canary,
    });
    if let Some(eng) = action_engine
        && let Some(obj) = req_body.as_object_mut()
    {
        obj.insert(
            "actionEngine".to_string(),
            Value::String(eng.trim().to_ascii_lowercase()),
        );
    }

    let resp = attach_auth(ctx.http.post(&endpoint).json(&req_body), ctx, base_url)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(extract_api_error(resp).await);
    }

    let created: CreateProposalApiResponse = resp.json().await?;
    let mut output = String::new();
    output.push_str(&format!(
        "Opened Proposal PR #{}: {}\n",
        created.proposal.id, created.proposal.title
    ));
    output.push_str(&format!(
        "Branch:        {} -> {}\n",
        created.branch, created.proposal.target_branch
    ));
    output.push_str(&format!("PR URL:        {}\n", created.proposal.url));
    output.push_str(&format!(
        "Preview Stack: {}/{} (preview={})\n\n",
        created.preview_stack.metadata.namespace,
        created.preview_stack.metadata.name,
        created.preview_stack.is_preview()
    ));
    output.push_str(&format_preview_table(&created.report));

    Ok(ProposalCommandOutcome {
        proposal: created.proposal,
        branch: created.branch,
        preview_stack: created.preview_stack,
        report: created.report,
        output,
    })
}

/// Builds a validated `ssh` argument vector (never invoking `sh -c`, preventing flag injection per `NFR-SEC-06`).
pub fn build_ssh_argv(
    info: &keel_core::StackSshInfoResponse,
    identity_file: Option<&Path>,
) -> Result<Vec<String>, CliError> {
    validate_non_flag_input(&info.ssh_user, "ssh_user")?;
    validate_non_flag_input(&info.ssh_host, "ssh_host")?;
    if info.ssh_user.chars().any(char::is_whitespace)
        || info.ssh_host.chars().any(char::is_whitespace)
    {
        return Err(CliError::Validation(
            "ssh_user and ssh_host must not contain whitespace".to_string(),
        ));
    }
    if info.ssh_port == 0 {
        return Err(CliError::Validation(
            "ssh_port must be greater than 0".to_string(),
        ));
    }

    let mut argv = Vec::new();
    if info.ssh_port != 22 {
        argv.push("-p".to_string());
        argv.push(info.ssh_port.to_string());
    }
    if let Some(id_path) = identity_file {
        let id_str = id_path.to_string_lossy().trim().to_string();
        validate_non_flag_input(&id_str, "identity_file")?;
        argv.push("-i".to_string());
        argv.push(id_str);
    }
    argv.push(format!("{}@{}", info.ssh_user, info.ssh_host));
    Ok(argv)
}

/// Formats [`keel_core::StackSshInfoResponse`] as `table`, `json`, or `command`.
pub fn format_ssh_info_output(
    info: &keel_core::StackSshInfoResponse,
    output_format: &str,
) -> Result<String, CliError> {
    match output_format.trim().to_ascii_lowercase().as_str() {
        "command" => Ok(info.ssh_command.clone()),
        "json" => Ok(serde_json::to_string_pretty(info)?),
        "table" => {
            let mut out = String::new();
            out.push_str(&format!("Stack:       {}\n", info.stack_name));
            out.push_str(&format!("Namespace:   {}\n", info.namespace));
            out.push_str(&format!("Tenant:      {}\n", info.tenant));
            out.push_str(&format!("Provider:    {}\n", info.provider));
            out.push_str(&format!("Host:        {}\n", info.ssh_host));
            out.push_str(&format!("User:        {}\n", info.ssh_user));
            out.push_str(&format!("Port:        {}\n", info.ssh_port));
            out.push_str(&format!("SSH Command: {}", info.ssh_command));
            Ok(out)
        }
        other => Err(CliError::Validation(format!(
            "Unsupported --output format '{other}'; expected 'table', 'json', or 'command'"
        ))),
    }
}

pub(crate) async fn resolve_local_fallback_ssh_info(
    workdir: &Path,
    tenant: &str,
    stack_name: &str,
) -> Option<keel_core::StackSshInfoResponse> {
    let stack_dir = workdir
        .join("tenants")
        .join(tenant)
        .join("infra")
        .join(stack_name);
    let stack_path = stack_dir.join("stack.yaml");
    let values_path = stack_dir.join("values.yaml");
    let pulumi_path = stack_dir.join("Pulumi.yaml");
    if !tokio::fs::try_exists(&stack_path).await.unwrap_or(false)
        && !tokio::fs::try_exists(&values_path).await.unwrap_or(false)
    {
        return None;
    }

    let stack_doc = tokio::fs::read_to_string(&stack_path)
        .await
        .ok()
        .and_then(|raw| serde_yml::from_str::<Value>(&raw).ok());
    let values_doc = tokio::fs::read_to_string(&values_path)
        .await
        .ok()
        .and_then(|raw| serde_yml::from_str::<Value>(&raw).ok());
    let pulumi_doc = tokio::fs::read_to_string(&pulumi_path)
        .await
        .ok()
        .and_then(|raw| serde_yml::from_str::<Value>(&raw).ok());

    Some(resolve_ssh_target_from_sources(
        stack_name,
        &format!("tenant-{tenant}"),
        tenant,
        None,
        &std::collections::HashMap::new(),
        values_doc.as_ref(),
        stack_doc.as_ref(),
        pulumi_doc.as_ref(),
        false,
    ))
}

/// Executes `keel infra ssh` querying `/api/v1/stacks/tenant-{tenant}/{stack_name}/ssh-info`
/// (with local GitOps fallback) and either printing the connection details or spawning `ssh`.
pub async fn execute_infra_ssh(args: &InfraSshArgs, ctx: &CliContext) -> Result<String, CliError> {
    validate_dns_label(&args.stack_name, "stack_name")?;
    validate_dns_label(&args.tenant, "tenant")?;

    let base_url = normalize_server_url(&args.server)?;
    let endpoint = format!(
        "{base_url}/api/v1/stacks/tenant-{}/{}/ssh-info",
        args.tenant, args.stack_name
    );

    let mut info: keel_core::StackSshInfoResponse =
        match attach_auth(ctx.http.get(&endpoint), ctx, &base_url)
            .send()
            .await
        {
            Ok(resp) => {
                if !resp.status().is_success() {
                    return Err(extract_api_error(resp).await);
                }
                resp.json().await?
            }
            Err(http_err) => {
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                if let Some(fallback) =
                    resolve_local_fallback_ssh_info(&cwd, &args.tenant, &args.stack_name).await
                {
                    fallback
                } else {
                    return Err(CliError::Http(http_err));
                }
            }
        };

    if let Some(ref user_override) = args.user {
        validate_non_flag_input(user_override, "user")?;
        info.ssh_user = user_override.trim().to_string();
    }
    if let Some(port_override) = args.port {
        if port_override == 0 {
            return Err(CliError::Validation(
                "port must be greater than 0".to_string(),
            ));
        }
        info.ssh_port = port_override;
    }

    let argv = build_ssh_argv(&info, args.identity_file.as_deref())?;
    info.ssh_command = format!("ssh {}", argv.join(" "));

    if args.exec {
        let status = tokio::process::Command::new("ssh")
            .args(&argv)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .await
            .map_err(|e| CliError::Execution(format!("Failed to execute 'ssh': {e}")))?;
        if !status.success() {
            return Err(CliError::Execution(format!(
                "SSH session exited with status {status}"
            )));
        }
        Ok(String::new())
    } else {
        format_ssh_info_output(&info, &args.output)
    }
}

/// Executes `keel infra force-delete` by calling `POST /api/v1/stacks/{namespace}/{name}/force-delete`.
pub async fn execute_infra_force_delete(
    args: &InfraForceDeleteArgs,
    ctx: &CliContext,
) -> Result<String, CliError> {
    validate_dns_label(&args.stack_name, "stack_name")?;
    let namespace = match &args.namespace {
        Some(ns) => {
            validate_dns_label(ns, "namespace")?;
            ns.clone()
        }
        None => {
            validate_dns_label(&args.tenant, "tenant")?;
            format!("tenant-{}", args.tenant)
        }
    };

    let base_url = normalize_server_url(&args.server)?;
    let endpoint = format!(
        "{base_url}/api/v1/stacks/{}/{}/force-delete",
        namespace, args.stack_name
    );

    let resp = attach_auth(ctx.http.post(&endpoint), ctx, &base_url)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(extract_api_error(resp).await);
    }

    let body: Value = resp.json().await?;
    if matches!(args.output, OutputFormat::Json) {
        return Ok(serde_json::to_string_pretty(&body)?);
    }

    let message = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Stack force-deleted");
    Ok(format!(
        "Force-deleted stack {namespace}/{}: {message}",
        args.stack_name
    ))
}

/// Executes `keel infra unlock` by calling `POST /api/v1/stacks/{namespace}/{name}/unlock`.
pub async fn execute_infra_unlock(
    args: &InfraUnlockArgs,
    ctx: &CliContext,
) -> Result<String, CliError> {
    validate_dns_label(&args.stack_name, "stack_name")?;
    let namespace = match &args.namespace {
        Some(ns) => {
            validate_dns_label(ns, "namespace")?;
            ns.clone()
        }
        None => {
            validate_dns_label(&args.tenant, "tenant")?;
            format!("tenant-{}", args.tenant)
        }
    };

    let base_url = normalize_server_url(&args.server)?;
    let endpoint = format!(
        "{base_url}/api/v1/stacks/{}/{}/unlock",
        namespace, args.stack_name
    );

    let resp = attach_auth(ctx.http.post(&endpoint), ctx, &base_url)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(extract_api_error(resp).await);
    }

    let body: Value = resp.json().await?;
    if matches!(args.output, OutputFormat::Json) {
        return Ok(serde_json::to_string_pretty(&body)?);
    }

    let message = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Stack unlocked");
    Ok(format!(
        "Unlocked stack {namespace}/{}: {message}",
        args.stack_name
    ))
}

// ============================================================================
// 8. `keel preview status` & Terminal Diff Table Formatter
// ============================================================================

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProposalPreviewApiResponse {
    pr_id: String,
    url: String,
    state: PullRequestState,
    source_branch: String,
    target_branch: String,
    report: PreviewReport,
}

/// Outcome of `keel preview status`.
#[derive(Debug, Clone)]
pub struct PreviewStatusOutcome {
    pub pr_id: String,
    pub state: PullRequestState,
    pub url: String,
    pub report: PreviewReport,
    pub watched_events: Vec<CrdWatchEvent>,
    pub output: String,
}

/// Renders a PKO v2 [`PreviewReport`] as an aligned terminal diff table (`+ Create`, `~ Update`, `- Delete`, `! Replace`, `! Drift`).
pub fn format_preview_table(report: &PreviewReport) -> String {
    let s = &report.summary;
    let mut out = String::new();
    out.push_str(&format!(
        "Summary: +{} create, ~{} update, -{} delete, !{} replace, !{} drift, ={} unchanged (total: {})\n",
        s.create,
        s.update,
        s.delete,
        s.replace,
        s.drift,
        s.same,
        s.total()
    ));

    if s.has_destructive_changes() {
        out.push_str("WARNING: Preview contains destructive resource changes (delete/replace).\n");
    }
    if s.drift > 0 {
        out.push_str(&format!(
            "DRIFT ALERT: {} resource(s) have drifted from desired Git state.\n",
            s.drift
        ));
    }

    if report.resources.is_empty() {
        out.push_str("No resource changes planned.");
        return out;
    }

    let headers = ["OPERATION", "RESOURCE TYPE", "NAME", "CHANGED FIELDS"];
    let rows: Vec<[String; 4]> = report
        .resources
        .iter()
        .map(|r| {
            let fields = if r.changed_fields.is_empty() {
                "-".to_string()
            } else {
                r.changed_fields.join(", ")
            };
            [
                r.op.badge().to_string(),
                r.resource_type.clone(),
                r.name.clone(),
                fields,
            ]
        })
        .collect();

    let mut widths = headers.map(str::len);
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    out.push_str(&format_fixed_row(&headers, &widths));
    out.push('\n');
    let dividers: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    let div_refs: [&str; 4] = [&dividers[0], &dividers[1], &dividers[2], &dividers[3]];
    out.push_str(&format_fixed_row(&div_refs, &widths));

    for row in &rows {
        out.push('\n');
        let refs: [&str; 4] = [&row[0], &row[1], &row[2], &row[3]];
        out.push_str(&format_fixed_row(&refs, &widths));
    }

    out
}

/// Executes `keel preview status` fetching `/api/v1/proposals/{pr}/preview` and optionally watching SSE updates.
pub async fn execute_preview_status(
    args: &PreviewStatusArgs,
    ctx: &CliContext,
) -> Result<PreviewStatusOutcome, CliError> {
    validate_non_flag_input(&args.pr, "pr")?;
    if args.pr.contains('/') || args.pr.contains("..") {
        return Err(CliError::Validation(format!(
            "invalid pull request identifier '{}'",
            args.pr
        )));
    }

    let base_url = normalize_server_url(&args.server)?;
    let preview_endpoint = format!("{base_url}/api/v1/proposals/{}/preview", args.pr.trim());
    let resp = attach_auth(ctx.http.get(&preview_endpoint), ctx, &base_url)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(extract_api_error(resp).await);
    }

    let mut preview: ProposalPreviewApiResponse = resp.json().await?;
    let mut watched_events = Vec::new();

    if args.watch && preview.state == PullRequestState::Open {
        let watch_timeout = if !preview.report.resources.is_empty() {
            Duration::from_secs(2)
        } else {
            Duration::from_secs(60)
        };
        watched_events = watch_preview_sse(&base_url, &args.pr, ctx, watch_timeout).await?;

        let refresh_resp = attach_auth(ctx.http.get(&preview_endpoint), ctx, &base_url)
            .send()
            .await?;
        if refresh_resp.status().is_success() {
            preview = refresh_resp.json().await?;
        }
    }

    let mut output = String::new();
    output.push_str(&format!(
        "PR #{} ({}) — {} -> {}\n",
        preview.pr_id, preview.state, preview.source_branch, preview.target_branch
    ));
    output.push_str(&format!("Forge URL: {}\n", preview.url));

    if !watched_events.is_empty() {
        output.push_str("Watch Events:\n");
        for ev in &watched_events {
            output.push_str(&format!(
                "  [{}] {}/{} -> {:?}: {}\n",
                ev.kind, ev.namespace, ev.name, ev.status, ev.message
            ));
        }
    }
    output.push('\n');
    output.push_str(&format_preview_table(&preview.report));

    Ok(PreviewStatusOutcome {
        pr_id: preview.pr_id,
        state: preview.state,
        url: preview.url,
        report: preview.report,
        watched_events,
        output,
    })
}

async fn watch_preview_sse(
    base_url: &str,
    pr_id: &str,
    ctx: &CliContext,
    watch_timeout: Duration,
) -> Result<Vec<CrdWatchEvent>, CliError> {
    let sse_url = format!("{base_url}/api/v1/events/stream");
    let mut resp = attach_auth(
        ctx.stream_http
            .get(&sse_url)
            .query(&[("namespace", KEEL_PREVIEWS_NAMESPACE)]),
        ctx,
        base_url,
    )
    .send()
    .await?;

    if !resp.status().is_success() {
        return Err(extract_api_error(resp).await);
    }

    let mut collected = Vec::new();
    let mut buffer = String::new();
    let pr_marker = format!("pr-{pr_id}");
    let deadline = tokio::time::Instant::now() + watch_timeout;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let chunk_res = tokio::time::timeout(remaining, resp.chunk()).await;
        let Ok(chunk_opt) = chunk_res else {
            // Bounded watch window elapsed without a new terminal SSE event; return collected events cleanly.
            break;
        };
        let Some(chunk) = chunk_opt? else {
            break;
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(newline_idx) = buffer.find('\n') {
            let line = buffer[..newline_idx].trim().to_string();
            buffer.drain(..=newline_idx);

            let Some(json_str) = line.strip_prefix("data:") else {
                continue;
            };
            let json_trimmed = json_str.trim();
            if json_trimmed.is_empty() {
                continue;
            }
            if let Ok(ev) = serde_json::from_str::<CrdWatchEvent>(json_trimmed) {
                let matches_pr =
                    ev.name.contains(&pr_marker) || ev.message.contains(&format!("#{pr_id}"));
                let is_terminal = matches!(
                    ev.status,
                    CrdReconcileStatus::Ready
                        | CrdReconcileStatus::Failed
                        | CrdReconcileStatus::Deleted
                );
                collected.push(ev);
                if matches_pr && is_terminal {
                    return Ok(collected);
                }
            }
        }
    }

    Ok(collected)
}
