//! Stage 0 core add-on installation (Flux, Kyverno, PKO, SeaweedFS, Envoy Gateway, Prometheus,
//! Flagger, cert-manager, Rauthy, optional Forgejo) and seed manifests.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use keel_core::{ClusterConnection, FLUX_SYSTEM_NAMESPACE, KEEL_SYSTEM_NAMESPACE};
use keel_kube::kyverno::{POLICY_SYNC_TENANT_SECRETS, RETIRED_SYNC_TENANT_SECRETS_RBAC};
use keel_kube::tenant::build_tenant_reconciler_rbac_yaml;

use super::forge::{render_forgejo_admin_secret_yaml, resolve_forgejo_admin_password};
use super::kube::{KubeTarget, find_gateway_service};
use super::s3::{
    ensure_seaweedfs_pulumi_bucket, provision_tenant_s3_identity,
    sync_in_cluster_s3_credentials_from_seaweedfs,
};
use super::state::{
    CONTROL_PLANE_FORGEJO_PULUMI_YAML, CONTROL_PLANE_PULUMI_YAML, CONTROL_PLANE_RAUTHY_PULUMI_YAML,
    Stage0Context, ensure_pulumi_passphrase_secret, migrate_tenant_state,
    run_pulumi_control_plane_stack, store_passphrase_secret,
};
use super::{
    DEFAULT_TENANT_ID, SeedControlPlaneArtifacts, Stage0Ingress, TENANT_GIT_TOKEN_ENV,
    apply_git_credentials_secret, apply_tenant_git_credentials_secret, render_manifest,
};
use crate::commands::CliError;

const FLAGGER_METRIC_TEMPLATES_YAML: &str =
    include_str!("../../../../../deploy/bootstrap/manifests/flagger-metric-templates.yaml");
const LOCAL_DOCKER_PROXY_YAML: &str =
    include_str!("../../../../../deploy/bootstrap/manifests/local-docker-proxy.yaml");
const LOCAL_CA_YAML: &str = include_str!("../../../../../deploy/bootstrap/manifests/local-ca.yaml");
const INGRESS_GATEWAY_YAML: &str =
    include_str!("../../../../../deploy/bootstrap/manifests/ingress-gateway.yaml");
const FORGEJO_ROUTE_YAML: &str =
    include_str!("../../../../../deploy/bootstrap/manifests/forgejo-route.yaml");
const MANAGEMENT_PKO_RBAC_YAML: &str =
    include_str!("../../../../../deploy/bootstrap/manifests/management-pko-rbac.yaml");
const PREVIEW_WORKSPACE_RBAC_YAML: &str =
    include_str!("../../../../../deploy/bootstrap/manifests/preview-workspace-rbac.yaml");
const TENANTS_KUSTOMIZATION_YAML: &str =
    include_str!("../../../../../deploy/bootstrap/manifests/tenants-kustomization.yaml");

/// Retries `kubectl apply` while Kyverno's admission webhook is still starting.
async fn apply_with_webhook_retry(target: KubeTarget<'_>, yaml: &str) -> Result<(), CliError> {
    for attempt in 1..=6 {
        match target.apply(yaml).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                let msg = err.to_string();
                let transient = msg.contains("failed calling webhook")
                    || msg.contains("context deadline exceeded")
                    || msg.contains("connection refused")
                    || msg.contains("no matches for kind");
                if transient && attempt < 6 {
                    tracing::warn!(
                        attempt,
                        "Admission webhook / CRDs not ready; retrying in 5s: {msg}"
                    );
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
                return Err(err);
            }
        }
    }
    unreachable!("loop always returns")
}

