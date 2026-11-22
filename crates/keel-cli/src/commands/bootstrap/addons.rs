//! Stage 0 core add-on installation (Flux, Kyverno, PKO, SeaweedFS, Envoy Gateway, Prometheus,
//! Flagger, cert-manager, Rauthy, optional Forgejo) and seed manifests.

use std::path::Path;
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
    DEFAULT_TENANT_ID, SEED_FLUX_GOTK_SYNC_YAML, SeedControlPlaneArtifacts, Stage0Ingress,
    TENANT_GIT_TOKEN_ENV, apply_git_credentials_secret, apply_tenant_git_credentials_secret,
    ensure_tenant_baseline, forgejo_stack_config, render_manifest, render_rauthy_httproute,
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
    repo_dir: Option<&Path>,
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
    if let Some(note) = restrict_source_controller_artifacts(target, repo_dir).await? {
        notes.push(note);
    }

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
                    "`{TENANT_GIT_TOKEN_ENV}` is unset: the operator `--git-token` is NOT shared with tenant-default; set a read-only token there if the tenant GitRepository needs authentication (tenant-default PKO previews are refused until it exists)"
                )),
            }
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

/// PKO `Stack`s (`<namespace>/<name>`) that still download `source-controller` artifacts from
/// outside `flux-system`: any `spec.fluxSource` Stack not in `flux-system` (tenant Stacks committed
/// by earlier releases and legacy `preview-source-pr-<id>` previews in `keel-previews`). The
/// protected `flux-system/management-cluster-core` Stack is same-namespace and never blocks.
pub(crate) fn stacks_requiring_cross_namespace_artifacts(
    stacks: &[serde_json::Value],
) -> Vec<String> {
    keel_kube::legacy_flux_source_stacks(stacks, |ns| ns != FLUX_SYSTEM_NAMESPACE)
        .into_iter()
        .map(|(ns, name)| format!("{ns}/{name}"))
        .collect()
}

/// Parses the PKO `Stack` documents of one `tenants/**` YAML file. `rel_path` is relative to
/// `tenants/`; a Stack without `metadata.namespace` is attributed to `tenant-<id>` from its first
/// path component. Unparsable documents and non-Stack kinds are skipped.
fn stacks_in_tenant_file(rel_path: &Path, content: &str) -> Vec<serde_json::Value> {
    let tenant_ns = rel_path
        .components()
        .next()
        .filter(|_| rel_path.components().count() > 1)
        .and_then(|c| c.as_os_str().to_str())
        .map(|id| format!("tenant-{id}"));
    let mut out = Vec::new();
    for doc in keel_core::split_yaml_documents(content) {
        let Ok(mut value) = serde_yml::from_str::<serde_json::Value>(&doc) else {
            continue;
        };
        let is_stack = value["kind"] == "Stack"
            && value["apiVersion"]
                .as_str()
                .is_some_and(|v| v.starts_with("pulumi.com/"));
        if !is_stack {
            continue;
        }
        if value.pointer("/metadata/namespace").is_none()
            && let (Some(ns), Some(meta)) = (tenant_ns.as_ref(), value.get_mut("metadata"))
            && let Some(meta) = meta.as_object_mut()
        {
            meta.insert("namespace".to_string(), serde_json::json!(ns));
        }
        out.push(value);
    }
    out
}

fn is_yaml_name(name: &str) -> bool {
    name.ends_with(".yaml") || name.ends_with(".yml")
}

/// PKO `Stack` documents in `tenants/**` of the bootstrap GitOps working copy (`--repo-dir`) as
/// checked out. Fallback for [`committed_gate_stacks`] when `origin/main` cannot be fetched.
/// Symlinks and unparsable documents are skipped.
pub(crate) fn stack_manifests_in_working_copy(repo_dir: &Path) -> Vec<serde_json::Value> {
    let tenants = repo_dir.join("tenants");
    let mut out = Vec::new();
    let mut pending = vec![tenants.clone()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                pending.push(path);
                continue;
            }
            let yaml = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_yaml_name);
            if !meta.is_file() || !yaml {
                continue;
            }
            let (Ok(content), Ok(rel)) =
                (std::fs::read_to_string(&path), path.strip_prefix(&tenants))
            else {
                continue;
            };
            out.extend(stacks_in_tenant_file(rel, &content));
        }
    }
    out
}

