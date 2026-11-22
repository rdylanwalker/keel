//! Read/act commands over existing keel-server APIs: `keel proposal list|show|close|merge`,
//! `keel catalog list|show`, `keel app list` and `keel tenant repair-credentials` (`SDD.md` §7.2,
//! §7.3).
//!
//! Responses are handled as loose JSON so the CLI tolerates additive server changes.

use serde_json::Value;

use crate::cli::{
    AppListArgs, CatalogListArgs, CatalogShowArgs, OutputFormat, ProposalListArgs, ProposalRefArgs,
    TenantRepairCredentialsArgs,
};

use super::{
    CliContext, CliError, attach_auth, extract_api_error, normalize_server_url,
    normalize_tenant_arg,
};

fn s<'a>(v: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .unwrap_or("-")
}

fn validate_pr_id(pr: &str) -> Result<&str, CliError> {
    let pr = pr.trim().trim_start_matches('#');
    if pr.is_empty() || pr.len() > 64 || !pr.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(CliError::Validation(format!(
            "invalid pull request identifier '{pr}'"
        )));
    }
    Ok(pr)
}

/// Renders rows as a left-aligned table with a header.
pub(crate) fn render_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(cell.chars().count());
            }
        }
    }
    let fmt_row = |cells: Vec<&str>| {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<width$}", width = widths[i]))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    let mut out = fmt_row(headers.to_vec());
    out.push('\n');
    for row in rows {
        out.push_str(&fmt_row(row.iter().map(String::as_str).collect()));
        out.push('\n');
    }
    out
}

async fn get_json(
    ctx: &CliContext,
    base_url: &str,
    path: &str,
    query: &[(&str, &str)],
) -> Result<Value, CliError> {
    let resp = attach_auth(
        ctx.http.get(format!("{base_url}{path}")).query(query),
        ctx,
        base_url,
    )
    .send()
    .await?;
    if !resp.status().is_success() {
        return Err(extract_api_error(resp).await);
    }
    Ok(resp.json().await?)
}

async fn post_json(ctx: &CliContext, base_url: &str, path: &str) -> Result<Value, CliError> {
    post_json_query(ctx, base_url, path, &[]).await
}

async fn post_json_query(
    ctx: &CliContext,
    base_url: &str,
    path: &str,
    query: &[(&str, &str)],
) -> Result<Value, CliError> {
    let resp = attach_auth(
        ctx.http
            .post(format!("{base_url}{path}"))
            .query(query)
            .json(&serde_json::json!({})),
        ctx,
        base_url,
    )
    .send()
    .await?;
    if !resp.status().is_success() {
        return Err(extract_api_error(resp).await);
    }
    Ok(resp.json().await.unwrap_or(Value::Null))
}

fn pretty(v: &Value) -> Result<String, CliError> {
    Ok(serde_json::to_string_pretty(v)?)
}

/// Formats a proposal list (`proposals` array) as a table.
pub(crate) fn format_proposals_table(proposals: &[Value]) -> String {
    if proposals.is_empty() {
        return "No proposals found.\n".to_string();
    }
    let rows: Vec<Vec<String>> = proposals
        .iter()
        .map(|p| {
            vec![
                format!("#{}", s(p, &["id"])),
                s(p, &["state"]).to_string(),
                p.get("scope")
                    .or_else(|| p.get("tenant"))
                    .and_then(Value::as_str)
                    .unwrap_or("-")
                    .to_string(),
                p.pointer("/repository/fullName")
                    .or_else(|| p.pointer("/repository/label"))
                    .and_then(Value::as_str)
                    .unwrap_or("-")
                    .to_string(),
                s(p, &["title"]).to_string(),
                s(p, &["sourceBranch"]).to_string(),
                s(p, &["updatedAt"]).to_string(),
            ]
        })
        .collect();
    render_table(
        &[
            "ID",
            "STATE",
            "TENANT",
            "REPOSITORY",
            "TITLE",
            "BRANCH",
            "UPDATED",
        ],
        &rows,
    )
}