/// Best-effort removal of the retired `sync-tenant-secrets` policy (+ RBAC) and of control-plane
/// admin credentials it previously cloned into `tenant-*` namespaces.
async fn purge_legacy_tenant_admin_secrets(target: KubeTarget<'_>) {
    for (kind, name) in [
        (
            "generatingpolicies.policies.kyverno.io",
            POLICY_SYNC_TENANT_SECRETS,
        ),
        ("clusterrolebinding", RETIRED_SYNC_TENANT_SECRETS_RBAC),
        ("clusterrole", RETIRED_SYNC_TENANT_SECRETS_RBAC),
    ] {
        if let Err(err) = target
            .output(&["delete", kind, name, "--ignore-not-found"])
            .await
        {
            tracing::warn!(kind, name, error = %err, "Could not remove retired Kyverno secret-sync resource");
        }
    }
    let listing = match target
        .output(&[
            "get",
            "secrets",
            "-A",
            "-l",
            "keel.io/secret-kind in (s3-credentials,git-credentials)",
            "-o",
            "go-template={{range .items}}{{.metadata.namespace}}/{{.metadata.name}}{{\"\\n\"}}{{end}}",
        ])
        .await
    {
        Ok(out) => out,
        Err(err) => {
            tracing::warn!(error = %err, "Could not list legacy tenant credential Secrets");
            return;
        }
    };
    for line in listing.lines() {
        let Some((ns, name)) = line.trim().split_once('/') else {
            continue;
        };
        if !ns.starts_with("tenant-") {
            continue;
        }
        tracing::info!(
            namespace = ns,
            secret = name,
            "Removing legacy cloned control-plane credential from tenant namespace"
        );
        if let Err(err) = target
            .output(&["-n", ns, "delete", "secret", name, "--ignore-not-found"])
            .await
        {
            tracing::warn!(namespace = ns, secret = name, error = %err, "Failed to delete legacy tenant credential");
        }
    }
}

/// Renders the CoreDNS Corefile with rewrites for the Stage 0 ingress hosts (idempotent).
pub(crate) fn rewrite_corefile(
    corefile: &str,
    hosts: &[&str],
    target_fqdn: &str,
) -> Option<String> {
    let missing: Vec<&&str> = hosts
        .iter()
        .filter(|h| !corefile.contains(&format!("rewrite name exact {h} ")))
        .collect();
    if missing.is_empty() {
        return None;
    }
    let mut block = String::new();
    for host in missing {
        block.push_str(&format!("    rewrite name exact {host} {target_fqdn}\n"));
    }
    block.push_str("    kubernetes cluster.local");
    let updated = corefile.replacen("    kubernetes cluster.local", &block, 1);
    (updated != corefile).then_some(updated)
}

/// Best-effort CoreDNS rewrite so in-cluster pods resolve the IdP / Forgejo ingress hosts to the
/// Envoy Gateway proxy service in `keel-system`.
async fn configure_coredns_local_rewrites(target: KubeTarget<'_>, ingress: &Stage0Ingress) {
    let mut svc_name = None;
    for _ in 0..15 {
        svc_name = find_gateway_service(target).await;
        if svc_name.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let Some(svc_name) = svc_name else {
        tracing::warn!(
            "Envoy Gateway service for keel-ingress not ready; skipping CoreDNS rewrites (in-cluster SSO discovery may fail)"
        );
        return;
    };
    let corefile = match target
        .output(&[
            "-n",
            "kube-system",
            "get",
            "configmap",
            "coredns",
            "-o",
            "jsonpath={.data.Corefile}",
        ])
        .await
    {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(error = %err, "Could not read CoreDNS Corefile; skipping rewrites");
            return;
        }
    };
    let fqdn = format!("{svc_name}.{KEEL_SYSTEM_NAMESPACE}.svc.cluster.local");
    let Some(updated) = rewrite_corefile(
        &corefile,
        &[ingress.idp_host.as_str(), ingress.forgejo_host.as_str()],
        &fqdn,
    ) else {
        return;
    };
    let cm = serde_json::json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": { "name": "coredns", "namespace": "kube-system" },
        "data": { "Corefile": updated },
    });
    let applied = match serde_yml::to_string(&cm) {
        Ok(yaml) => target.apply(&yaml).await,
        Err(e) => Err(e.into()),
    };
    if let Err(err) = applied {
        tracing::warn!(error = %err, "Failed to apply CoreDNS rewrites");
        return;
    }
    for args in [
        [
            "-n",
            "kube-system",
            "rollout",
            "restart",
            "deployment/coredns",
        ]
        .as_slice(),
        [
            "-n",
            "kube-system",
            "rollout",
            "status",
            "deployment/coredns",
            "--timeout=60s",
        ]
        .as_slice(),
    ] {
        if let Err(err) = target.output(args).await {
            tracing::warn!(error = %err, "CoreDNS rollout did not complete");
        }
    }
    tracing::info!(envoy_svc = %svc_name, "Configured CoreDNS rewrites for Stage 0 ingress hosts");
}

