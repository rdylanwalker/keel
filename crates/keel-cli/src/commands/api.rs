//! Read/act commands over existing keel-server APIs: `keel proposal list|show|close|merge`,
//! `keel catalog list|show`, and `keel app list` (`SDD.md` §7.2, §7.3).
//!
//! Responses are handled as loose JSON so the CLI tolerates additive server changes.

use serde_json::Value;

use crate::cli::{
    AppListArgs, CatalogListArgs, CatalogShowArgs, OutputFormat, ProposalListArgs, ProposalRefArgs,
    TenantMigrateBaselineArgs, TenantMigrateStackSourcesArgs,
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
                s(p, &["sourceBranch", "source_branch"]).to_string(),
                s(p, &["updatedAt", "updated_at"]).to_string(),
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
        ("Branch", &["sourceBranch", "source_branch"][..]),
        ("Target", &["targetBranch", "target_branch"][..]),
        ("Head SHA", &["headSha", "head_sha"][..]),
        ("URL", &["url"][..]),
        ("Created", &["createdAt", "created_at"][..]),
        ("Updated", &["updatedAt", "updated_at"][..]),
    ] {
        out.push_str(&format!("{label:<10} {}\n", s(p, keys)));
    }
    if let Some(stat) = p
        .get("diffStat")
        .or_else(|| p.get("diff_stat"))
        .and_then(Value::as_str)
    {
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
    if let Some(gc) = payload.get("gcPlan").or_else(|| payload.get("gc_plan")) {
        let n = gc
            .get("deletions")
            .or_else(|| gc.get("stacks"))
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

/// Renders the `POST /api/v1/tenants/{id}/stack-sources/migrate` response.
pub(crate) fn format_stack_source_migration(payload: &Value) -> String {
    let tenant = s(payload, &["tenant"]);
    let strings = |field: &str| -> Vec<String> {
        payload
            .get(field)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    };
    let migrated = strings("migrated");
    let dry_run = payload
        .get("dryRun")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut out = if migrated.is_empty() {
        format!("Tenant `{tenant}`: no Stacks use fluxSource; nothing to migrate.\n")
    } else if dry_run {
        format!(
            "Tenant `{tenant}`: {} Stack(s) would move to PKO gitSource (dry run):\n",
            migrated.len()
        )
    } else {
        format!(
            "Tenant `{tenant}`: opened a PR moving {} Stack(s) to PKO gitSource:\n",
            migrated.len()
        )
    };
    for path in &migrated {
        out.push_str(&format!("  {path}\n"));
    }
    if let Some(pr) = payload.get("proposal").filter(|p| !p.is_null()) {
        out.push_str(&format!(
            "Proposal #{}: {} ({})\nReview and merge it (`keel proposal merge {}`).\n",
            s(pr, &["id"]),
            s(pr, &["title"]),
            s(pr, &["url"]),
            s(pr, &["id"])
        ));
    }
    if let Some(skipped) = payload
        .get("skipped")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    {
        out.push_str("Not migrated (review manually):\n");
        for item in skipped {
            out.push_str(&format!(
                "  {}: {}\n",
                s(item, &["path"]),
                s(item, &["reason"])
            ));
        }
    }
    out
}

/// Human-readable summary of `POST /api/v1/tenants/{id}/baseline/migrate`.
pub(crate) fn format_baseline_migration(payload: &Value) -> String {
    let tenant = s(payload, &["tenant"]);
    let labelled: Vec<String> = payload
        .get("labelled")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let dry_run = payload
        .get("dryRun")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut out = if labelled.is_empty() {
        format!(
            "Tenant `{tenant}`: baseline already labelled keel.io/platform-owned; nothing to do.\n"
        )
    } else if dry_run {
        format!(
            "Tenant `{tenant}`: {} file(s) would be labelled keel.io/platform-owned (dry run):\n",
            labelled.len()
        )
    } else {
        format!(
            "Tenant `{tenant}`: opened a PR labelling {} file(s) keel.io/platform-owned:\n",
            labelled.len()
        )
    };
    for path in &labelled {
        out.push_str(&format!("  {path}\n"));
    }
    if let Some(pr) = payload.get("proposal").filter(|p| !p.is_null()) {
        out.push_str(&format!(
            "Proposal #{}: {} ({})\nReview and merge it (`keel proposal merge {}`).\n",
            s(pr, &["id"]),
            s(pr, &["title"]),
            s(pr, &["url"]),
            s(pr, &["id"])
        ));
    }
    out
}

/// `keel tenant migrate-baseline --tenant <id> [--dry-run]`.
pub async fn execute_tenant_migrate_baseline(
    args: &TenantMigrateBaselineArgs,
    ctx: &CliContext,
) -> Result<String, CliError> {
    let tenant = normalize_tenant_arg(&args.tenant);
    keel_core::validate_dns_label(&tenant, "tenant")?;
    let base_url = normalize_server_url(&args.server)?;
    let query: &[(&str, &str)] = if args.dry_run {
        &[("dryRun", "true")]
    } else {
        &[]
    };
    let payload = post_json_query(
        ctx,
        &base_url,
        &format!("/api/v1/tenants/{tenant}/baseline/migrate"),
        query,
    )
    .await?;
    match args.output {
        OutputFormat::Json => pretty(&payload),
        OutputFormat::Table => Ok(format_baseline_migration(&payload)),
    }
}

/// `keel tenant migrate-stack-sources --tenant <id> [--dry-run]`.
pub async fn execute_tenant_migrate_stack_sources(
    args: &TenantMigrateStackSourcesArgs,
    ctx: &CliContext,
) -> Result<String, CliError> {
    let tenant = normalize_tenant_arg(&args.tenant);
    keel_core::validate_dns_label(&tenant, "tenant")?;
    let base_url = normalize_server_url(&args.server)?;
    let query: &[(&str, &str)] = if args.dry_run {
        &[("dryRun", "true")]
    } else {
        &[]
    };
    let payload = post_json_query(
        ctx,
        &base_url,
        &format!("/api/v1/tenants/{tenant}/stack-sources/migrate"),
        query,
    )
    .await?;
    match args.output {
        OutputFormat::Json => pretty(&payload),
        OutputFormat::Table => Ok(format_stack_source_migration(&payload)),
    }
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
    fn baseline_migration_output() {
        let opened = format_baseline_migration(&json!({
            "tenant": "acme", "dryRun": false,
            "labelled": ["tenants/acme/rbac.yaml"],
            "proposal": {"id": "9", "title": "fix(acme): protect the platform-owned tenant baseline", "url": "http://forge/pr/9"}
        }));
        assert!(
            opened.contains("opened a PR labelling 1 file(s)"),
            "{opened}"
        );
        assert!(opened.contains("tenants/acme/rbac.yaml"));
        assert!(opened.contains("keel proposal merge 9"));
        let dry = format_baseline_migration(&json!({
            "tenant": "acme", "dryRun": true, "labelled": ["tenants/acme/rbac.yaml"]
        }));
        assert!(dry.contains("(dry run)"), "{dry}");
        assert!(!dry.contains("Proposal"));
        let none =
            format_baseline_migration(&json!({"tenant": "acme", "dryRun": false, "labelled": []}));
        assert!(none.contains("nothing to do"), "{none}");
    }

    #[test]
    fn stack_source_migration_output() {
        let opened = format_stack_source_migration(&json!({
            "tenant": "default", "dryRun": false,
            "migrated": ["tenants/default/infra/kind/stack.yaml"],
            "skipped": [{"path": "tenants/default/x/stack.yaml", "reason": "fluxSource is not the tenant's own GitRepository"}],
            "proposal": {"id": "12", "title": "fix(default): clone tenant PKO Stacks with gitSource", "url": "http://forge/pr/12"}
        }));
        assert!(opened.contains("opened a PR moving 1 Stack(s)"), "{opened}");
        assert!(opened.contains("tenants/default/infra/kind/stack.yaml"));
        assert!(opened.contains("keel proposal merge 12"));
        assert!(opened.contains("Not migrated"));
        let dry = format_stack_source_migration(&json!({
            "tenant": "acme", "dryRun": true, "migrated": ["tenants/acme/infra/vm/stack.yaml"], "skipped": []
        }));
        assert!(
            dry.contains("would move to PKO gitSource (dry run)"),
            "{dry}"
        );
        let none = format_stack_source_migration(
            &json!({"tenant": "acme", "migrated": [], "skipped": []}),
        );
        assert!(none.contains("nothing to migrate"), "{none}");
    }
}