fn format_proposal_detail(p: &Value) -> String {
    let mut out = format!("Proposal #{}: {}\n", s(p, &["id"]), s(p, &["title"]));
    for (label, keys) in [
        ("State", &["state"][..]),
        ("Tenant", &["scope", "tenant"][..]),
        ("Branch", &["sourceBranch"][..]),
        ("Target", &["targetBranch"][..]),
        ("Head SHA", &["headSha"][..]),
        ("URL", &["url"][..]),
        ("Created", &["createdAt"][..]),
        ("Updated", &["updatedAt"][..]),
    ] {
        out.push_str(&format!("{label:<10} {}\n", s(p, keys)));
    }
    if let Some(stat) = p.get("diffStat").and_then(Value::as_str) {
        out.push_str(&format!("Diff       {stat}\n"));
    }
    let body = s(p, &["body"]);
    if body != "-" && !body.trim().is_empty() {
        out.push('\n');
        out.push_str(body.trim());
        out.push('\n');
    }
    out
}

/// Validated proposal scope of `--tenant` (a tenant id or `management`).
pub(crate) fn proposal_scope(raw: &str) -> Result<String, CliError> {
    let scope = normalize_tenant_arg(raw);
    keel_core::validate_dns_label(&scope, "tenant")?;
    Ok(scope)
}

/// `keel proposal list`.
pub async fn execute_proposal_list(
    args: &ProposalListArgs,
    ctx: &CliContext,
) -> Result<String, CliError> {
    let base_url = normalize_server_url(&args.server)?;
    let tenant = args.tenant.as_deref().map(normalize_tenant_arg);
    if let Some(t) = &tenant {
        keel_core::validate_dns_label(t, "tenant")?;
    }
    let query = [("status", args.status.as_str())];
    let path = match tenant.as_deref() {
        Some(t) => format!("/api/v1/tenants/{t}/proposals"),
        None => "/api/v1/proposals".to_string(),
    };
    let payload = get_json(ctx, &base_url, &path, &query).await?;
    if let Some(unavailable) = payload.get("unavailable").and_then(Value::as_array) {
        for repo in unavailable {
            eprintln!(
                "warning: proposals of {} ({}) may be missing: {}",
                s(repo, &["scope"]),
                repo.pointer("/repository/label")
                    .and_then(Value::as_str)
                    .unwrap_or("-"),
                s(repo, &["reason"])
            );
        }
    }
    let proposals = payload
        .get("proposals")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    match args.output {
        OutputFormat::Json => pretty(&Value::Array(proposals)),
        OutputFormat::Table => Ok(format_proposals_table(&proposals)),
    }
}

/// `keel proposal show <pr>`.
pub async fn execute_proposal_show(
    args: &ProposalRefArgs,
    ctx: &CliContext,
) -> Result<String, CliError> {
    let pr = validate_pr_id(&args.pr)?;
    let scope = proposal_scope(&args.tenant)?;
    let base_url = normalize_server_url(&args.server)?;
    let payload = get_json(
        ctx,
        &base_url,
        &format!("/api/v1/tenants/{scope}/proposals"),
        &[("status", "all")],
    )
    .await?;
    let found = payload
        .get("proposals")
        .and_then(Value::as_array)
        .and_then(|list| {
            list.iter()
                .find(|p| p.get("id").and_then(Value::as_str) == Some(pr))
        })
        .cloned()
        .ok_or_else(|| CliError::Api {
            status: 404,
            message: format!(
                "proposal #{pr} not found in the `{scope}` repository (or not visible to you)"
            ),
        })?;
    match args.output {
        OutputFormat::Json => pretty(&found),
        OutputFormat::Table => Ok(format_proposal_detail(&found)),
    }
}

async fn act_on_proposal(
    args: &ProposalRefArgs,
    ctx: &CliContext,
    action: &str,
    verb: &str,
) -> Result<String, CliError> {
    let pr = validate_pr_id(&args.pr)?;
    let scope = proposal_scope(&args.tenant)?;
    let base_url = normalize_server_url(&args.server)?;
    let payload = post_json(
        ctx,
        &base_url,
        &format!("/api/v1/tenants/{scope}/proposals/{pr}/{action}"),
    )
    .await?;
    if args.output == OutputFormat::Json {
        return pretty(&payload);
    }
    let proposal = payload.get("proposal").unwrap_or(&payload);
    let mut out = format!(
        "{verb} proposal #{pr}: {} (state: {})\n",
        s(proposal, &["title"]),
        s(proposal, &["state"])
    );
    if let Some(gc) = payload.get("gcPlan") {
        let n = gc
            .get("targets")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if n > 0 {
            out.push_str(&format!(
                "Garbage-collected {n} ephemeral preview resource(s)\n"
            ));
        }
    }
    Ok(out)
}

