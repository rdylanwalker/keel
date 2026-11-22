//! Stage 0 core add-on installation (Flux, Kyverno, PKO, SeaweedFS, Envoy Gateway, Prometheus,
//! Flagger, cert-manager, Rauthy, optional Forgejo) and seed manifests.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use keel_core::{ClusterConnection, FLUX_SYSTEM_NAMESPACE, KEEL_SYSTEM_NAMESPACE};
use keel_kube::tenant::build_tenant_reconciler_rbac_yaml;

use super::forge::{render_forgejo_admin_secret_yaml, resolve_forgejo_admin_password};
use super::kube::{KubeTarget, find_gateway_service};
use super::s3::{
    ensure_seaweedfs_pulumi_bucket, provision_tenant_s3_identity,
    sync_in_cluster_s3_credentials_from_seaweedfs,
};
use super::state::{
    CONTROL_PLANE_FORGEJO_PULUMI_YAML, CONTROL_PLANE_PULUMI_YAML, CONTROL_PLANE_RAUTHY_PULUMI_YAML,
    Stage0Context, ensure_pulumi_passphrase_secret, run_pulumi_control_plane_stack,
    store_passphrase_secret,
};
use super::{
    DEFAULT_TENANT_ID, SEED_FLUX_GOTK_SYNC_YAML, SeedControlPlaneArtifacts, Stage0Ingress,
    apply_git_credentials_secret, ensure_tenant_baseline, forgejo_stack_config, render_manifest,
    render_rauthy_httproute,
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
const SOURCE_CONTROLLER_ARTIFACTS_POLICY_YAML: &str = include_str!(
    "../../../../../deploy/bootstrap/manifests/source-controller-artifacts-network-policy.yaml"
);
/// `flux-system` NetworkPolicy guarding the `source-controller` artifact server (port 9090).
#[cfg(test)]
pub(crate) const SOURCE_CONTROLLER_ARTIFACTS_POLICY: &str = "allow-source-controller-artifacts";

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

/// Ingress hosts CoreDNS must rewrite to the Envoy Gateway proxy: only hosts the cluster
/// actually serves. The in-cluster Forgejo host (PKO clones it through its HTTPS ingress) when
/// `--with-forge` is set, and the IdP host only for the built-in Rauthy IdP (in-cluster OIDC
/// discovery through the public issuer URL). An external IdP's host (`--oidc-public-url` with
/// `--oidc-issuer`) is never rewritten: it has no Envoy route, so in-cluster OIDC calls would fail.
pub(crate) fn coredns_ingress_rewrite_hosts(
    ingress: &Stage0Ingress,
    with_forge: bool,
    with_rauthy: bool,
) -> Vec<&str> {
    let mut hosts = Vec::new();
    if with_rauthy {
        hosts.push(ingress.idp_host.as_str());
    }
    if with_forge {
        hosts.push(ingress.forgejo_host.as_str());
    }
    hosts
}

/// Writes `keel-system/keel-ca-bundle` (key `ca.crt`) from the **public** certificate of the
/// self-signed Keel CA (`keel-system/keel-ca-secret` `ca.crt`; its `tls.key` is never read into
/// the bundle) and copies it into `keel-previews` and every existing `tenant-*` namespace
/// ([`keel_ca_bundle_namespaces`]). Kyverno `sync-keel-ca-bundle` clones the `keel-system` source
/// into tenant namespaces created later. PKO's operator and the tenant/preview workspace pods mount it to trust the Forgejo
/// HTTPS ingress. Best effort: without a CA (it was not issued in time) nothing is written and the
/// mounts stay empty (`optional: true`); re-running `keel bootstrap init` after a CA rotation
/// refreshes every copy.
async fn ensure_keel_ca_bundle(target: KubeTarget<'_>) {
    let mut ca = None;
    for _ in 0..15 {
        ca = target
            .read_secret_key(
                KEEL_SYSTEM_NAMESPACE,
                keel_kube::KEEL_CA_SECRET_NAME,
                "ca.crt",
            )
            .await;
        if ca.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let Some(ca) = ca else {
        tracing::warn!(
            "keel-system/keel-ca-secret has no ca.crt yet; skipping the keel-ca-bundle ConfigMap (PKO cannot clone the in-cluster Forgejo over HTTPS until it exists; re-run `keel bootstrap init`)"
        );
        return;
    };
    let current = target
        .output(&[
            "-n",
            KEEL_SYSTEM_NAMESPACE,
            "get",
            "configmap",
            keel_core::KEEL_CA_BUNDLE_CONFIGMAP,
            "--ignore-not-found",
            "-o",
            "jsonpath={.data.ca\\.crt}",
        ])
        .await
        .unwrap_or_default();
    let changed =
        keel_kube::validate_public_ca_pem(&ca).is_ok_and(|pem| pem.trim() != current.trim());
    let listed = target
        .output(&["get", "namespaces", "-o", "name"])
        .await
        .unwrap_or_default();
    for namespace in keel_ca_bundle_namespaces(&listed) {
        let namespace = namespace.as_str();
        let applied = keel_kube::build_keel_ca_bundle_configmap(namespace, &ca)
            .map_err(CliError::from)
            .and_then(|doc| serde_yml::to_string(&doc).map_err(CliError::from));
        let result = match applied {
            Ok(yaml) => target.apply(&yaml).await,
            Err(e) => Err(e),
        };
        if let Err(err) = result {
            tracing::warn!(namespace, error = %err, "Failed to write the keel-ca-bundle ConfigMap");
        }
    }
    if changed {
        // Go loads its trust store once per process: restart the PKO operator (installed before
        // the CA existed, or trusting a rotated CA) so it picks up the new bundle.
        restart_pko_operator(target).await;
    }
}

/// Namespaces Stage 0 writes `keel-ca-bundle` into: `keel-system` (the source), `keel-previews`,
/// `tenant-default` and every other existing `tenant-*` namespace (`kubectl get ns -o name`
/// output), so a fresh install does not wait for Kyverno and a rotated CA reaches every copy.
pub(crate) fn keel_ca_bundle_namespaces(listed: &str) -> Vec<String> {
    let mut out = vec![
        KEEL_SYSTEM_NAMESPACE.to_string(),
        keel_core::KEEL_PREVIEWS_NAMESPACE.to_string(),
        format!("tenant-{DEFAULT_TENANT_ID}"),
    ];
    for ns in listed
        .split_whitespace()
        .filter_map(|n| n.strip_prefix("namespace/"))
        .filter(|n| keel_kube::is_keel_ca_bundle_target_namespace(n))
    {
        if !out.iter().any(|o| o == ns) {
            out.push(ns.to_string());
        }
    }
    out
}

/// Deployments in `keel-system` that belong to the PKO release (`pulumi-kubernetes-operator*`).
pub(crate) fn pko_operator_deployments(names: &str) -> Vec<String> {
    names
        .split_whitespace()
        .filter_map(|n| n.strip_prefix("deployment.apps/"))
        .filter(|n| n.starts_with("pulumi-kubernetes-operator"))
        .map(str::to_string)
        .collect()
}

async fn restart_pko_operator(target: KubeTarget<'_>) {
    let names = target
        .output(&[
            "-n",
            KEEL_SYSTEM_NAMESPACE,
            "get",
            "deployments",
            "-o",
            "name",
        ])
        .await
        .unwrap_or_default();
    for name in pko_operator_deployments(&names) {
        let resource = format!("deployment/{name}");
        if let Err(err) = target
            .output(&["-n", KEEL_SYSTEM_NAMESPACE, "rollout", "restart", &resource])
            .await
        {
            tracing::warn!(deployment = %name, error = %err, "Could not restart the PKO operator to trust the Keel CA bundle");
        }
    }
}

/// Best-effort CoreDNS rewrite so in-cluster pods resolve the IdP / Forgejo ingress hosts to the
/// Envoy Gateway proxy service in `keel-system`.
async fn configure_coredns_local_rewrites(target: KubeTarget<'_>, hosts: &[&str]) {
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
    let Some(updated) = rewrite_corefile(&corefile, hosts, &fqdn) else {
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
        // The `keel.io/platform-owned` tenant-default baseline is created BEFORE the Kyverno
        // bundle (`protect-platform-owned-tenant-objects`) and only when `tenant-default` does
        // not exist yet; afterwards only `keel-tenants` (keel-tenant-reconciler) writes it and
        // Stage 0 only writes unlabelled Secrets, so a managed-cluster admin outside
        // `system:masters` is never denied on a re-run.
        ensure_tenant_baseline(target, DEFAULT_TENANT_ID).await?;
        if needs_seaweedfs_sync {
            sync_in_cluster_s3_credentials_from_seaweedfs(target).await?;
            ensure_seaweedfs_pulumi_bucket(target).await?;
            // Also ensures `tenant-default/keel-pulumi-passphrase`.
            provision_tenant_s3_identity(target, DEFAULT_TENANT_ID).await?;
        } else {
            ensure_pulumi_passphrase_secret(target, &format!("tenant-{DEFAULT_TENANT_ID}")).await?;
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

    let ingress_and_rauthy_fut = async {
        // The CA, wildcard Certificate, GatewayClass and `keel-ingress` Gateway are part of the
        // `core-addons` Git layout (cert-manager.yaml / envoy-gateway.yaml) whether or not the
        // built-in Rauthy IdP is enabled, so Stage 0 always applies them too (Git == cluster).
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

        let Some(ref rauthy_cfg) = artifacts.rauthy_config else {
            return Ok::<(), CliError>(());
        };
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
        .await?;
        // Same rendered document as `clusters/management/addons/rauthy/httproute.yaml`.
        tracing::info!("Applying Rauthy HTTPRoute");
        apply_with_webhook_retry(target, &render_rauthy_httproute(ingress)?).await
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
        // Same `ROOT_URL` as the seed `forgejo` HelmRelease (Flux adopts it in place).
        let forgejo_config = forgejo_stack_config(ingress);
        let mut config: Vec<(&str, &str, bool)> = vec![
            ("kubeconfig", &connection.kubeconfig_yaml, true),
            ("kubeContext", kube_context, false),
        ];
        config.extend(
            forgejo_config
                .iter()
                .map(|(key, value)| (*key, value.as_str(), false)),
        );
        run_pulumi_control_plane_stack(
            stage0,
            "control-plane-forgejo",
            CONTROL_PLANE_FORGEJO_PULUMI_YAML,
            &config,
        )
        .await
    };

    tokio::try_join!(infra_setup_fut, ingress_and_rauthy_fut, forgejo_fut)?;

    if with_forge {
        // Same document as the `forgejo-ingress` HTTPRoute in `core-addons/forgejo.yaml`, so the
        // catalog `serviceUrl` resolves with or without the built-in Rauthy IdP.
        target
            .apply(&render_manifest(
                FORGEJO_ROUTE_YAML,
                &ingress.template_vars(),
            )?)
            .await?;
    }
    let rewrite_hosts =
        coredns_ingress_rewrite_hosts(ingress, with_forge, artifacts.rauthy_config.is_some());
    if !rewrite_hosts.is_empty() {
        // In-cluster pods resolve the public ingress hosts to Envoy Gateway: PKO (operator and
        // tenant/preview workspaces) clones the in-cluster Forgejo over
        // `https://<forgejo-host>` (`forge.publicUrl`; PKO rejects the `http://` Service URL), and
        // Forgejo / keel-server reach the built-in Rauthy IdP for OIDC discovery.
        configure_coredns_local_rewrites(target, &rewrite_hosts).await;
    }
    ensure_keel_ca_bundle(target).await;

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
    // `source-controller` artifacts (port 9090, whole-repository tarballs) are reachable only from
    // `flux-system`; tenant and preview Stacks clone through PKO `gitSource`.
    target
        .apply(SOURCE_CONTROLLER_ARTIFACTS_POLICY_YAML)
        .await?;

    let repo_url = &artifacts.flux_git_repository.spec.url;
    if ["http://", "https://", "ssh://", "git://"]
        .iter()
        .any(|p| repo_url.starts_with(p))
    {
        if let Some(token) = git_token.map(str::trim).filter(|t| !t.is_empty()) {
            // Platform `flux-system` source only; tenant-default gets its own repository-scoped
            // credential (`bootstrap::tenant_repo`).
            apply_git_credentials_secret(target, "git", token).await?;
        }
        // Exactly what `clusters/management/flux-system/` holds in Git (see
        // `seed_management_files`): tenant reconciler RBAC, the root GitRepository and the root
        // Flux Kustomizations (`flux-system`, `core-addons`, `keel-tenants`). From here on Flux
        // reconciles the management add-ons and adopts the Stage 0 releases in place.
        for yaml in stage0_flux_sync_manifests(artifacts)? {
            target.apply(&yaml).await?;
        }
    } else {
        tracing::info!(url = %repo_url, "Skipping in-cluster Flux GitRepository apply for local filesystem path");
        notes.push(local_git_flux_handoff_note(repo_url));
    }

    Ok(notes)
}

/// Bootstrap note for a GitOps repository on the local filesystem: Flux cannot reach it, so it
/// never adopts the Stage 0 add-ons, and the Stage 0 programs deliberately ignore version/value
/// changes after the handoff (`ignoreChanges`), so core add-ons have no upgrade path in this mode.
pub(crate) fn local_git_flux_handoff_note(repo_url: &str) -> String {
    format!(
        "GitOps repository `{repo_url}` is a local path that in-cluster Flux cannot reach: the \
         control-plane add-ons stay Stage 0-only (no Flux reconciliation, no management API \
         upgrades/rollbacks, and re-running `keel bootstrap init`/`recover` does not apply newer \
         add-on versions). Push the repository to a network Git remote (`--git-url`) and re-run \
         `keel bootstrap init` to hand the add-ons over to Flux."
    )
}

/// Flux bootstrap manifests applied in-cluster when the GitOps repository is reachable over the
/// network (in order). Identical to the documents written to `clusters/management/flux-system/`.
pub(crate) fn stage0_flux_sync_manifests(
    artifacts: &SeedControlPlaneArtifacts,
) -> Result<Vec<String>, CliError> {
    Ok(vec![
        build_tenant_reconciler_rbac_yaml()?,
        artifacts.flux_git_repository.to_yaml()?,
        SEED_FLUX_GOTK_SYNC_YAML.to_string(),
    ])
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
    fn forgejo_route_in_git_matches_stage0() {
        // Stage 0 applies the same `forgejo-ingress` route Flux reconciles from
        // `core-addons/forgejo.yaml`, so the adoption never rewrites it.
        let ingress = Stage0Ingress::from_public_url("https://idp.corp.example:9443").unwrap();
        let stage0 = docs(&render_manifest(FORGEJO_ROUTE_YAML, &ingress.template_vars()).unwrap());
        let git = docs(&super::super::render_seed_forgejo_helmrelease(&ingress).unwrap());
        let route = git.iter().find(|d| d["kind"] == "HTTPRoute").unwrap();
        assert_eq!(stage0.len(), 1);
        assert_eq!(&stage0[0], route);
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
            (INGRESS_GATEWAY_YAML, vec!["GatewayClass", "Gateway"]),
            (FORGEJO_ROUTE_YAML, vec!["HTTPRoute"]),
            (
                SEED_FLUX_GOTK_SYNC_YAML,
                vec!["Kustomization", "Kustomization", "Kustomization"],
            ),
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
        let route = docs(&render_rauthy_httproute(&ingress).unwrap());
        assert_eq!(route.len(), 1);
        assert_eq!(route[0]["kind"], "HTTPRoute");
        assert_eq!(route[0]["spec"]["hostnames"][0], "idp.example.org");
        let listeners: Vec<&str> = route[0]["spec"]["parentRefs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["sectionName"].as_str().unwrap())
            .collect();
        assert_eq!(listeners, ["https-local", "https"]);
        // Every listener the route binds exists on the Stage 0 Gateway.
        let gw = docs(&render_manifest(INGRESS_GATEWAY_YAML, &vars).unwrap());
        let gw_listeners: Vec<&str> = gw[1]["spec"]["listeners"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["name"].as_str().unwrap())
            .collect();
        assert!(listeners.iter().all(|l| gw_listeners.contains(l)));
        let ca = docs(&render_manifest(LOCAL_CA_YAML, &vars).unwrap());
        assert_eq!(ca[3]["spec"]["dnsNames"][0], "*.example.org");

        let rbac =
            docs(&render_manifest(MANAGEMENT_PKO_RBAC_YAML, &[("MGMT_SA", "mgmt-sa")]).unwrap());
        assert_eq!(rbac[0]["metadata"]["name"], "mgmt-sa");
        assert_eq!(rbac[1]["subjects"][0]["name"], "mgmt-sa");
        let kinds: Vec<&str> = rbac.iter().map(|d| d["kind"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            ["ServiceAccount", "ClusterRoleBinding", "NetworkPolicy"]
        );

        let note = local_git_flux_handoff_note("/tmp/keel-gitops.git");
        assert!(note.contains("/tmp/keel-gitops.git"));
        assert!(note.contains("--git-url"));

        let fl = docs(FLAGGER_METRIC_TEMPLATES_YAML);
        assert!(
            fl[0]["spec"]["query"]
                .as_str()
                .unwrap()
                .contains("{{ namespace }}")
        );
    }

    /// Ingress rules (`from` peers, `ports`) of every NetworkPolicy selecting `source-controller`.
    fn source_controller_ingress(docs: &[Value]) -> Vec<(Vec<Value>, Vec<Value>)> {
        docs.iter()
            .filter(|d| d["kind"] == "NetworkPolicy")
            .filter(|d| d["spec"]["podSelector"]["matchLabels"]["app"] == "source-controller")
            .flat_map(|d| d["spec"]["ingress"].as_array().cloned().unwrap_or_default())
            .map(|rule| {
                (
                    rule["from"].as_array().cloned().unwrap_or_default(),
                    rule["ports"].as_array().cloned().unwrap_or_default(),
                )
            })
            .collect()
    }

    #[test]
    fn source_controller_artifacts_are_reachable_only_from_flux_system() {
        let policy = docs(SOURCE_CONTROLLER_ARTIFACTS_POLICY_YAML);
        assert_eq!(policy.len(), 1);
        assert_eq!(
            policy[0]["metadata"]["name"],
            SOURCE_CONTROLLER_ARTIFACTS_POLICY
        );
        assert_eq!(policy[0]["metadata"]["namespace"], FLUX_SYSTEM_NAMESPACE);
        let rules = source_controller_ingress(&policy);
        assert!(!rules.is_empty());
        for (from, ports) in &rules {
            let artifact_port = ports.is_empty()
                || ports
                    .iter()
                    .any(|p| p["port"] == 9090 || p["port"] == "http");
            if !artifact_port {
                continue;
            }
            // Every peer that may reach port 9090 is the `flux-system` namespace, never `{}`, a
            // pod selector across namespaces, an ipBlock or a missing `from` (= everyone).
            assert!(!from.is_empty(), "port 9090 open to every source");
            for peer in from {
                assert_eq!(
                    peer,
                    &serde_json::json!({
                        "namespaceSelector": {
                            "matchLabels": { "kubernetes.io/metadata.name": FLUX_SYSTEM_NAMESPACE }
                        }
                    }),
                    "port 9090 reachable from {peer}"
                );
            }
        }

        // No other Stage 0 manifest re-opens the artifact port.
        let ingress = Stage0Ingress::from_public_url("https://idp.example.org").unwrap();
        let vars = ingress.template_vars();
        for tpl in [
            MANAGEMENT_PKO_RBAC_YAML,
            PREVIEW_WORKSPACE_RBAC_YAML,
            LOCAL_DOCKER_PROXY_YAML,
            INGRESS_GATEWAY_YAML,
            FORGEJO_ROUTE_YAML,
        ] {
            let mut all_vars = vars.clone();
            all_vars.push(("MGMT_SA", "mgmt-sa"));
            let rendered = render_manifest(tpl, &all_vars).unwrap();
            assert!(
                source_controller_ingress(&docs(&rendered)).is_empty(),
                "unexpected source-controller NetworkPolicy in {rendered}"
            );
        }
    }

    #[test]
    fn coredns_ingress_rewrites_follow_the_forge_not_only_rauthy() {
        let ingress = Stage0Ingress::from_public_url("https://idp.example.org").unwrap();
        let idp = ingress.idp_host.as_str();
        let forgejo = ingress.forgejo_host.as_str();
        assert_eq!(
            coredns_ingress_rewrite_hosts(&ingress, true, true),
            vec![idp, forgejo]
        );
        // External IdP (`--oidc-issuer`, Rauthy disabled): its host has no Envoy route, so
        // only the in-cluster Forgejo host is rewritten.
        assert_eq!(
            coredns_ingress_rewrite_hosts(&ingress, true, false),
            vec![forgejo]
        );
        assert_eq!(
            coredns_ingress_rewrite_hosts(&ingress, false, true),
            vec![idp]
        );
        assert!(coredns_ingress_rewrite_hosts(&ingress, false, false).is_empty());
    }

    #[test]
    fn keel_ca_bundle_goes_to_keel_system_previews_and_every_tenant() {
        let listed = "namespace/default\nnamespace/flux-system\nnamespace/keel-system\n\
                      namespace/keel-previews\nnamespace/tenant-acme\nnamespace/tenant-default\n";
        assert_eq!(
            keel_ca_bundle_namespaces(listed),
            [
                "keel-system",
                "keel-previews",
                "tenant-default",
                "tenant-acme"
            ]
        );
        assert_eq!(
            keel_ca_bundle_namespaces(""),
            ["keel-system", "keel-previews", "tenant-default"]
        );
    }

    #[test]
    fn pko_operator_deployments_are_selected_by_release_prefix() {
        let names = "deployment.apps/pulumi-kubernetes-operator-controller-manager\n\
                     deployment.apps/keel-server\ndeployment.apps/forgejo\n";
        assert_eq!(
            pko_operator_deployments(names),
            vec!["pulumi-kubernetes-operator-controller-manager".to_string()]
        );
        assert!(pko_operator_deployments("").is_empty());
    }

    #[test]
    fn stage0_never_rewrites_an_existing_tenant_baseline() {
        use crate::commands::bootstrap::{TenantBaselineAction, tenant_baseline_action};
        // Existing namespace: never re-applied (keel-tenants owns it), policy or not.
        assert_eq!(
            tenant_baseline_action(true, false),
            TenantBaselineAction::Skip
        );
        assert_eq!(
            tenant_baseline_action(true, true),
            TenantBaselineAction::Skip
        );
        // First bootstrap (before the Kyverno bundle): the operator creates it.
        assert_eq!(
            tenant_baseline_action(false, false),
            TenantBaselineAction::ApplyAsOperator
        );
        // Policy already live: never as the operator, only as the confined tenant reconciler.
        assert_eq!(
            tenant_baseline_action(false, true),
            TenantBaselineAction::ApplyAs(
                "system:serviceaccount:flux-system:keel-tenant-reconciler".to_string()
            )
        );
        for (exists, installed) in [(true, true), (false, true)] {
            assert_ne!(
                tenant_baseline_action(exists, installed),
                TenantBaselineAction::ApplyAsOperator
            );
        }
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
