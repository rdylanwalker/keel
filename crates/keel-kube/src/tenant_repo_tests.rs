use super::*;

const FORGEJO_URL: &str =
    "https://forgejo.keel.local:8888/keel-tenants/infra-gitops-tenant-acme.git";
const GITHUB_URL: &str = "ssh://git@github.com/acme-org/infra-gitops-tenant-acme.git";
const GITHUB_443_URL: &str = "ssh://git@ssh.github.com:443/acme-org/infra-gitops-tenant-acme.git";
const HOST_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
/// Synthetic OpenSSH key header (`openssh-key-v1`, cipher `none`, KDF `none`): unencrypted.
const PRIVATE_KEY_BODY: &str = "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAAB";
const PRIVATE_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAAB\n-----END OPENSSH PRIVATE KEY-----";
/// Same header with cipher `aes256-ctr` / KDF `bcrypt` (passphrase-protected, as `ssh-keygen -N`
/// writes it): the PEM label does not say ENCRYPTED.
const ENCRYPTED_PRIVATE_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAFAAAAAAAAAAAAAAAAAAAAAAAAAAQAAAAAQ==\n-----END OPENSSH PRIVATE KEY-----";
const PUBLIC_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl keel-tenant-acme";
const CA: &str = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
const TOKEN: &str = "s3cr3t-read-token-value";

fn https() -> TenantRepoSource {
    TenantRepoSource::new("acme", FORGEJO_URL).unwrap()
}

fn ssh() -> TenantRepoSource {
    TenantRepoSource::new("acme", GITHUB_URL).unwrap()
}

fn https_credential() -> TenantRepoCredential {
    TenantRepoCredential::HttpsToken {
        username: "keel-tenant-reader".to_string(),
        token: TOKEN.to_string(),
        ca_crt: Some(CA.to_string()),
    }
}

fn ssh_credential(known_hosts: &str) -> TenantRepoCredential {
    TenantRepoCredential::SshDeployKey {
        private_key: PRIVATE_KEY.to_string(),
        public_key: PUBLIC_KEY.to_string(),
        known_hosts: known_hosts.to_string(),
    }
}

#[test]
fn source_accepts_only_https_and_ssh_urls_with_a_path() {
    assert_eq!(https().transport(), TenantRepoTransport::Https);
    assert_eq!(ssh().transport(), TenantRepoTransport::Ssh);
    assert_eq!(https().namespace(), "tenant-acme");
    assert_eq!(https().branch(), "main");
    for bad in [
        // Token in clear text; PKO cannot clone it either.
        "http://forgejo-http.keel-system.svc.cluster.local:3000/keel-tenants/r.git",
        // Flux only accepts ssh:// URLs.
        "git@github.com:acme-org/r.git",
        "https://user:pw@forgejo.example/o/r.git",
        "https://forgejo.example",
        "https://forgejo.example/",
        "ssh://github.com/acme-org/r.git",
        // A password in the SSH userinfo would be committed in the stub and echoed in errors.
        "ssh://git:secret@github.com/acme-org/r.git",
        "ssh://@github.com/acme-org/r.git",
        // In-cluster Forgejo SSH (pod port 2222) is not reachable from tenant PKO workspaces.
        "ssh://git@forgejo-ssh.keel-system.svc.cluster.local/keel-tenants/r.git",
        "ssh://git@forgejo-ssh.keel-system.svc:2222/keel-tenants/r.git",
        "ssh://git@forgejo-ssh/keel-tenants/r.git",
        "ext::sh -c touch% /tmp/x",
        "file:///srv/git/r.git",
        "/srv/git/r.git",
        " https://forgejo.example/o/r.git",
    ] {
        assert!(TenantRepoSource::new("acme", bad).is_err(), "{bad}");
    }
    assert!(TenantRepoSource::new("Bad_Tenant", FORGEJO_URL).is_err());
    let err = TenantRepoSource::new("acme", "ssh://git:hunter2@github.com/o/r.git").unwrap_err();
    assert!(!err.to_string().contains("hunter2"), "{err}");
    // Other plain SSH users (self-hosted forges) are fine.
    TenantRepoSource::new("acme", "ssh://deploy_user-1@forge.example/o/r.git").unwrap();
}

#[test]
fn ssh_known_hosts_host_follows_openssh_port_rules() {
    assert_eq!(ssh().ssh_known_hosts_host().as_deref(), Some("github.com"));
    let fallback = TenantRepoSource::new("acme", GITHUB_443_URL).unwrap();
    assert_eq!(
        fallback.ssh_known_hosts_host().as_deref(),
        Some("[ssh.github.com]:443")
    );
    let explicit_22 = TenantRepoSource::new("acme", "ssh://git@Forge.Example:22/o/r.git").unwrap();
    assert_eq!(
        explicit_22.ssh_known_hosts_host().as_deref(),
        Some("forge.example")
    );
    assert_eq!(https().ssh_known_hosts_host(), None);
}