/// Installs Stage 0 core control-plane controllers and seed manifests onto the cluster.
pub(crate) async fn install_stage0_core_addons(
    connection: &ClusterConnection,
    artifacts: &SeedControlPlaneArtifacts,
    stage0: &Stage0Context,
    with_forge: bool,
    git_token: Option<&str>,
) -> Result<Vec<String>, CliError> {
    let target = KubeTarget::new(
        connection.context.as_deref(),
        connection.kubeconfig_path.as_deref(),
    );
    let mut notes = Vec::new();

    let sops_age_key = match target
        .read_secret_key(FLUX_SYSTEM_NAMESPACE, "sops-age", "age.agekey")
        .await
    {
        Some(existing) => existing,
        None => artifacts.sops_age_keyfile.clone().unwrap_or_default(),
    };
    let enable_policy_exceptions = if artifacts.kyverno_bundle.policy_exception.is_some() {
        "true"
    } else {
        "false"
    };
    let kube_context = connection.context.as_deref().unwrap_or("");

    tracing::info!(context = ?connection.context, "Deploying Stage 0 seed control-plane addons via Pulumi (`control-plane`)");
    run_pulumi_control_plane_stack(
        stage0,
        "control-plane",
        CONTROL_PLANE_PULUMI_YAML,
        &[
            ("kubeconfig", &connection.kubeconfig_yaml, true),
            ("kubeContext", kube_context, false),
            ("sopsAgeKey", &sops_age_key, true),
            (
                "enableKyvernoPolicyExceptions",
                enable_policy_exceptions,
                false,
            ),
        ],
    )
    .await?;
    // keel-system now exists: persist the passphrase in-cluster as a second escrow copy.
    store_passphrase_secret(target, stage0).await?;
    // Management PKO Stacks (`flux-system`, `keel-system`) encrypt their Pulumi state with a
    // namespace-local generated passphrase (create-if-absent, never rotated).
    for ns in [FLUX_SYSTEM_NAMESPACE, KEEL_SYSTEM_NAMESPACE] {
        ensure_pulumi_passphrase_secret(target, ns).await?;
    }
    // keel-previews workspace identity (TokenReview + read-only `view`, no Secrets).
    target.apply(PREVIEW_WORKSPACE_RBAC_YAML).await?;

    let needs_seaweedfs_sync = artifacts
        .management_stack
        .spec
        .backend
        .as_deref()
        .is_some_and(|b| b.contains("s3-state-backend-seaweedfs-s3.keel-system.svc.cluster.local"));

    let kyverno_yaml = artifacts.kyverno_bundle.to_multi_doc_yaml()?;
    let ingress = &artifacts.ingress;

    let infra_setup_fut = async {
        if needs_seaweedfs_sync {
            sync_in_cluster_s3_credentials_from_seaweedfs(target).await?;
            ensure_seaweedfs_pulumi_bucket(target).await?;
            purge_legacy_tenant_admin_secrets(target).await;
            // Also ensures `tenant-default/keel-pulumi-passphrase`.
            provision_tenant_s3_identity(target, DEFAULT_TENANT_ID).await?;
            // OPEN-25: move any legacy bucket-root state of tenant-default under tenants/default/.
            match migrate_tenant_state(target, DEFAULT_TENANT_ID, false).await {
                Ok(report)
                    if report
                        .stacks
                        .iter()
                        .any(|s| s.action.starts_with("migrated")) =>
                {
                    tracing::info!("{}", report.render());
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(
                    error = %err,
                    "Legacy tenant-default state migration failed; run `keel tenant migrate-state --tenant default` manually"
                ),
            }
        } else if let Err(err) =
            ensure_pulumi_passphrase_secret(target, &format!("tenant-{DEFAULT_TENANT_ID}")).await
        {
            tracing::warn!(
                error = %err,
                "tenant-default Pulumi state passphrase deferred until the namespace exists"
            );
        }
        apply_with_webhook_retry(target, FLAGGER_METRIC_TEMPLATES_YAML).await?;
        tracing::info!("Applying Kyverno policies");
        apply_with_webhook_retry(target, &kyverno_yaml).await?;
        if artifacts.kyverno_bundle.policy_exception.is_some() {
            tracing::info!(
                "Deploying local-dev Docker socket proxy in flux-system for nested kind"
            );
            target.apply(LOCAL_DOCKER_PROXY_YAML).await?;
        }
        Ok::<(), CliError>(())
    };

    let rauthy_fut = async {
        let Some(ref rauthy_cfg) = artifacts.rauthy_config else {
            return Ok::<(), CliError>(());
        };
        tracing::info!("Applying cert-manager self-signed CA & keel-wildcard-tls Certificate");
        apply_with_webhook_retry(
            target,
            &render_manifest(LOCAL_CA_YAML, &ingress.template_vars())?,
        )
        .await?;
        for _ in 0..30 {
            if target
                .read_secret_key(KEEL_SYSTEM_NAMESPACE, "keel-wildcard-tls", "tls.crt")
                .await
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        tracing::info!("Applying Envoy GatewayClass & keel-ingress Gateway");
        apply_with_webhook_retry(
            target,
            &render_manifest(INGRESS_GATEWAY_YAML, &ingress.template_vars())?,
        )
        .await?;

        let groups_b64 = STANDARD.encode(rauthy_cfg.groups_json.as_bytes());
        let users_b64 = STANDARD.encode(rauthy_cfg.users_json.as_bytes());
        let clients_b64 = STANDARD.encode(rauthy_cfg.clients_json.as_bytes());
        tracing::info!(
            "Installing Stage 0 Rauthy OIDC provider via Pulumi (`control-plane-rauthy`)"
        );
        run_pulumi_control_plane_stack(
            stage0,
            "control-plane-rauthy",
            CONTROL_PLANE_RAUTHY_PULUMI_YAML,
            &[
                ("kubeconfig", &connection.kubeconfig_yaml, true),
                ("kubeContext", kube_context, false),
                ("rauthyConfigToml", &rauthy_cfg.config_toml, true),
                ("rauthyGroupsB64", &groups_b64, false),
                ("rauthyUsersB64", &users_b64, true),
                ("rauthyClientsB64", &clients_b64, true),
                ("rauthySecretName", &rauthy_cfg.secret_name, false),
                ("rauthySecretNamespace", &rauthy_cfg.secret_namespace, false),
            ],
        )
        .await
    };

    let forgejo_fut = async {
        if !with_forge {
            return Ok::<(), CliError>(());
        }
        tracing::info!(
            "Installing in-cluster Forgejo Git forge via Pulumi (`control-plane-forgejo`)"
        );
        let password = resolve_forgejo_admin_password(target).await;
        target
            .apply(&render_forgejo_admin_secret_yaml(&password)?)
            .await?;
        run_pulumi_control_plane_stack(
            stage0,
            "control-plane-forgejo",
            CONTROL_PLANE_FORGEJO_PULUMI_YAML,
            &[
                ("kubeconfig", &connection.kubeconfig_yaml, true),
                ("kubeContext", kube_context, false),
            ],
        )
        .await
    };

    tokio::try_join!(infra_setup_fut, rauthy_fut, forgejo_fut)?;

    if with_forge && artifacts.rauthy_config.is_some() {
        target
            .apply(&render_manifest(
                FORGEJO_ROUTE_YAML,
                &ingress.template_vars(),
            )?)
            .await?;
        configure_coredns_local_rewrites(target, ingress).await;
    }

    let mgmt_sa_name = artifacts
        .management_stack
        .spec
        .service_account_name
        .clone()
        .unwrap_or_else(|| "keel-management-pko".to_string());
    target
        .apply(&render_manifest(
            MANAGEMENT_PKO_RBAC_YAML,
            &[("MGMT_SA", mgmt_sa_name.as_str())],
        )?)
        .await?;
    apply_with_webhook_retry(target, &artifacts.management_stack.to_yaml()?).await?;

    let repo_url = &artifacts.flux_git_repository.spec.url;
    if ["http://", "https://", "ssh://", "git://"]
        .iter()
        .any(|p| repo_url.starts_with(p))
    {
        if let Some(token) = git_token.map(str::trim).filter(|t| !t.is_empty()) {
            apply_git_credentials_secret(target, "git", token).await?;
            match std::env::var(TENANT_GIT_TOKEN_ENV)
                .ok()
                .filter(|t| !t.trim().is_empty())
            {
                Some(ro_token) => {
                    apply_tenant_git_credentials_secret(target, DEFAULT_TENANT_ID, "git", ro_token.trim())
                        .await?
                }
                None => notes.push(format!(
                    "`{TENANT_GIT_TOKEN_ENV}` is unset: the operator `--git-token` is NOT shared with tenant-default; set a read-only token there if the tenant GitRepository needs authentication"
                )),
            }
        }
        target.apply(&build_tenant_reconciler_rbac_yaml()?).await?;
        target
            .apply(&artifacts.flux_git_repository.to_yaml()?)
            .await?;
        target.apply(TENANTS_KUSTOMIZATION_YAML).await?;
    } else {
        tracing::info!(url = %repo_url, "Skipping in-cluster Flux GitRepository apply for local filesystem path");
    }

    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn docs(yaml: &str) -> Vec<Value> {
        yaml.split("\n---\n")
            .filter(|d| !d.trim().is_empty())
            .map(|d| serde_yml::from_str::<Value>(d).unwrap())
            .collect()
    }

    #[test]
    fn embedded_manifests_parse_and_render() {
        let ingress = Stage0Ingress::from_public_url("https://idp.example.org").unwrap();
        let vars = ingress.template_vars();
        for (tpl, kinds) in [
            (
                FLAGGER_METRIC_TEMPLATES_YAML,
                vec!["MetricTemplate", "MetricTemplate"],
            ),
            (
                LOCAL_DOCKER_PROXY_YAML,
                vec!["Deployment", "Service", "NetworkPolicy"],
            ),
            (
                LOCAL_CA_YAML,
                vec![
                    "ClusterIssuer",
                    "Certificate",
                    "ClusterIssuer",
                    "Certificate",
                ],
            ),
            (
                INGRESS_GATEWAY_YAML,
                vec!["GatewayClass", "Gateway", "HTTPRoute"],
            ),
            (FORGEJO_ROUTE_YAML, vec!["HTTPRoute"]),
            (TENANTS_KUSTOMIZATION_YAML, vec!["Kustomization"]),
            (
                PREVIEW_WORKSPACE_RBAC_YAML,
                vec!["ServiceAccount", "ClusterRoleBinding", "ClusterRoleBinding"],
            ),
        ] {
            let rendered = render_manifest(tpl, &vars).unwrap();
            assert!(
                !rendered.contains("${"),
                "unrendered placeholder in {rendered}"
            );
            let parsed = docs(&rendered);
            let got: Vec<&str> = parsed.iter().map(|d| d["kind"].as_str().unwrap()).collect();
            assert_eq!(got, kinds);
        }
        let gw = docs(&render_manifest(INGRESS_GATEWAY_YAML, &vars).unwrap());
        assert_eq!(gw[2]["spec"]["hostnames"][0], "idp.example.org");
        let ca = docs(&render_manifest(LOCAL_CA_YAML, &vars).unwrap());
        assert_eq!(ca[3]["spec"]["dnsNames"][0], "*.example.org");

        let rbac =
            docs(&render_manifest(MANAGEMENT_PKO_RBAC_YAML, &[("MGMT_SA", "mgmt-sa")]).unwrap());
        assert_eq!(rbac[0]["metadata"]["name"], "mgmt-sa");
        assert_eq!(rbac[1]["subjects"][0]["name"], "mgmt-sa");

        let ks = docs(TENANTS_KUSTOMIZATION_YAML);
        assert_eq!(
            ks[0]["spec"]["serviceAccountName"],
            keel_kube::tenant::TENANT_RECONCILER_SERVICE_ACCOUNT
        );
        let fl = docs(FLAGGER_METRIC_TEMPLATES_YAML);
        assert!(
            fl[0]["spec"]["query"]
                .as_str()
                .unwrap()
                .contains("{{ namespace }}")
        );
    }

    #[test]
    fn corefile_rewrites_are_idempotent() {
        let core = ".:53 {\n    errors\n    kubernetes cluster.local in-addr.arpa {\n    }\n}\n";
        let updated = rewrite_corefile(
            core,
            &["idp.keel.local", "forgejo.keel.local"],
            "gw.keel-system.svc.cluster.local",
        )
        .unwrap();
        assert!(
            updated.contains("rewrite name exact idp.keel.local gw.keel-system.svc.cluster.local")
        );
        assert!(
            rewrite_corefile(&updated, &["idp.keel.local", "forgejo.keel.local"], "x").is_none()
        );
    }
}