/// `keel proposal close <pr>` (server `reject`).
pub async fn execute_proposal_close(
    args: &ProposalRefArgs,
    ctx: &CliContext,
) -> Result<String, CliError> {
    act_on_proposal(args, ctx, "reject", "Closed").await
}

/// `keel proposal merge <pr>`.
pub async fn execute_proposal_merge(
    args: &ProposalRefArgs,
    ctx: &CliContext,
) -> Result<String, CliError> {
    act_on_proposal(args, ctx, "merge", "Merged").await
}

fn catalog_rows(payload: &Value) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for (field, kind) in [
        ("applications", "app"),
        ("coreAddons", "core-addon"),
        ("blueprints", "blueprint"),
        ("bundles", "bundle"),
    ] {
        for item in payload
            .get(field)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let meta = item.get("metadata").unwrap_or(&Value::Null);
            rows.push(vec![
                s(meta, &["id"]).to_string(),
                kind.to_string(),
                s(meta, &["name"]).to_string(),
                s(meta, &["category", "provider", "runtime"]).to_string(),
            ]);
        }
    }
    rows
}

/// `keel catalog list`.
pub async fn execute_catalog_list(
    args: &CatalogListArgs,
    ctx: &CliContext,
) -> Result<String, CliError> {
    let base_url = normalize_server_url(&args.server)?;
    let tenant = args.tenant.as_deref().map(normalize_tenant_arg);
    let query: Vec<(&str, &str)> = tenant
        .as_deref()
        .map(|t| ("tenant", t))
        .into_iter()
        .collect();
    let payload = get_json(ctx, &base_url, "/api/v1/catalog", &query).await?;
    match args.output {
        OutputFormat::Json => {
            let mut trimmed = payload.clone();
            if let Some(obj) = trimmed.as_object_mut() {
                obj.remove("pulumiYaml");
                obj.remove("manifestsYaml");
            }
            pretty(&trimmed)
        }
        OutputFormat::Table => {
            let rows = catalog_rows(&payload);
            if rows.is_empty() {
                return Ok("Catalog is empty.\n".to_string());
            }
            Ok(render_table(&["ID", "KIND", "NAME", "CATEGORY"], &rows))
        }
    }
}

/// `keel catalog show <id>`.
pub async fn execute_catalog_show(
    args: &CatalogShowArgs,
    ctx: &CliContext,
) -> Result<String, CliError> {
    keel_core::validate_dns_label(&args.id, "id")?;
    let base_url = normalize_server_url(&args.server)?;
    let payload = get_json(ctx, &base_url, "/api/v1/catalog", &[]).await?;
    let found = ["applications", "coreAddons", "blueprints", "bundles"]
        .iter()
        .filter_map(|f| payload.get(*f).and_then(Value::as_array))
        .flatten()
        .find(|item| item.pointer("/metadata/id").and_then(Value::as_str) == Some(&args.id))
        .cloned()
        .ok_or_else(|| CliError::Api {
            status: 404,
            message: format!("catalog entry '{}' not found", args.id),
        })?;
    match args.output {
        OutputFormat::Json => pretty(&found),
        OutputFormat::Table => Ok(serde_yml::to_string(&found)?),
    }
}

/// `keel app list` (deployed releases).
pub async fn execute_app_list(args: &AppListArgs, ctx: &CliContext) -> Result<String, CliError> {
    let base_url = normalize_server_url(&args.server)?;
    let tenant = args.tenant.as_deref().map(normalize_tenant_arg);
    if let Some(t) = &tenant {
        keel_core::validate_dns_label(t, "tenant")?;
    }
    let query: Vec<(&str, &str)> = tenant
        .as_deref()
        .map(|t| ("tenant", t))
        .into_iter()
        .collect();
    let payload = get_json(ctx, &base_url, "/api/v1/releases", &query).await?;
    let releases = payload
        .get("releases")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    match args.output {
        OutputFormat::Json => pretty(&Value::Array(releases)),
        OutputFormat::Table => {
            if releases.is_empty() {
                return Ok("No releases found.\n".to_string());
            }
            let rows: Vec<Vec<String>> = releases
                .iter()
                .map(|r| {
                    vec![
                        s(r, &["namespace"]).to_string(),
                        s(r, &["name"]).to_string(),
                        s(r, &["chart"]).to_string(),
                        s(r, &["version"]).to_string(),
                        s(r, &["runtime"]).to_string(),
                        s(r, &["status", "phase", "ready"]).to_string(),
                    ]
                })
                .collect();
            Ok(render_table(
                &["NAMESPACE", "NAME", "CHART", "VERSION", "RUNTIME", "STATUS"],
                &rows,
            ))
        }
    }
}