#[test]
fn git_repository_stub_points_at_the_tenant_repository() {
    let repo = build_tenant_repo_git_repository(&https()).unwrap();
    assert_eq!(repo["apiVersion"], "source.toolkit.fluxcd.io/v1");
    assert_eq!(repo["kind"], "GitRepository");
    assert_eq!(repo["metadata"]["name"], "keel-gitops");
    assert_eq!(repo["metadata"]["namespace"], "tenant-acme");
    assert_eq!(repo["metadata"]["labels"]["keel.io/platform-owned"], "true");
    assert_eq!(repo["metadata"]["labels"]["keel.io/tenant"], "acme");
    assert_eq!(
        repo["metadata"]["annotations"]["keel.io/git-transport"],
        "https"
    );
    assert_eq!(repo["spec"]["interval"], "1m");
    assert_eq!(repo["spec"]["url"], FORGEJO_URL);
    assert_eq!(repo["spec"]["ref"], json!({"branch": "main"}));
    assert_eq!(
        repo["spec"]["secretRef"],
        json!({"name": "keel-gitops-credentials"})
    );
    let ssh_repo = build_tenant_repo_git_repository(&ssh()).unwrap();
    assert_eq!(ssh_repo["spec"]["url"], GITHUB_URL);
    assert_eq!(
        ssh_repo["metadata"]["annotations"]["keel.io/git-transport"],
        "ssh"
    );
}

#[test]
fn root_kustomization_runs_as_the_tenant_and_decrypts_with_its_key() {
    let ks = build_tenant_root_kustomization("acme").unwrap();
    assert_eq!(ks["apiVersion"], "kustomize.toolkit.fluxcd.io/v1");
    assert_eq!(ks["kind"], "Kustomization");
    assert_eq!(ks["metadata"]["name"], "keel-tenant-root");
    assert_eq!(ks["metadata"]["namespace"], "tenant-acme");
    assert_eq!(ks["metadata"]["labels"]["keel.io/platform-owned"], "true");
    let spec = &ks["spec"];
    assert_eq!(spec["path"], "./tenants/acme");
    assert_eq!(spec["serviceAccountName"], "tenant-acme-sa");
    assert_eq!(spec["targetNamespace"], "tenant-acme");
    assert_eq!(spec["prune"], true);
    assert_eq!(spec["wait"], false);
    assert_eq!(spec["interval"], "5m");
    assert_eq!(
        spec["sourceRef"],
        json!({"kind": "GitRepository", "name": "keel-gitops", "namespace": "tenant-acme"})
    );
    assert_eq!(
        spec["decryption"],
        json!({"provider": "sops", "secretRef": {"name": "sops-age"}})
    );
    assert!(spec.get("kubeConfig").is_none());
    assert!(build_tenant_root_kustomization("../x").is_err());
}

#[test]
fn stub_contains_the_baseline_source_and_sync_and_no_secret() {
    let stub = synthesize_tenant_repo_stub(&TenantIsolationConfig::new("acme"), &https()).unwrap();
    let paths: Vec<&str> = stub.files.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        paths,
        [
            "tenants/acme/rbac.yaml",
            "tenants/acme/git-repository.yaml",
            "tenants/acme/sync.yaml"
        ]
    );
    assert_eq!(
        stub.kustomization_entries(),
        [
            "acme/rbac.yaml",
            "acme/git-repository.yaml",
            "acme/sync.yaml"
        ]
    );
    let mut kinds = Vec::new();
    for (path, yaml) in &stub.files {
        ensure_git_safe_manifests(path, yaml).unwrap();
        // Every stub object is platform-owned (Kyverno-protected) and in the tenant namespace.
        assert!(!crate::platform_owned_labels_missing(yaml), "{path}");
        for doc in keel_core::split_yaml_documents(yaml) {
            let v: Value = serde_yml::from_str(&doc).unwrap();
            let kind = v["kind"].as_str().unwrap().to_string();
            assert!(
                crate::PLATFORM_OWNED_KINDS.contains(&kind.as_str()),
                "{kind}"
            );
            if kind != "Namespace" && kind != "ClusterRoleBinding" {
                assert_eq!(v["metadata"]["namespace"], "tenant-acme", "{path} {kind}");
            }
            kinds.push(kind);
        }
        // Platform-owned stub files are the ones the baseline migration relabels.
        let file = path.rsplit('/').next().unwrap();
        assert!(crate::PLATFORM_OWNED_TENANT_FILES.contains(&file), "{file}");
    }
    // Quota, LimitRange and NetworkPolicies stay in the platform stub (the tenant SA cannot
    // write them; one in the tenant repository would fail the tenant's whole apply).
    for kind in [
        "ResourceQuota",
        "LimitRange",
        "NetworkPolicy",
        "GitRepository",
        "Kustomization",
    ] {
        assert!(kinds.iter().any(|k| k == kind), "{kind}");
    }
    assert!(!kinds.iter().any(|k| k == "Secret"));
    let all: String = stub.files.iter().map(|(_, y)| y.as_str()).collect();
    assert!(!all.contains(TOKEN) && !all.contains("stringData"));

    let other = synthesize_tenant_repo_stub(&TenantIsolationConfig::new("other"), &https());
    assert!(other.is_err());
}