/// PKO `Stack` documents in `tenants/**` at `rev` of `repo_dir` (read with `git ls-tree`/`git
/// show`, never touching the worktree).
async fn stack_manifests_at_ref(
    git: &keel_git::GitRunner,
    repo_dir: &Path,
    rev: &str,
) -> Result<Vec<serde_json::Value>, keel_git::GitError> {
    let mut out = Vec::new();
    let mut pending = vec!["tenants".to_string()];
    while let Some(dir) = pending.pop() {
        for name in git.list_dir_at_ref(repo_dir, rev, &dir).await? {
            let path = format!("{dir}/{name}");
            if !is_yaml_name(&name) {
                // A blob lists as empty, so only real subtrees are descended.
                pending.push(path);
                continue;
            }
            if let Some(content) = git.show_file_at_ref(repo_dir, rev, &path).await? {
                let rel = Path::new(&path)
                    .strip_prefix("tenants")
                    .unwrap_or(Path::new(&path));
                out.extend(stacks_in_tenant_file(rel, &content));
            }
        }
    }
    Ok(out)
}

/// Where the committed-Stack half of the port 9090 gate was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommittedStacksSource {
    /// Freshly fetched `origin/main` (what Flux reconciles).
    OriginMain,
    /// The `--repo-dir` working copy, because `origin/main` could not be fetched (may be stale).
    WorkingCopy,
    /// No `--repo-dir` (or no Git repository in it yet).
    None,
}

/// PKO `Stack`s committed under `tenants/**` of the GitOps repository behind `--repo-dir`. On a
/// new or rebuilt cluster attached to an existing repository these are the Stacks Flux is about to
/// create, so they gate closing port 9090 exactly like live Stacks.
///
/// The operator's clone may predate merged `keel tenant migrate-stack-sources` PRs, so `origin` is
/// fetched first and `origin/main` is scanned; only when that fails is the (possibly stale)
/// working copy used instead.
pub(crate) async fn committed_gate_stacks(
    repo_dir: Option<&Path>,
) -> (Vec<serde_json::Value>, CommittedStacksSource) {
    let Some(repo_dir) = repo_dir else {
        return (Vec::new(), CommittedStacksSource::None);
    };
    if !repo_dir.join(".git").exists() {
        return (
            stack_manifests_in_working_copy(repo_dir),
            CommittedStacksSource::None,
        );
    }
    let git = keel_git::GitRunner::new();
    let fetched = git
        .fetch_with_timeout(repo_dir, "origin", Duration::from_secs(30))
        .await;
    if let Err(err) = &fetched {
        tracing::warn!(error = %err, "Could not fetch `origin` in --repo-dir; scanning the working copy for fluxSource Stacks");
    } else if git
        .ref_exists(repo_dir, "refs/remotes/origin/main")
        .await
        .unwrap_or(false)
    {
        match stack_manifests_at_ref(&git, repo_dir, "origin/main").await {
            Ok(stacks) => return (stacks, CommittedStacksSource::OriginMain),
            Err(err) => {
                tracing::warn!(error = %err, "Could not read `origin/main` in --repo-dir; scanning the working copy")
            }
        }
    }
    (
        stack_manifests_in_working_copy(repo_dir),
        CommittedStacksSource::WorkingCopy,
    )
}

/// Union of the live and Git-committed Stacks that block closing port 9090 (sorted, unique).
pub(crate) fn source_artifact_gate_blockers(
    live: &[serde_json::Value],
    committed: &[serde_json::Value],
) -> Vec<String> {
    let mut all = stacks_requiring_cross_namespace_artifacts(live);
    all.extend(stacks_requiring_cross_namespace_artifacts(committed));
    all.sort();
    all.dedup();
    all
}