/// Outcome of one `POST /api/v1/tenants/{id}/credentials/repair` call.
#[derive(Debug)]
pub(crate) enum RepairResult {
    /// The server's step outcomes (never a secret value).
    Repaired(Value),
    /// `409`: the tenant is not provisioned on the cluster yet (stub proposal not merged,
    /// `tenant-<id>` not created) or is being deleted. Not a failure in bulk mode.
    Skipped(String),
    Failed(String),
}

/// Classifies one repair call; `None` stops a bulk run (`400`: tenants have no dedicated
/// repository credential, Local Git provider).
pub(crate) fn classify_repair(result: Result<Value, CliError>) -> Result<RepairResult, CliError> {
    match result {
        Ok(v) => Ok(RepairResult::Repaired(v)),
        Err(CliError::Api {
            status: 409,
            message,
        }) => Ok(RepairResult::Skipped(message)),
        Err(e @ CliError::Api { status: 400, .. }) => Err(e),
        Err(e) => Ok(RepairResult::Failed(e.to_string())),
    }
}

/// One table row of a repair result.
fn repair_row(tenant: &str, result: &RepairResult) -> Vec<String> {
    match result {
        RepairResult::Repaired(v) => vec![
            tenant.to_string(),
            s(v, &["repository"]).to_string(),
            s(v, &["credential"]).to_string(),
            s(v, &["sopsAge"]).to_string(),
            "ok".to_string(),
        ],
        RepairResult::Skipped(why) | RepairResult::Failed(why) => vec![
            tenant.to_string(),
            "-".to_string(),
            "-".to_string(),
            "-".to_string(),
            if matches!(result, RepairResult::Skipped(_)) {
                format!("skipped: {why}")
            } else {
                why.clone()
            },
        ],
    }
}

/// Renders `keel tenant repair-credentials` results as a table.
pub(crate) fn format_credential_repairs(results: &[(String, RepairResult)]) -> String {
    if results.is_empty() {
        return "No tenants found.\n".to_string();
    }
    let rows: Vec<Vec<String>> = results.iter().map(|(t, r)| repair_row(t, r)).collect();
    render_table(
        &["TENANT", "REPOSITORY", "CREDENTIAL", "SOPS-AGE", "RESULT"],
        &rows,
    )
}

/// JSON document of a repair run: `{"results": [...], "failed": n}`.
pub(crate) fn credential_repairs_json(results: &[(String, RepairResult)]) -> Value {
    let failed = results
        .iter()
        .filter(|(_, r)| matches!(r, RepairResult::Failed(_)))
        .count();
    let rows: Vec<Value> = results
        .iter()
        .map(|(t, r)| match r {
            RepairResult::Repaired(v) => v.clone(),
            RepairResult::Skipped(why) => serde_json::json!({ "id": t, "skipped": why }),
            RepairResult::Failed(e) => serde_json::json!({ "id": t, "error": e }),
        })
        .collect();
    serde_json::json!({ "results": rows, "failed": failed })
}