/// The credentials Secret is applied at runtime and must never end up in a Git-written bundle.
#[test]
fn credentials_secret_is_never_written_to_git() {
    for (source, credential) in [
        (https(), https_credential()),
        (ssh(), ssh_credential(&format!("github.com {HOST_KEY}\n"))),
    ] {
        let secret = build_tenant_gitops_credentials_secret(&source, &credential).unwrap();
        let yaml = serde_yml::to_string(&secret).unwrap();
        let err = ensure_git_safe_manifests("tenants/acme/git-repository.yaml", &yaml)
            .unwrap_err()
            .to_string();
        assert!(err.contains("tenant Git credentials Secret"), "{err}");
        // Also when appended to an otherwise valid stub file, or renamed but still labelled.
        let stub =
            synthesize_tenant_repo_stub(&TenantIsolationConfig::new("acme"), &source).unwrap();
        for (path, content) in &stub.files {
            let combined = format!("{content}---\n{yaml}");
            assert!(
                ensure_git_safe_manifests(path, &combined).is_err(),
                "{path}"
            );
        }
        let renamed = yaml.replace("name: keel-gitops-credentials", "name: renamed");
        assert!(ensure_git_safe_manifests("x.yaml", &renamed).is_err());
    }
    // The legacy read-only `git-credentials` Secret too, and any plaintext Secret.
    let legacy = crate::tenant::build_tenant_git_credentials_secret("acme", "u", "t").unwrap();
    assert!(ensure_git_safe_manifests("x.yaml", &serde_yml::to_string(&legacy).unwrap()).is_err());
    let plain = "apiVersion: v1\nkind: Secret\nmetadata:\n  name: app\nstringData:\n  k: v\n";
    assert!(ensure_git_safe_manifests("x.yaml", plain).is_err());
    // SOPS-encrypted app Secrets (`secrets.enc.yaml`) and non-Secret objects are fine.
    let encrypted = "apiVersion: v1\nkind: Secret\nmetadata:\n  name: app\nstringData:\n  k: ENC[AES256_GCM,data:x]\nsops:\n  version: 3.9.0\n";
    ensure_git_safe_manifests("x.yaml", encrypted).unwrap();
    ensure_git_safe_manifests(
        "x.yaml",
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: c\n",
    )
    .unwrap();
    assert!(ensure_git_safe_manifests("x.yaml", "kind: [unterminated").is_err());
}

#[test]
fn https_credentials_secret_has_the_flux_and_pko_keys() {
    let secret = build_tenant_gitops_credentials_secret(&https(), &https_credential()).unwrap();
    assert_eq!(secret["metadata"]["name"], "keel-gitops-credentials");
    assert_eq!(secret["metadata"]["namespace"], "tenant-acme");
    assert_eq!(
        secret["metadata"]["labels"]["keel.io/secret-kind"],
        "tenant-gitops-credentials"
    );
    // Cluster-only, but platform-owned: Kyverno keeps tenant admins from replacing it.
    assert_eq!(secret["metadata"]["labels"][PLATFORM_OWNED_LABEL], "true");
    assert_eq!(secret["metadata"]["labels"]["keel.io/tenant"], "acme");
    assert_eq!(
        secret["metadata"]["annotations"]["keel.io/git-transport"],
        "https"
    );
    assert_eq!(
        secret["stringData"],
        json!({"username": "keel-tenant-reader", "password": TOKEN, "ca.crt": CA})
    );
    let no_ca = TenantRepoCredential::HttpsToken {
        username: "gitlab+deploy-token-7".to_string(),
        token: TOKEN.to_string(),
        ca_crt: None,
    };
    let s = build_tenant_gitops_credentials_secret(&https(), &no_ca).unwrap();
    assert!(s["stringData"].get("ca.crt").is_none());

    for (username, token, ca) in [
        ("", TOKEN, None),
        ("a b", TOKEN, None),
        ("a:b", TOKEN, None),
        ("u", "", None),
        ("u", "tok en", None),
        (
            "u",
            TOKEN,
            Some("-----BEGIN PRIVATE KEY-----\nx\n-----END PRIVATE KEY-----"),
        ),
    ] {
        let c = TenantRepoCredential::HttpsToken {
            username: username.to_string(),
            token: token.to_string(),
            ca_crt: ca.map(str::to_string),
        };
        assert!(
            build_tenant_gitops_credentials_secret(&https(), &c).is_err(),
            "{username}"
        );
    }
    // Transport must match the URL.
    let err = build_tenant_gitops_credentials_secret(&ssh(), &https_credential()).unwrap_err();
    assert!(err.to_string().contains("uses ssh"));
}

