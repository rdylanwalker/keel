//! Infrastructure, application, and preview inspection command handlers
//! (`keel infra *`, `keel app *`, `keel preview *`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use keel_core::{
    CatalogApplication, CatalogInfraBlueprint, KEEL_PREVIEWS_NAMESPACE, PreparedHelmValues,
    apply_schema_defaults, prepare_helm_values, resolve_ssh_target_from_sources, strip_null_values,
    validate_dns_label, validate_schema_values,
};
use keel_git::{PullRequestInfo, PullRequestState};
use keel_kube::{CrdReconcileStatus, CrdWatchEvent};
use keel_pko::{PreviewPhase, PreviewReport, PreviewSource};
use keel_server::StackSummary;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cli::{
    AppInstallArgs, InfraForceDeleteArgs, InfraListArgs, InfraProvisionArgs, InfraSshArgs,
    InfraUnlockArgs, OutputFormat, PreviewStatusArgs,
};

use super::{
    CliContext, CliError, attach_auth, extract_api_error, normalize_server_url,
    normalize_tenant_arg, validate_non_flag_input,
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
    let tenant = args.tenant.as_deref().map(normalize_tenant_arg);
    if let Some(tenant) = &tenant {
        validate_dns_label(tenant, "tenant")?;
    }
    if let Some(provider) = &args.provider {
        validate_dns_label(provider, "provider")?;
    }

    let base_url = normalize_server_url(&args.server)?;
    let endpoint = format!("{base_url}/api/v1/stacks");

    let mut query: Vec<(&str, &str)> = Vec::new();
    if let Some(t) = tenant.as_deref() {
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
    report: PreviewReport,
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
    let val: Value = noyalib::from_str(&raw).map_err(|e| {
        CliError::Validation(format!(
            "invalid YAML/JSON in values file '{}': {e}",
            path.display()
        ))
    })?;
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
    validate_schema_values(
        &blueprint.spec.schema,
        &effective_values(&blueprint.spec.schema, values),
    )?;
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
    Ok(prepare_helm_values(
        app,
        &effective_values(&app.spec.schema, values),
    )?)
}

/// What the server commits for a values file, as far as the CLI can tell without the stored
/// values: a first install gets the schema defaults; an edit merges the file over the committed
/// values (which hold the install's defaults), with `null` removing a key. The server validates
/// the real result again (`SDD.md` §5.1).
fn effective_values(schema: &Value, values: &Value) -> Value {
    apply_schema_defaults(schema, &strip_null_values(values))
}

/// Executes `keel infra provision`: validates values against the blueprint JSONSchema and opens a PR (`AGENTS.md` §3.6).
pub async fn execute_infra_provision(
    args: &InfraProvisionArgs,
    ctx: &CliContext,
) -> Result<String, CliError> {
    validate_dns_label(&args.blueprint, "blueprint")?;
    let tenant = normalize_tenant_arg(&args.tenant);
    validate_dns_label(&tenant, "tenant")?;
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
        &tenant,
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
) -> Result<String, CliError> {
    validate_dns_label(&args.app, "app")?;
    let tenant = normalize_tenant_arg(&args.tenant);
    validate_dns_label(&tenant, "tenant")?;
    if let Some(ref eng) = args.engine {
        eng.parse::<keel_core::ManifestEngine>()?;
    }
    // Without a values file the server applies the catalog schema defaults (FU-7).
    let values = match &args.values_file {
        Some(path) => load_values_file(path).await?,
        None => json!({}),
    };
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
    let _prepared = validate_application_values(app, &values, args.canary).map_err(|err| {
        match (&args.values_file, err) {
            // Name what to do: the missing fields have no schema default.
            (None, CliError::Core(core @ keel_core::CoreError::SchemaValidation(_))) => {
                CliError::Validation(format!(
                    "{core} (no --values-file given: set required values without a catalog \
                     default in one)"
                ))
            }
            (_, err) => err,
        }
    })?;

    submit_proposal_request(
        &base_url,
        ctx,
        &tenant,
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
) -> Result<String, CliError> {
    keel_core::validate_dns_label(tenant, "tenant")?;
    let endpoint = format!("{base_url}/api/v1/tenants/{tenant}/proposals");
    let mut req_body = json!({
        "catalogId": catalog_id,
        "kind": kind,
        "values": values,
        "canary": canary,
        // Re-running against an existing instance edits it: merge the file over the committed
        // values instead of replacing them, so omitted keys (and install defaults) are kept.
        "mergeStoredValues": true,
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
    // PR numbers are per repository: the tenant names the repository holding this PR.
    output.push_str(&format!(
        "Next:          keel preview status --pr {id} --tenant {tenant} --watch; keel proposal merge {id} --tenant {tenant}\n",
        id = created.proposal.id
    ));
    output.push('\n');
    output.push_str(&format_preview_table(&created.report));

    Ok(output)
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
        .and_then(|raw| noyalib::from_str::<Value>(&raw).ok());
    let values_doc = tokio::fs::read_to_string(&values_path)
        .await
        .ok()
        .and_then(|raw| noyalib::from_str::<Value>(&raw).ok());
    let pulumi_doc = tokio::fs::read_to_string(&pulumi_path)
        .await
        .ok()
        .and_then(|raw| noyalib::from_str::<Value>(&raw).ok());

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
    let tenant = normalize_tenant_arg(&args.tenant);
    validate_dns_label(&tenant, "tenant")?;

    let base_url = normalize_server_url(&args.server)?;
    let endpoint = format!(
        "{base_url}/api/v1/stacks/tenant-{}/{}/ssh-info",
        tenant, args.stack_name
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
                    resolve_local_fallback_ssh_info(&cwd, &tenant, &args.stack_name).await
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
            let tenant = normalize_tenant_arg(&args.tenant);
            validate_dns_label(&tenant, "tenant")?;
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
            let tenant = normalize_tenant_arg(&args.tenant);
            validate_dns_label(&tenant, "tenant")?;
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
    /// Repository-scoped preview key (absent from older servers: the PR id).
    #[serde(default)]
    preview_key: Option<String>,
    url: String,
    state: PullRequestState,
    source_branch: String,
    target_branch: String,
    report: PreviewReport,
}

/// Renders a PKO v2 [`PreviewReport`] as an aligned terminal diff table (`+ Create`, `~ Update`, `- Delete`, `! Replace`, `! Drift`).
/// Describes where a preview report came from and the state of its PKO run (CLI-4): a real
/// PKO `pulumi preview` vs. a control-plane simulation, and the preview `Stack` only when it was
/// actually applied to the cluster.
pub fn format_preview_provenance(report: &PreviewReport) -> String {
    let mut out = String::new();
    let source = match report.source {
        PreviewSource::Pko => "PKO (pulumi preview executed in keel-previews)",
        PreviewSource::Synthesized => {
            "Simulated (synthesized by the control plane; not a pulumi preview)"
        }
    };
    out.push_str(&format!("Preview Source: {source}\n"));
    match &report.run {
        Some(run) => {
            let mut status = run.phase.label().to_string();
            if let Some(msg) = run.message.as_deref().filter(|m| !m.is_empty()) {
                status.push_str(&format!(" - {msg}"));
            }
            out.push_str(&format!("PKO Preview:    {status}\n"));
            if let Some(stack) = &run.stack {
                out.push_str(&format!("Preview Stack:  {stack}\n"));
            }
            if let Some(commit) = &run.commit {
                out.push_str(&format!("Commit:         {commit}\n"));
            }
            if matches!(run.phase, PreviewPhase::Pending | PreviewPhase::Running) {
                out.push_str(
                    "                (run `keel preview status --pr <id> --watch` for real PKO results)\n",
                );
            }
        }
        None => out.push_str("PKO Preview:    Unknown\n"),
    }
    out
}

pub fn format_preview_table(report: &PreviewReport) -> String {
    let s = &report.summary;
    let mut out = format_preview_provenance(report);
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

/// Executes `keel preview status` fetching `/api/v1/tenants/{tenant}/proposals/{pr}/preview` and optionally watching SSE updates.
pub async fn execute_preview_status(
    args: &PreviewStatusArgs,
    ctx: &CliContext,
) -> Result<String, CliError> {
    validate_non_flag_input(&args.pr, "pr")?;
    if args.pr.contains('/') || args.pr.contains("..") {
        return Err(CliError::Validation(format!(
            "invalid pull request identifier '{}'",
            args.pr
        )));
    }

    let scope = super::api::proposal_scope(&args.tenant)?;
    let base_url = normalize_server_url(&args.server)?;
    let preview_endpoint = format!(
        "{base_url}/api/v1/tenants/{scope}/proposals/{}/preview",
        args.pr.trim()
    );
    let resp = attach_auth(ctx.http.get(&preview_endpoint), ctx, &base_url)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(extract_api_error(resp).await);
    }

    let mut preview: ProposalPreviewApiResponse = resp.json().await?;
    let mut watched_events = Vec::new();

    if args.watch && preview.state == PullRequestState::Open {
        // Wait for the real PKO run while it is pending/running; otherwise only briefly.
        let pko_in_flight = preview
            .report
            .run
            .as_ref()
            .is_some_and(|r| matches!(r.phase, PreviewPhase::Pending | PreviewPhase::Running));
        let watch_timeout = if pko_in_flight {
            Duration::from_secs(15 * 60)
        } else if !preview.report.resources.is_empty() {
            Duration::from_secs(2)
        } else {
            Duration::from_secs(60)
        };
        // SSE events carry the repository-scoped preview key (`preview-stack-pr-<key>`).
        let key = preview
            .preview_key
            .clone()
            .unwrap_or_else(|| args.pr.trim().to_string());
        watched_events =
            watch_preview_sse(&base_url, &key, ctx, watch_timeout, pko_in_flight).await?;

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

    Ok(output)
}

async fn watch_preview_sse(
    base_url: &str,
    pr_id: &str,
    ctx: &CliContext,
    watch_timeout: Duration,
    require_pko_result: bool,
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
    // Server events name the PR's repository: `PR #<n> in tenant-<id>` / `PR #<n> in platform`.
    let pr_label = match keel_pko::parse_preview_key(pr_id) {
        (Some(tenant), number) => format!("PR #{number} in tenant-{tenant}"),
        (None, number) => format!("PR #{number} in {}", keel_pko::PLATFORM_REPO_LABEL),
    };
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
                let matches_pr = ev.name.contains(&pr_marker)
                    || ev.message.contains(&pr_label)
                    || ev.message.contains(&format!("#{pr_id}"));
                // While a PKO preview is in flight, only the control plane's own "PKO preview ..."
                // result event (published after the report is persisted) ends the watch; generic
                // Stack watcher events can arrive before the results are collected.
                let is_terminal = matches!(
                    ev.status,
                    CrdReconcileStatus::Ready
                        | CrdReconcileStatus::Failed
                        | CrdReconcileStatus::Deleted
                ) && (!require_pko_result
                    || ev.status == CrdReconcileStatus::Deleted
                    || ev.message.starts_with("PKO preview"));
                collected.push(ev);
                if matches_pr && is_terminal {
                    return Ok(collected);
                }
            }
        }
    }

    Ok(collected)
}

#[cfg(test)]
mod preview_output_tests {
    use keel_pko::PreviewRunStatus;

    use super::*;

    #[test]
    fn preview_table_labels_simulated_vs_pko_and_only_shows_applied_stacks() {
        let simulated = PreviewReport {
            run: Some(PreviewRunStatus::not_executed(
                "No live cluster connected: PKO preview not executed",
            )),
            ..PreviewReport::default()
        };
        let out = format_preview_table(&simulated);
        assert!(out.contains("Preview Source: Simulated"));
        assert!(out.contains("PKO Preview:    Not executed - No live cluster connected"));
        assert!(!out.contains("Preview Stack:"));

        let running = PreviewReport {
            run: Some(PreviewRunStatus {
                phase: PreviewPhase::Running,
                stack: Some("keel-previews/preview-stack-pr-4".to_string()),
                commit: Some("abc123".to_string()),
                ..PreviewRunStatus::default()
            }),
            ..PreviewReport::default()
        };
        let out = format_preview_table(&running);
        assert!(out.contains("Preview Source: Simulated"));
        assert!(out.contains("PKO Preview:    Running"));
        assert!(out.contains("Preview Stack:  keel-previews/preview-stack-pr-4"));
        assert!(out.contains("Commit:         abc123"));
        assert!(out.contains("--watch"));

        let pko = PreviewReport {
            source: PreviewSource::Pko,
            run: Some(PreviewRunStatus {
                phase: PreviewPhase::Succeeded,
                ..PreviewRunStatus::default()
            }),
            ..PreviewReport::default()
        };
        let out = format_preview_table(&pko);
        assert!(out.contains("Preview Source: PKO"));
        assert!(out.contains("PKO Preview:    Succeeded"));
        assert!(!out.contains("--watch"));

        let legacy = format_preview_table(&PreviewReport::default());
        assert!(legacy.contains("PKO Preview:    Unknown"));
        assert!(legacy.contains("No resource changes planned."));
    }
}