/// `keel tenant repair-credentials [<id>]`: re-mints one tenant's (or every tenant's) repository
/// read credential through `POST /api/v1/tenants/{id}/credentials/repair`. With one id any error
/// fails the command. With every tenant, tenants that are not provisioned yet or are being
/// deleted (`409`) are reported as skipped, other failures are reported per tenant and fail the
/// command once all were attempted (the full report still goes to stdout), and the Local Git
/// provider (`400`, no tenant repository credentials) stops the run.
pub async fn execute_tenant_repair_credentials(
    args: &TenantRepairCredentialsArgs,
    ctx: &CliContext,
) -> Result<String, CliError> {
    let base_url = normalize_server_url(&args.server)?;
    if let Some(t) = args.tenant.as_deref() {
        let tenant = normalize_tenant_arg(t);
        keel_core::validate_dns_label(&tenant, "tenant")?;
        let path = format!("/api/v1/tenants/{tenant}/credentials/repair");
        let v = post_json(ctx, &base_url, &path).await?;
        let results = vec![(tenant, RepairResult::Repaired(v.clone()))];
        return match args.output {
            OutputFormat::Json => pretty(&v),
            OutputFormat::Table => Ok(format_credential_repairs(&results)),
        };
    }
    let tenants: Vec<String> = get_json(ctx, &base_url, "/api/v1/tenants", &[])
        .await?
        .get("tenants")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.get("id").and_then(Value::as_str).map(str::to_string))
        .filter(|id| keel_core::validate_dns_label(id, "tenant").is_ok())
        .collect();
    let mut results = Vec::new();
    for tenant in tenants {
        let path = format!("/api/v1/tenants/{tenant}/credentials/repair");
        let result = classify_repair(post_json(ctx, &base_url, &path).await)?;
        results.push((tenant, result));
    }
    let failed = results
        .iter()
        .filter(|(_, r)| matches!(r, RepairResult::Failed(_)))
        .count();
    let out = match args.output {
        OutputFormat::Json => pretty(&credential_repairs_json(&results))?,
        OutputFormat::Table => format_credential_repairs(&results),
    };
    if failed > 0 {
        return Err(CliError::Incomplete {
            output: out,
            message: format!("{failed} tenant(s) could not be repaired"),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tables_and_ids() {
        let t = format_proposals_table(&[json!({
            "id": "7", "state": "open", "tenant": "default", "title": "feat(default): add app",
            "sourceBranch": "keel/x", "updatedAt": "2026-11-20T00:00:00Z"
        })]);
        assert!(t.starts_with("ID"));
        assert!(t.contains("#7"));
        assert!(t.contains("feat(default): add app"));
        assert_eq!(format_proposals_table(&[]), "No proposals found.\n");
        assert_eq!(validate_pr_id("#42").unwrap(), "42");
        assert!(validate_pr_id("../etc").is_err());
        let rows = catalog_rows(&json!({
            "applications": [{"metadata": {"id": "podinfo", "name": "Podinfo", "category": "demo"}}],
            "blueprints": [{"metadata": {"id": "aws-vm", "name": "AWS VM", "provider": "aws"}}]
        }));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][1], "blueprint");
    }

    #[test]
    fn credential_repair_output_has_no_secrets_and_reports_failures() {
        let results = vec![
            (
                "acme".to_string(),
                RepairResult::Repaired(
                    json!({"id": "acme", "repository": "keel-tenants/tenant-acme",
                          "namespaceReady": true, "credential": "rotated", "sopsAge": "existed"}),
                ),
            ),
            (
                "beta".to_string(),
                RepairResult::Skipped("`tenant-beta` does not exist yet".to_string()),
            ),
            (
                "gamma".to_string(),
                RepairResult::Failed("Server API error (HTTP 502): forge down".to_string()),
            ),
        ];
        let out = format_credential_repairs(&results);
        assert!(out.starts_with("TENANT"), "{out}");
        assert!(out.contains("keel-tenants/tenant-acme"));
        assert!(out.contains("rotated"));
        assert!(out.contains("skipped: `tenant-beta` does not exist yet"));
        assert!(out.contains("HTTP 502"));
        assert_eq!(format_credential_repairs(&[]), "No tenants found.\n");

        let doc = credential_repairs_json(&results);
        assert_eq!(doc["failed"], 1);
        assert_eq!(doc["results"][0]["credential"], "rotated");
        assert_eq!(doc["results"][1]["id"], "beta");
        assert!(doc["results"][1]["skipped"].is_string());
        assert!(doc["results"][2]["error"].is_string());
    }

    #[test]
    fn credential_repair_classifies_pending_and_local_provider_responses() {
        let api = |status, message: &str| CliError::Api {
            status,
            message: message.to_string(),
        };
        assert!(matches!(
            classify_repair(Ok(json!({"id": "a"}))).unwrap(),
            RepairResult::Repaired(_)
        ));
        // Not provisioned yet / being deleted: skipped, not a failure.
        assert!(matches!(
            classify_repair(Err(api(409, "`tenant-a` does not exist yet"))).unwrap(),
            RepairResult::Skipped(m) if m.contains("does not exist yet")
        ));
        // Local Git provider: stops the bulk run with the server's message.
        let err = classify_repair(Err(api(400, "Local Git provider"))).unwrap_err();
        assert!(err.to_string().contains("Local Git provider"), "{err}");
        // Anything else is a per-tenant failure.
        assert!(matches!(
            classify_repair(Err(api(502, "forge down"))).unwrap(),
            RepairResult::Failed(m) if m.contains("forge down")
        ));
    }
}