#[test]
fn ssh_credentials_secret_has_the_flux_and_pko_keys_and_pinned_host_keys() {
    let secret = build_tenant_gitops_credentials_secret(
        &ssh(),
        &ssh_credential(&format!("# comment\ngithub.com {HOST_KEY} trailing\n")),
    )
    .unwrap();
    let data = secret["stringData"].as_object().unwrap();
    let keys: Vec<&str> = data.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        ["identity", "identity.pub", "known_hosts", "sshPrivateKey"]
    );
    assert_eq!(data["identity"], data["sshPrivateKey"]);
    assert!(
        data["identity"]
            .as_str()
            .unwrap()
            .ends_with("PRIVATE KEY-----\n")
    );
    assert_eq!(data["known_hosts"], format!("github.com {HOST_KEY}\n"));
    assert_eq!(data["identity.pub"], format!("{PUBLIC_KEY}\n"));
    assert_eq!(
        secret["metadata"]["annotations"]["keel.io/git-transport"],
        "ssh"
    );

    // Port 443 fallback needs the `[host]:port` entry.
    let fallback = TenantRepoSource::new("acme", GITHUB_443_URL).unwrap();
    assert!(
        build_tenant_gitops_credentials_secret(
            &fallback,
            &ssh_credential(&format!("ssh.github.com {HOST_KEY}\n"))
        )
        .is_err()
    );
    build_tenant_gitops_credentials_secret(
        &fallback,
        &ssh_credential(&format!("[ssh.github.com]:443 {HOST_KEY}\n")),
    )
    .unwrap();

    for known_hosts in [
        String::new(),
        "# only a comment\n".to_string(),
        format!("gitlab.com {HOST_KEY}\n"),
        format!("* {HOST_KEY}\n"),
        format!("*.github.com {HOST_KEY}\n"),
        format!("@cert-authority github.com {HOST_KEY}\n"),
        "github.com ssh-ed25519\n".to_string(),
        "github.com not-a-type AAAA\n".to_string(),
        // Extra entries for other hosts are refused, not kept (they would be trusted wherever
        // the file is used), as are multi-host and hashed entries.
        format!("github.com {HOST_KEY}\ngitlab.com {HOST_KEY}\n"),
        format!("github.com {HOST_KEY}\n[ssh.github.com]:443 {HOST_KEY}\n"),
        format!("github.com,140.82.112.3 {HOST_KEY}\n"),
        format!("|1|abc=|def= {HOST_KEY}\n"),
    ] {
        assert!(
            build_tenant_gitops_credentials_secret(&ssh(), &ssh_credential(&known_hosts)).is_err(),
            "{known_hosts:?}"
        );
    }
    for (private_key, public_key) in [
        ("not a key", PUBLIC_KEY),
        (
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\nx\n-----END ENCRYPTED PRIVATE KEY-----",
            PUBLIC_KEY,
        ),
        // Passphrase-protected OpenSSH key: the cipher is only inside the base64 body.
        (ENCRYPTED_PRIVATE_KEY, PUBLIC_KEY),
        // Legacy PEM with an encryption header.
        (
            "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,00\n\nx\n-----END RSA PRIVATE KEY-----",
            PUBLIC_KEY,
        ),
        // OpenSSH block that is not an openssh-key-v1 body.
        (
            "-----BEGIN OPENSSH PRIVATE KEY-----\nbm90LWEta2V5\n-----END OPENSSH PRIVATE KEY-----",
            PUBLIC_KEY,
        ),
        (
            "-----BEGIN OPENSSH PRIVATE KEY-----\n-----END OPENSSH PRIVATE KEY-----",
            PUBLIC_KEY,
        ),
        (PRIVATE_KEY, "not-a-key"),
        (PRIVATE_KEY, "ssh-ed25519 AAAA\nssh-ed25519 BBBB"),
    ] {
        let c = TenantRepoCredential::SshDeployKey {
            private_key: private_key.to_string(),
            public_key: public_key.to_string(),
            known_hosts: format!("github.com {HOST_KEY}\n"),
        };
        assert!(build_tenant_gitops_credentials_secret(&ssh(), &c).is_err());
    }
    let err = build_tenant_gitops_credentials_secret(
        &https(),
        &ssh_credential(&format!("github.com {HOST_KEY}\n")),
    )
    .unwrap_err();
    assert!(err.to_string().contains("uses https"));
}

#[test]
fn credential_debug_never_prints_secret_material() {
    let https = format!("{:?}", https_credential());
    assert!(!https.contains(TOKEN) && https.contains("REDACTED"));
    let ssh = format!("{:?}", ssh_credential("github.com x y"));
    assert!(!ssh.contains(PRIVATE_KEY_BODY) && ssh.contains("REDACTED"));
    // Errors never echo secret values either.
    let bad = TenantRepoCredential::HttpsToken {
        username: "u".to_string(),
        token: "has space-secret".to_string(),
        ca_crt: None,
    };
    let err = build_tenant_gitops_credentials_secret(&self::https(), &bad)
        .unwrap_err()
        .to_string();
    assert!(!err.contains("space-secret"), "{err}");
}