/// What `keel bootstrap init` does with `flux-system/allow-source-controller-artifacts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ArtifactPolicyAction {
    /// No `fluxSource` Stack outside `flux-system`: port 9090 only from `flux-system`.
    Restrict,
    /// `fluxSource` Stacks remain: port 9090 from `flux-system` plus exactly these namespaces
    /// (sorted, unique). Replaces both a missing object (the flux2 chart's `allow-egress` policy
    /// would otherwise close 9090 to them) and the old `namespaceSelector: {}` rule.
    AllowNamespaces(Vec<String>),
    /// Stacks could not be listed and no object exists: recreate the pre-0.10 open rule so legacy
    /// `fluxSource` Stacks keep working, exactly as the previous release did.
    LegacyOpen,
    /// Stacks could not be listed and an object exists (or its existence is unknown): keep it.
    Unchanged,
}

/// Pure decision for the port 9090 gate (`SDD.md` §4.2). `blockers` is `None` when the live Stacks
/// could not be listed; `object_exists` is `None` when the policy itself could not be read.
pub(crate) fn artifact_policy_action(
    blockers: Option<&[String]>,
    object_exists: Option<bool>,
) -> ArtifactPolicyAction {
    match (blockers, object_exists) {
        (Some([]), _) => ArtifactPolicyAction::Restrict,
        (Some(blocking), _) => {
            let mut namespaces: Vec<String> = blocking
                .iter()
                .filter_map(|s| s.split_once('/').map(|(ns, _)| ns.to_string()))
                .filter(|ns| ns != FLUX_SYSTEM_NAMESPACE)
                .collect();
            namespaces.sort();
            namespaces.dedup();
            ArtifactPolicyAction::AllowNamespaces(namespaces)
        }
        (None, Some(false)) => ArtifactPolicyAction::LegacyOpen,
        (None, _) => ArtifactPolicyAction::Unchanged,
    }
}

/// Renders `flux-system/allow-source-controller-artifacts` for `action` (`None` for
/// [`ArtifactPolicyAction::Unchanged`]). The port 9090 peer of the restricted manifest is replaced
/// by a `kubernetes.io/metadata.name In [flux-system, ...]` selector or the legacy
/// `namespaceSelector: {}`.
pub(crate) fn render_source_artifacts_policy(
    action: &ArtifactPolicyAction,
) -> Result<Option<String>, CliError> {
    let selector = match action {
        ArtifactPolicyAction::Restrict => {
            return Ok(Some(SOURCE_CONTROLLER_ARTIFACTS_POLICY_YAML.to_string()));
        }
        ArtifactPolicyAction::Unchanged => return Ok(None),
        ArtifactPolicyAction::LegacyOpen => serde_json::json!({}),
        ArtifactPolicyAction::AllowNamespaces(namespaces) => {
            let mut values = vec![FLUX_SYSTEM_NAMESPACE.to_string()];
            values.extend(namespaces.iter().cloned());
            serde_json::json!({ "matchExpressions": [{
                "key": "kubernetes.io/metadata.name",
                "operator": "In",
                "values": values,
            }] })
        }
    };
    let mut policy: serde_json::Value =
        serde_yml::from_str(SOURCE_CONTROLLER_ARTIFACTS_POLICY_YAML)
            .map_err(|e| CliError::Validation(format!("invalid artifacts NetworkPolicy: {e}")))?;
    let rule = policy
        .pointer_mut("/spec/ingress")
        .and_then(serde_json::Value::as_array_mut)
        .and_then(|rules| {
            rules
                .iter_mut()
                .find(|r| r.pointer("/ports/0/port") == Some(&serde_json::json!(9090)))
        })
        .ok_or_else(|| {
            CliError::Validation("artifacts NetworkPolicy has no port 9090 rule".to_string())
        })?;
    rule["from"] = serde_json::json!([{ "namespaceSelector": selector }]);
    serde_yml::to_string(&policy)
        .map(Some)
        .map_err(|e| CliError::Validation(format!("cannot render artifacts NetworkPolicy: {e}")))
}