#[test]
fn stack_git_source_tracks_main_with_the_tenant_credentials() {
    let git = tenant_repo_stack_git_source(&https(), "tenants/acme/infra/vm").unwrap();
    let doc = serde_json::to_value(&git).unwrap();
    assert_eq!(
        doc,
        json!({
            "projectRepo": FORGEJO_URL,
            "branch": "main",
            "repoDir": "tenants/acme/infra/vm",
            "shallow": true,
            "gitAuth": {"basicAuth": {
                "userName": {"type": "Secret", "secret": {"name": "keel-gitops-credentials", "key": "username"}},
                "password": {"type": "Secret", "secret": {"name": "keel-gitops-credentials", "key": "password"}}
            }}
        })
    );
    let ssh_git = tenant_repo_stack_git_source(&ssh(), "tenants/acme").unwrap();
    assert_eq!(
        serde_json::to_value(&ssh_git).unwrap()["gitAuth"],
        json!({"sshAuth": {"sshPrivateKey": {"type": "Secret", "secret": {"name": "keel-gitops-credentials", "key": "sshPrivateKey"}}}})
    );
    for bad in [
        "tenants/other/infra",
        "tenants/acme-evil/x",
        "tenants/acme/../other",
        "clusters/management",
        "/tenants/acme",
    ] {
        assert!(
            tenant_repo_stack_git_source(&https(), bad).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn preview_git_source_is_commit_pinned_with_the_preview_copy() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let git = tenant_repo_preview_git_source(
        &ssh(),
        sha,
        "tenants/acme/infra/vm",
        "keel-preview-42-keel-gitops-credentials",
    )
    .unwrap();
    assert_eq!(git.commit.as_deref(), Some(sha));
    assert_eq!(git.branch, None);
    // Full history: the pinned commit must still resolve after a later push moves the branch.
    assert_eq!(git.shallow, Some(false));
    // Abbreviated SHAs never resolve in PKO/go-git (zero-padded full hash).
    for short in [
        "0123456",
        "0123456789abcdef0123456789abcdef0123456",
        &format!("{sha}0"),
    ] {
        assert!(
            tenant_repo_preview_git_source(&ssh(), short, "tenants/acme", "copy").is_err(),
            "{short}"
        );
    }
    assert!(is_full_commit_sha(&"a".repeat(64)));
    assert!(!is_full_commit_sha(&"g".repeat(40)));
    assert_eq!(
        git.git_auth.unwrap().secret_names(),
        BTreeSet::from(["keel-preview-42-keel-gitops-credentials".to_string()])
    );
    assert!(
        tenant_repo_preview_git_source(&ssh(), "main", "tenants/acme", "copy").is_err(),
        "previews never track a branch"
    );
    assert!(tenant_repo_preview_git_source(&ssh(), sha, "tenants/acme", "Bad_Name").is_err());
    assert_eq!(
        tenant_repo_preview_secret_keys(TenantRepoTransport::Https),
        BTreeSet::from(["password".to_string(), "username".to_string()])
    );
    assert_eq!(
        tenant_repo_preview_secret_keys(TenantRepoTransport::Ssh),
        BTreeSet::from(["known_hosts".to_string(), "sshPrivateKey".to_string()])
    );
}

#[test]
fn workspace_template_adds_known_hosts_only_for_ssh() {
    let https_tpl = tenant_repo_workspace_template(
        PkoWorkspaceTemplate::restricted("keel-pulumi-passphrase"),
        TenantRepoTransport::Https,
        TENANT_GITOPS_CREDENTIALS_SECRET_NAME,
    );
    let text = serde_json::to_string(&https_tpl).unwrap();
    assert!(text.contains("keel-ca-bundle") && !text.contains("SSH_KNOWN_HOSTS"));

    let ssh_tpl = tenant_repo_workspace_template(
        PkoWorkspaceTemplate::restricted("keel-pulumi-passphrase"),
        TenantRepoTransport::Ssh,
        TENANT_GITOPS_CREDENTIALS_SECRET_NAME,
    );
    let pod = serde_json::to_value(&ssh_tpl.spec.pod_template.spec).unwrap();
    assert!(pod["volumes"].as_array().unwrap().contains(&json!({
        "name": "keel-ssh-known-hosts",
        "secret": {
            "secretName": "keel-gitops-credentials",
            "optional": false,
            "items": [{"key": "known_hosts", "path": "known_hosts"}]
        }
    })));
    assert!(
        pod["initContainers"][0]["env"]
            .as_array()
            .unwrap()
            .contains(&json!({
                "name": "SSH_KNOWN_HOSTS", "value": "/etc/keel-ssh/known_hosts"
            }))
    );
}

#[test]
fn pko_operator_known_hosts_values_are_merged_idempotently() {
    let seed: Value =
        keel_core::split_yaml_documents(include_str!("../../../deploy/seed/core-addons/pko.yaml"))
            .iter()
            .map(|d| serde_yml::from_str::<Value>(d).unwrap())
            .find(|d| d["kind"] == "HelmRelease")
            .unwrap();
    let mut values = seed["spec"]["values"].clone();
    merge_pko_operator_ssh_known_hosts_values(&mut values).unwrap();
    merge_pko_operator_ssh_known_hosts_values(&mut values).unwrap();
    let env = values["extraEnv"].as_array().unwrap();
    // The Keel CA trust stays; SSH_KNOWN_HOSTS is added once.
    assert_eq!(env.len(), 2);
    assert!(env.contains(&json!({"name": "SSL_CERT_DIR", "value": "/etc/ssl/certs:/etc/keel-ca"})));
    assert!(
        env.contains(&json!({"name": "SSH_KNOWN_HOSTS", "value": "/etc/keel-ssh/known_hosts"}))
    );
    let volumes = values["extraVolumes"].as_array().unwrap();
    assert_eq!(volumes.len(), 2);
    assert!(volumes.contains(&json!({
        "name": "keel-ssh-known-hosts",
        "configMap": {
            "name": "keel-pko-ssh-known-hosts",
            "optional": true,
            "items": [{"key": "known_hosts", "path": "known_hosts"}]
        }
    })));
    assert_eq!(values["extraVolumeMounts"].as_array().unwrap().len(), 2);
    assert_eq!(values["rbac"]["createClusterAggregationRoles"], true);

    let mut empty = Value::Null;
    merge_pko_operator_ssh_known_hosts_values(&mut empty).unwrap();
    assert_eq!(empty["extraEnv"].as_array().unwrap().len(), 1);
    let mut bad = json!({"extraEnv": {"not": "a list"}});
    assert!(merge_pko_operator_ssh_known_hosts_values(&mut bad).is_err());

    assert!(!pko_operator_needs_ssh_known_hosts(&[https()]));
    assert!(pko_operator_needs_ssh_known_hosts(&[https(), ssh()]));
}

#[test]
fn pko_operator_known_hosts_configmap_merges_public_host_keys() {
    let fallback = TenantRepoSource::new("acme", GITHUB_443_URL).unwrap();
    let other = TenantRepoSource::new("other", "ssh://git@github.com/o/other.git").unwrap();
    let github = format!("github.com {HOST_KEY}\n");
    let github_443 = format!("[ssh.github.com]:443 {HOST_KEY}\n");
    let cm = build_pko_operator_known_hosts_configmap(&[
        (&ssh(), github.as_str()),
        (&fallback, github_443.as_str()),
        (&other, github.as_str()),
    ])
    .unwrap();
    assert_eq!(cm["kind"], "ConfigMap");
    assert_eq!(cm["metadata"]["namespace"], "keel-system");
    assert_eq!(cm["metadata"]["name"], "keel-pko-ssh-known-hosts");
    assert_eq!(
        cm["data"]["known_hosts"],
        format!("[ssh.github.com]:443 {HOST_KEY}\ngithub.com {HOST_KEY}\n")
    );
    assert!(build_pko_operator_known_hosts_configmap(&[]).is_err());
    assert!(build_pko_operator_known_hosts_configmap(&[(&ssh(), PRIVATE_KEY)]).is_err());
    // One source can never add a host key for another host (e.g. a planted `github.com` key
    // next to its own `[ssh.github.com]:443` entry), which the operator would trust for every
    // tenant.
    let planted = format!("[ssh.github.com]:443 {HOST_KEY}\ngithub.com ssh-ed25519 AAAAattacker\n");
    let err = build_pko_operator_known_hosts_configmap(&[
        (&ssh(), github.as_str()),
        (&fallback, planted.as_str()),
    ])
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("is not for `[ssh.github.com]:443`"),
        "{err}"
    );
    // HTTPS sources need no operator host key.
    assert!(build_pko_operator_known_hosts_configmap(&[(&https(), github.as_str())]).is_err());
}

#[test]
fn tenant_repo_kustomization_lists_only_tenant_resources() {
    let monorepo: Vec<String> = [
        "default/rbac.yaml",
        "acme/rbac.yaml",
        "acme/git-repository.yaml",
        "acme/sync.yaml",
        "acme/apps/web/release.yaml",
        "acme/apps/web/secrets.enc.yaml",
        "acme/apps/web/release.yaml",
        "acme-two/apps/x/release.yaml",
        "acme/infra/vm/stack.yaml",
    ]
    .map(str::to_string)
    .to_vec();
    let resources = tenant_repo_resources_from_monorepo("acme", &monorepo).unwrap();
    assert_eq!(
        resources,
        [
            "apps/web/release.yaml",
            "apps/web/secrets.enc.yaml",
            "infra/vm/stack.yaml"
        ]
    );
    let yaml = build_tenant_repo_kustomization_yaml("acme", &resources).unwrap();
    let v: Value = serde_yml::from_str(&yaml).unwrap();
    assert_eq!(v["kind"], "Kustomization");
    assert_eq!(v["resources"].as_array().unwrap().len(), 3);
    let empty: Value =
        serde_yml::from_str(&build_tenant_repo_kustomization_yaml("acme", &[]).unwrap()).unwrap();
    assert_eq!(empty["resources"], json!([]));
    assert!(build_tenant_repo_kustomization_yaml("acme", &["sync.yaml".to_string()]).is_err());
    for bad in [
        "acme/../default/rbac.yaml",
        "acme//x.yaml",
        "acme/./x.yaml",
        "acme/a\\b",
        "acme/github.com/o/r?ref=main",
    ] {
        assert!(
            tenant_repo_resources_from_monorepo("acme", &[bad.to_string()]).is_err(),
            "{bad}"
        );
    }
    // The builder refuses remote bases and paths outside `tenants/<id>/` itself.
    for bad in [
        "https://example.com/base.yaml",
        "github.com/o/r/deploy?ref=v1",
        "github.com/o/r/deploy",
        "git@github.com:o/r.git",
        "ssh://git@github.com/o/r.git",
        "../default/rbac.yaml",
        "apps/../../default",
        "/etc/passwd",
        "./apps/web",
        "apps//web",
        "apps\\web",
        "",
        "apps/web release.yaml",
    ] {
        assert!(
            build_tenant_repo_kustomization_yaml("acme", &[bad.to_string()]).is_err(),
            "{bad:?}"
        );
    }
    // Plain files at the tenant root (dots in a single segment) and nested dirs are fine.
    build_tenant_repo_kustomization_yaml(
        "acme",
        &[
            "secrets.enc.yaml".to_string(),
            "apps/v1.2/release.yaml".to_string(),
        ],
    )
    .unwrap();
}

/// Bypasses of the "never in Git" guard: Secrets nested in a `List`, and SOPS files whose
/// `encrypted_regex`/`unencrypted_suffix` left `data`/`stringData` values in plaintext.
#[test]
fn git_safety_guard_checks_lists_and_partially_encrypted_secrets() {
    let list = "apiVersion: v1\nkind: List\nitems:\n- apiVersion: v1\n  kind: Secret\n  metadata:\n    name: app\n  stringData:\n    k: v\n";
    assert!(ensure_git_safe_manifests("x.yaml", list).is_err());
    let typed_list = "apiVersion: v1\nkind: SecretList\nitems:\n- apiVersion: v1\n  kind: Secret\n  metadata:\n    name: keel-gitops-credentials\n";
    assert!(ensure_git_safe_manifests("x.yaml", typed_list).is_err());
    let nested = "apiVersion: v1\nkind: List\nitems:\n- apiVersion: v1\n  kind: List\n  items:\n  - apiVersion: v1\n    kind: Secret\n    metadata:\n      name: app\n    data:\n      k: dg==\n";
    assert!(ensure_git_safe_manifests("x.yaml", nested).is_err());
    let harmless_list = "apiVersion: v1\nkind: List\nitems:\n- apiVersion: v1\n  kind: ConfigMap\n  metadata:\n    name: c\n";
    ensure_git_safe_manifests("x.yaml", harmless_list).unwrap();

    for partial in [
        // `encrypted_regex: ^stringData$` leaves `data` in plaintext.
        "apiVersion: v1\nkind: Secret\nmetadata:\n  name: app\ndata:\n  k: cGxhaW4=\nstringData:\n  j: ENC[AES256_GCM,data:x]\nsops:\n  version: 3.9.0\n",
        // `unencrypted_suffix` keeps one value in clear text.
        "apiVersion: v1\nkind: Secret\nmetadata:\n  name: app\nstringData:\n  k_unencrypted: plain\n  j: ENC[AES256_GCM,data:x]\nsops:\n  version: 3.9.0\n",
        // A `sops` key that is not SOPS metadata.
        "apiVersion: v1\nkind: Secret\nmetadata:\n  name: app\nstringData:\n  k: ENC[AES256_GCM,data:x]\nsops: yes\n",
        "apiVersion: v1\nkind: Secret\nmetadata:\n  name: app\nstringData:\n  k: 1\nsops:\n  version: 3.9.0\n",
    ] {
        let err = ensure_git_safe_manifests("x.yaml", partial).unwrap_err();
        assert!(
            err.to_string().contains("refusing to write `x.yaml`"),
            "{err}"
        );
    }
}

/// Every manifest Keel already commits from this crate (tenant baseline `rbac.yaml`, both
/// stub variants) and every YAML file of the seed / bootstrap trees passes the guard, so
/// workflow 2 can call it from the shared commit helper without false positives.
#[test]
fn git_safety_guard_accepts_every_bundle_keel_writes() {
    let rbac = crate::TenantManifestBundle::synthesize(&TenantIsolationConfig::new("acme"))
        .unwrap()
        .to_multi_doc_yaml()
        .unwrap();
    ensure_git_safe_manifests("tenants/acme/rbac.yaml", &rbac).unwrap();
    for source in [https(), ssh()] {
        let stub =
            synthesize_tenant_repo_stub(&TenantIsolationConfig::new("acme"), &source).unwrap();
        for (path, yaml) in &stub.files {
            ensure_git_safe_manifests(path, yaml).unwrap();
        }
    }
    let kustomization =
        build_tenant_repo_kustomization_yaml("acme", &["apps/web/release.yaml".to_string()])
            .unwrap();
    ensure_git_safe_manifests("tenants/acme/kustomization.yaml", &kustomization).unwrap();

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy");
    let mut stack = vec![root.join("seed"), root.join("bootstrap/manifests")];
    let mut checked = 0;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "yaml" || e == "yml") {
                let yaml = std::fs::read_to_string(&path).unwrap();
                ensure_git_safe_manifests(&path.display().to_string(), &yaml)
                    .unwrap_or_else(|e| panic!("{e}"));
                checked += 1;
            }
        }
    }
    assert!(checked > 10, "seed manifests not found ({checked})");
}

/// The preview copy of `keel-gitops-credentials` holds exactly the keys PKO needs for the
/// transport (HTTPS: username/password), never tenant-planted or Flux-only keys, and only from
/// the platform-labelled Secret.
#[test]
fn preview_payload_is_exactly_the_pko_keys_of_the_platform_secret() {
    use k8s_openapi::ByteString;
    use k8s_openapi::api::core::v1::Secret;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    let secret = |labels: &[(&str, &str)], data: &[(&str, &str)]| Secret {
        metadata: ObjectMeta {
            name: Some(TENANT_GITOPS_CREDENTIALS_SECRET_NAME.to_string()),
            namespace: Some("tenant-acme".to_string()),
            labels: Some(
                labels
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            ..ObjectMeta::default()
        },
        data: Some(
            data.iter()
                .map(|(k, v)| (k.to_string(), ByteString(v.as_bytes().to_vec())))
                .collect(),
        ),
        ..Secret::default()
    };
    let platform = [
        ("keel.io/secret-kind", TENANT_GITOPS_CREDENTIALS_SECRET_KIND),
        (PLATFORM_OWNED_LABEL, "true"),
    ];
    let https_secret = secret(
        &platform,
        &[
            ("username", "keel-tenant-reader"),
            ("password", TOKEN),
            ("ca.crt", CA),
            ("bearerToken", "planted"),
            ("identity", "planted"),
        ],
    );
    let payload = tenant_repo_preview_secret_payload(&https(), &https_secret).unwrap();
    assert_eq!(
        payload.keys().map(String::as_str).collect::<Vec<_>>(),
        ["password", "username"]
    );

    let github = format!("github.com {HOST_KEY}\n");
    let ssh_secret = secret(
        &platform,
        &[
            ("sshPrivateKey", PRIVATE_KEY),
            ("identity", PRIVATE_KEY),
            ("known_hosts", github.as_str()),
        ],
    );
    let payload = tenant_repo_preview_secret_payload(&ssh(), &ssh_secret).unwrap();
    assert_eq!(
        payload.keys().map(String::as_str).collect::<Vec<_>>(),
        ["known_hosts", "sshPrivateKey"]
    );
    // A tenant-edited known_hosts with an extra host is refused.
    let planted = format!("{github}gitlab.com {HOST_KEY}\n");
    let tampered = secret(
        &platform,
        &[
            ("sshPrivateKey", PRIVATE_KEY),
            ("known_hosts", planted.as_str()),
        ],
    );
    assert!(tenant_repo_preview_secret_payload(&ssh(), &tampered).is_err());
    // Missing keys, missing labels, or another Secret fail closed.
    let missing = secret(&platform, &[("username", "u")]);
    assert!(tenant_repo_preview_secret_payload(&https(), &missing).is_err());
    let unowned = secret(
        &[("keel.io/secret-kind", TENANT_GITOPS_CREDENTIALS_SECRET_KIND)],
        &[("username", "u"), ("password", "p")],
    );
    assert!(tenant_repo_preview_secret_payload(&https(), &unowned).is_err());
    let mut renamed = https_secret.clone();
    renamed.metadata.name = Some("git-credentials".to_string());
    assert!(tenant_repo_preview_secret_payload(&https(), &renamed).is_err());
    let mut elsewhere = https_secret;
    elsewhere.metadata.namespace = Some("tenant-other".to_string());
    let err = tenant_repo_preview_secret_payload(&https(), &elsewhere).unwrap_err();
    assert!(!err.to_string().contains(TOKEN));
}