/// Runbook suffix shared by the gate notes.
fn source_artifacts_runbook(source: &CommittedStacksSource) -> String {
    let pull = if *source == CommittedStacksSource::WorkingCopy {
        " (`origin/main` could not be fetched, so the `--repo-dir` working copy was scanned: pull \
         it after merging, or the stale `stack.yaml` files keep blocking)"
    } else {
        ""
    };
    format!(
        "Run `keel tenant migrate-stack-sources --tenant <id>` for each tenant and merge the PR \
         (close or re-run PRs whose previews predate PKO `gitSource`), then re-run `keel \
         bootstrap init`{pull}."
    )
}

/// Bootstrap note when port 9090 stays reachable from `blocking` Stacks' namespaces.
pub(crate) fn source_artifacts_gate_note(
    blocking: &[String],
    namespaces: &[String],
    source: &CommittedStacksSource,
) -> String {
    format!(
        "`{FLUX_SYSTEM_NAMESPACE}/{SOURCE_CONTROLLER_ARTIFACTS_POLICY}` admits source-controller \
         artifacts (port 9090, a tarball of the whole GitOps repository) from \
         `{FLUX_SYSTEM_NAMESPACE}` and, temporarily, from {} because these PKO Stacks still use \
         `fluxSource`: {}. {}",
        namespaces
            .iter()
            .map(|ns| format!("`{ns}`"))
            .collect::<Vec<_>>()
            .join(", "),
        blocking.join(", "),
        source_artifacts_runbook(source)
    )
}

/// Bootstrap note when the gate could not be evaluated (PKO Stacks not listable).
fn source_artifacts_unverified_note(error: &str, action: &ArtifactPolicyAction) -> String {
    tracing::warn!(
        error,
        "Could not list PKO Stacks; source-controller artifact access not restricted"
    );
    let what = if *action == ArtifactPolicyAction::LegacyOpen {
        "did not exist and was created with the previous release's rule (port 9090 reachable from \
         every namespace)"
    } else {
        "was left unchanged"
    };
    format!(
        "`{FLUX_SYSTEM_NAMESPACE}/{SOURCE_CONTROLLER_ARTIFACTS_POLICY}` {what} because the PKO \
         Stacks could not be listed ({error}); re-run `keel bootstrap init` to restrict \
         source-controller artifacts (port 9090) to `{FLUX_SYSTEM_NAMESPACE}`."
    )
}

/// Whether `flux-system/allow-source-controller-artifacts` exists (`None` if unreadable).
async fn source_artifacts_policy_exists(target: KubeTarget<'_>) -> Option<bool> {
    target
        .output(&[
            "get",
            "networkpolicy",
            SOURCE_CONTROLLER_ARTIFACTS_POLICY,
            "-n",
            FLUX_SYSTEM_NAMESPACE,
            "--ignore-not-found",
            "-o",
            "name",
        ])
        .await
        .ok()
        .map(|out| !out.trim().is_empty())
}

/// Restricts `source-controller` artifact access (port 9090) to `flux-system`, replacing the
/// `namespaceSelector: {}` rule of earlier releases in place.
///
/// Gate (`SDD.md` §4.2): while PKO Stacks outside `flux-system` still use `fluxSource` (live, or
/// committed under `tenants/**` of `origin/main` behind `--repo-dir`, which Flux reconciles next),
/// port 9090 is opened to `flux-system` plus exactly their namespaces instead, so they keep working
/// on fresh clusters (where the flux2 chart's `allow-egress` policy would close it) and upgraded
/// ones stop exposing it cluster-wide. When the Stacks cannot be listed an existing object is kept
/// and a missing one is recreated with the legacy open rule. A cluster without the PKO CRD has no
/// Stacks.
async fn restrict_source_controller_artifacts(
    target: KubeTarget<'_>,
    repo_dir: Option<&Path>,
) -> Result<Option<String>, CliError> {
    let (committed, source) = committed_gate_stacks(repo_dir).await;
    let live: Result<Vec<serde_json::Value>, String> = match target
        .output(&["get", "stacks.pulumi.com", "-A", "-o", "json"])
        .await
    {
        Ok(json) => serde_json::from_str::<serde_json::Value>(&json)
            .map(|list| {
                list.get("items")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            })
            .map_err(|e| e.to_string()),
        Err(err)
            if err
                .to_string()
                .contains("the server doesn't have a resource type")
                || err.to_string().contains("no matches for kind") =>
        {
            Ok(Vec::new())
        }
        Err(err) => Err(err.to_string()),
    };
    let blocking = live
        .as_ref()
        .ok()
        .map(|live| source_artifact_gate_blockers(live, &committed));
    let object_exists = if blocking.is_none() {
        source_artifacts_policy_exists(target).await
    } else {
        None
    };
    let action = artifact_policy_action(blocking.as_deref(), object_exists);
    if let Some(yaml) = render_source_artifacts_policy(&action)? {
        target.apply(&yaml).await?;
    }
    Ok(match (&action, &live, &blocking) {
        (ArtifactPolicyAction::AllowNamespaces(namespaces), _, Some(blocking)) => {
            tracing::warn!(
                stacks = %blocking.join(", "),
                "source-controller artifacts stay reachable from these Stacks' namespaces until they use PKO gitSource"
            );
            Some(source_artifacts_gate_note(blocking, namespaces, &source))
        }
        (_, Err(error), _) => Some(source_artifacts_unverified_note(error, &action)),
        _ => None,
    })
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
        let git = docs(&super::super::render_seed_forgejo_helmrelease(&ingress, true).unwrap());
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
    fn source_artifact_gate_blocks_on_fluxsource_stacks_outside_flux_system() {
        let list = serde_json::json!([
            { "metadata": { "namespace": "flux-system", "name": "management-cluster-core" },
              "spec": { "fluxSource": { "sourceRef": { "name": "flux-system" } } } },
            { "metadata": { "namespace": "tenant-default", "name": "lab" },
              "spec": { "fluxSource": { "sourceRef": { "name": "keel-gitops" } } } },
            { "metadata": { "namespace": "tenant-acme", "name": "vm" },
              "spec": { "projectRepo": "https://forge.example/keel.git", "fluxSource": null } },
            { "metadata": { "namespace": "keel-previews", "name": "preview-stack-pr-7" },
              "spec": { "fluxSource": { "sourceRef": { "name": "preview-source-pr-7" } } } },
            { "metadata": { "namespace": "keel-system", "name": "no-source" }, "spec": {} }
        ]);
        let list = list.as_array().unwrap();
        assert_eq!(
            stacks_requiring_cross_namespace_artifacts(list),
            vec![
                "keel-previews/preview-stack-pr-7".to_string(),
                "tenant-default/lab".to_string()
            ]
        );
        // Only the same-namespace management Stack (or no Stacks at all) never blocks.
        let mgmt_only = [list[0].clone(), list[2].clone()];
        assert!(stacks_requiring_cross_namespace_artifacts(&mgmt_only).is_empty());
        assert!(stacks_requiring_cross_namespace_artifacts(&[]).is_empty());

        // Live and Git-committed blockers are merged (sorted, unique).
        let committed = [list[1].clone()];
        assert_eq!(
            source_artifact_gate_blockers(&mgmt_only, &committed),
            vec!["tenant-default/lab".to_string()]
        );
        assert_eq!(
            source_artifact_gate_blockers(list, &committed),
            vec![
                "keel-previews/preview-stack-pr-7".to_string(),
                "tenant-default/lab".to_string()
            ]
        );

        let note = source_artifacts_gate_note(
            &["tenant-default/lab".to_string()],
            &["tenant-default".to_string()],
            &CommittedStacksSource::OriginMain,
        );
        assert!(note.contains("tenant-default/lab"), "{note}");
        assert!(note.contains("`tenant-default`"), "{note}");
        assert!(note.contains("keel tenant migrate-stack-sources"), "{note}");
        assert!(note.contains(SOURCE_CONTROLLER_ARTIFACTS_POLICY), "{note}");
        assert!(!note.contains("every namespace"), "{note}");
        assert!(!note.contains("pull it"), "{note}");
        let stale = source_artifacts_gate_note(
            &["tenant-default/lab".to_string()],
            &["tenant-default".to_string()],
            &CommittedStacksSource::WorkingCopy,
        );
        assert!(stale.contains("pull it"), "{stale}");
        let unverified =
            source_artifacts_unverified_note("forbidden", &ArtifactPolicyAction::Unchanged);
        assert!(unverified.contains("forbidden") && unverified.contains("keel bootstrap init"));
        assert!(unverified.contains("left unchanged"), "{unverified}");
        let recreated =
            source_artifacts_unverified_note("forbidden", &ArtifactPolicyAction::LegacyOpen);
        assert!(recreated.contains("every namespace"), "{recreated}");
    }

    #[test]
    fn artifact_policy_never_closes_port_9090_on_fluxsource_stacks() {
        let blockers = vec![
            "keel-previews/preview-stack-pr-7".to_string(),
            "tenant-acme/a".to_string(),
            "tenant-acme/b".to_string(),
        ];
        let narrowed = ArtifactPolicyAction::AllowNamespaces(vec![
            "keel-previews".to_string(),
            "tenant-acme".to_string(),
        ]);
        // Blockers known: narrowed rule whether the object is absent (fresh/rebuilt cluster, where
        // the flux2 chart's `allow-egress` would otherwise close 9090) or holds the old open rule.
        for exists in [Some(true), Some(false), None] {
            assert_eq!(artifact_policy_action(Some(&blockers), exists), narrowed);
            assert_eq!(
                artifact_policy_action(Some(&[]), exists),
                ArtifactPolicyAction::Restrict
            );
        }
        // Stacks not listable: keep an existing object, recreate the legacy open rule if absent.
        assert_eq!(
            artifact_policy_action(None, Some(false)),
            ArtifactPolicyAction::LegacyOpen
        );
        assert_eq!(
            artifact_policy_action(None, Some(true)),
            ArtifactPolicyAction::Unchanged
        );
        assert_eq!(
            artifact_policy_action(None, None),
            ArtifactPolicyAction::Unchanged
        );

        let rule_9090 = |action: &ArtifactPolicyAction| -> Value {
            let yaml = render_source_artifacts_policy(action).unwrap().unwrap();
            let policy: Value = serde_yml::from_str(&yaml).unwrap();
            assert_eq!(
                policy["metadata"]["name"],
                SOURCE_CONTROLLER_ARTIFACTS_POLICY
            );
            assert_eq!(policy["metadata"]["namespace"], "flux-system");
            let rules = policy["spec"]["ingress"].as_array().unwrap();
            // Metrics (8080) rule is kept as is.
            assert!(rules.iter().any(|r| r["ports"][0]["port"] == 8080));
            rules
                .iter()
                .find(|r| r["ports"][0]["port"] == 9090)
                .unwrap()["from"]
                .clone()
        };
        assert_eq!(
            rule_9090(&ArtifactPolicyAction::Restrict),
            serde_json::json!([{ "namespaceSelector": { "matchLabels": {
                "kubernetes.io/metadata.name": "flux-system" } } }])
        );
        assert_eq!(
            rule_9090(&narrowed),
            serde_json::json!([{ "namespaceSelector": { "matchExpressions": [{
                "key": "kubernetes.io/metadata.name", "operator": "In",
                "values": ["flux-system", "keel-previews", "tenant-acme"] }] } }])
        );
        assert_eq!(
            rule_9090(&ArtifactPolicyAction::LegacyOpen),
            serde_json::json!([{ "namespaceSelector": {} }])
        );
        assert!(
            render_source_artifacts_policy(&ArtifactPolicyAction::Unchanged)
                .unwrap()
                .is_none()
        );
    }

    fn git_in(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[tokio::test]
    async fn committed_gate_reads_fetched_origin_main_not_a_stale_working_copy() {
        let root = std::env::temp_dir().join(format!(
            "keel-gate-remote-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bare = root.join("remote.git");
        let operator = root.join("operator");
        let server = root.join("server");
        std::fs::create_dir_all(&root).unwrap();
        git_in(
            &root,
            &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        git_in(
            &root,
            &["clone", bare.to_str().unwrap(), operator.to_str().unwrap()],
        );
        let legacy = "apiVersion: pulumi.com/v1\nkind: Stack\nmetadata:\n  name: lab\nspec:\n  fluxSource:\n    sourceRef:\n      name: keel-gitops\n";
        std::fs::create_dir_all(operator.join("tenants/acme/lab")).unwrap();
        std::fs::write(operator.join("tenants/acme/lab/stack.yaml"), legacy).unwrap();
        git_in(&operator, &["add", "-A"]);
        git_in(&operator, &["commit", "-qm", "legacy"]);
        git_in(&operator, &["push", "-q", "origin", "HEAD:refs/heads/main"]);

        // The migration PR is merged on the remote, the operator's clone is not pulled.
        git_in(
            &root,
            &["clone", bare.to_str().unwrap(), server.to_str().unwrap()],
        );
        std::fs::write(
            server.join("tenants/acme/lab/stack.yaml"),
            "apiVersion: pulumi.com/v1\nkind: Stack\nmetadata:\n  name: lab\nspec:\n  projectRepo: https://forge.example/keel.git\n",
        )
        .unwrap();
        std::fs::write(server.join("tenants/acme/notes.txt"), "kind: Stack").unwrap();
        git_in(&server, &["add", "-A"]);
        git_in(&server, &["commit", "-qm", "migrate"]);
        git_in(&server, &["push", "-q", "origin", "HEAD:refs/heads/main"]);

        assert_eq!(
            source_artifact_gate_blockers(&[], &stack_manifests_in_working_copy(&operator)),
            vec!["tenant-acme/lab".to_string()]
        );
        let (committed, source) = committed_gate_stacks(Some(&operator)).await;
        assert_eq!(source, CommittedStacksSource::OriginMain);
        assert_eq!(committed.len(), 1);
        assert_eq!(committed[0]["metadata"]["namespace"], "tenant-acme");
        assert!(source_artifact_gate_blockers(&[], &committed).is_empty());

        // Unreachable remote: falls back to the (stale) working copy and says so.
        git_in(
            &operator,
            &[
                "remote",
                "set-url",
                "origin",
                root.join("gone.git").to_str().unwrap(),
            ],
        );
        let (committed, source) = committed_gate_stacks(Some(&operator)).await;
        assert_eq!(source, CommittedStacksSource::WorkingCopy);
        assert_eq!(
            source_artifact_gate_blockers(&[], &committed),
            vec!["tenant-acme/lab".to_string()]
        );
        assert_eq!(
            committed_gate_stacks(None).await.1,
            CommittedStacksSource::None
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn committed_tenant_stacks_gate_the_artifact_restriction() {
        let root = std::env::temp_dir().join(format!(
            "keel-gate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let lab = root.join("tenants/acme/infra/lab");
        std::fs::create_dir_all(&lab).unwrap();
        // Namespace inferred from `tenants/<id>/`; multi-doc files and non-Stack docs handled.
        std::fs::write(
            lab.join("stack.yaml"),
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: x\n---\napiVersion: pulumi.com/v1\nkind: Stack\nmetadata:\n  name: lab\nspec:\n  fluxSource:\n    sourceRef:\n      name: keel-gitops\n",
        )
        .unwrap();
        std::fs::write(
            root.join("tenants/acme/migrated.yaml"),
            "apiVersion: pulumi.com/v1\nkind: Stack\nmetadata:\n  name: vm\n  namespace: tenant-acme\nspec:\n  projectRepo: https://forge.example/keel.git\n",
        )
        .unwrap();
        std::fs::write(root.join("tenants/acme/notes.txt"), "kind: Stack").unwrap();
        std::fs::write(root.join("tenants/broken.yaml"), ": : :").unwrap();

        let committed = stack_manifests_in_working_copy(&root);
        assert_eq!(committed.len(), 2);
        assert_eq!(
            source_artifact_gate_blockers(&[], &committed),
            vec!["tenant-acme/lab".to_string()]
        );
        // A repository without `tenants/` has no committed Stacks.
        assert!(stack_manifests_in_working_copy(&root.join("missing")).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
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
