//! Mock-HTTP tests for the per-repository forge operations (`remote_repo.rs`).

use super::*;
use crate::forge::RepoCredentialKind;
use crate::ssh_keys::generate_ed25519_key_pair;
use crate::test_http::{request_body, request_line, spawn_mock_http_server};

fn r(status: u16, body: impl Into<String>) -> (u16, String) {
    (status, body.into())
}

fn target(owner: &str, name: &str) -> RepositoryTarget {
    RepositoryTarget::new(owner, name).unwrap()
}

fn password() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// A fresh public key plus its fingerprint.
fn public_key() -> (String, String) {
    let pair = generate_ed25519_key_pair("keel-test").unwrap();
    (pair.public_key_openssh, pair.fingerprint_sha256)
}

fn key_json(id: u64, key: &str, read_only: bool) -> Value {
    json!({ "id": id, "title": "k", "key": key, "read_only": read_only })
}

// ----------------------------------------------------------------------------
// Pure helpers
// ----------------------------------------------------------------------------

#[test]
fn helpers_validate_ids_segments_and_plan_deploy_keys() {
    for good in ["acme", "tenant-acme", "a.b_c-1", "A", "x.git"] {
        assert_eq!(single_segment(good, "owner").unwrap(), good);
    }
    for bad in [
        "a/b", "a b", "..", "", "a\\b", "a?b", ".", "%2e%2e", ".%2E", "%2e.", "%2f", "a%2fb", "-a",
        "_a", ".hidden", "a..b", "a@b", "a#b", " a", "a\n",
    ] {
        assert!(single_segment(bad, "owner").is_err(), "{bad}");
    }
    assert!(single_segment(&"a".repeat(101), "owner").is_err());
    assert_eq!(
        namespace_path("team/tenants", "owner").unwrap(),
        "team/tenants"
    );
    for bad in ["team/", "/team", "team//x", "team/..", "team/%2e%2e", ""] {
        assert!(namespace_path(bad, "owner").is_err(), "{bad}");
    }
    assert!(RepositoryTarget::new("o", "%2e%2e").is_err());
    assert!(RepositoryTarget::new("team/../x", "r").is_err());

    assert_eq!(effective_permission(None), "none");
    assert_eq!(
        effective_permission(Some(&json!({"permission": "admin", "role_name": "admin"}))),
        "admin"
    );
    assert_eq!(
        effective_permission(Some(&json!({"permission": "read", "role_name": "triage"}))),
        "triage"
    );
    assert_eq!(
        effective_permission(Some(&json!({"permission": "none", "role_name": "read"}))),
        "none"
    );
    assert_eq!(effective_permission(Some(&json!({}))), "unknown");

    assert!(repo_read_token_name("keel-tenant-repo-acme").is_ok());
    for bad in ["t", "keel-tenant-repo-", "keel-tenant-repo-a\nb", " "] {
        assert!(repo_read_token_name(bad).is_err(), "{bad}");
    }

    assert!(github_fork_policy_refusal(
        r#"{"message":"Validation Failed","errors":["Forking is disabled by the organization"]}"#
    ));
    assert!(!github_fork_policy_refusal(
        r#"{"message":"Must have admin rights to Repository.","documentation_url":"https://docs.github.com/rest/repos/forks"}"#
    ));
    assert_eq!(numeric_id("42", "x").unwrap(), "42");
    for bad in ["", "4a", "../1", "-1", "123456789012345678901"] {
        assert!(numeric_id(bad, "x").is_err(), "{bad}");
    }
    assert!(fields_match(&json!({"a": 1, "b": 2}), &json!({"a": 1})));
    assert!(!fields_match(&json!({"a": 2}), &json!({"a": 1})));
    assert!(!fields_match(&json!({}), &json!({"a": 1})));

    let key = |id: &str, fp: &str, ro: bool| DeployKeyInfo {
        id: id.to_string(),
        title: String::new(),
        fingerprint: Some(fp.to_string()),
        read_only: ro,
    };
    let keys = [
        key("1", "A", false),
        key("2", "A", true),
        key("3", "B", true),
    ];
    assert_eq!(
        plan_deploy_key(&keys, "A", true),
        DeployKeyPlan {
            keep: Some(keys[1].clone()),
            delete: vec!["1".to_string()],
        }
    );
    assert_eq!(
        plan_deploy_key(&keys, "C", true),
        DeployKeyPlan {
            keep: None,
            delete: vec![],
        }
    );
    assert_eq!(
        plan_deploy_key(&keys[..1], "A", true),
        DeployKeyPlan {
            keep: None,
            delete: vec!["1".to_string()],
        }
    );

    assert!(validated_deploy_key("", "ssh-ed25519 AAAA").is_err());
    assert!(validated_deploy_key("t\n", "ssh-ed25519 AAAA").is_err());
    let (pk, _) = public_key();
    let (title, normalized) = validated_deploy_key(" title ", &pk).unwrap();
    assert_eq!(title, "title");
    assert!(!normalized.ends_with("keel-test"));
}

#[test]
fn protection_matchers_only_accept_keel_settings() {
    let gh = json!({
        "enforce_admins": {"enabled": true},
        "allow_force_pushes": {"enabled": false},
        "allow_deletions": {"enabled": false},
        "required_pull_request_reviews": {"required_approving_review_count": 0},
    });
    assert!(github_protection_matches(&gh));
    let mut empty_bypass = gh.clone();
    empty_bypass["required_pull_request_reviews"]["bypass_pull_request_allowances"] =
        json!({"users": [], "teams": [], "apps": []});
    empty_bypass["restrictions"] = Value::Null;
    assert!(github_protection_matches(&empty_bypass));
    for (key, value) in [
        ("users", json!([{"login": "mallory"}])),
        ("teams", json!([{"slug": "devs"}])),
        ("apps", json!([{"slug": "bot"}])),
    ] {
        let mut bypass = gh.clone();
        bypass["required_pull_request_reviews"]["bypass_pull_request_allowances"] =
            json!({ key: value });
        assert!(!github_protection_matches(&bypass), "{key}");
    }
    let mut restricted = gh.clone();
    restricted["restrictions"] = json!({"users": [], "teams": [], "apps": []});
    assert!(!github_protection_matches(&restricted));
    let mut locked = gh.clone();
    locked["lock_branch"] = json!({"enabled": true});
    assert!(!github_protection_matches(&locked));
    let mut weaker = gh.clone();
    weaker["enforce_admins"]["enabled"] = json!(false);
    assert!(!github_protection_matches(&weaker));
    let mut no_prs = gh.clone();
    no_prs
        .as_object_mut()
        .unwrap()
        .remove("required_pull_request_reviews");
    assert!(!github_protection_matches(&no_prs));

    let gl = json!({
        "push_access_levels": [{"access_level": 0}],
        "merge_access_levels": [{"access_level": 40}],
        "allow_force_push": false,
    });
    assert!(gitlab_protection_matches(&gl));
    let default_rule = json!({
        "push_access_levels": [{"access_level": 40}],
        "merge_access_levels": [{"access_level": 40}],
        "allow_force_push": false,
    });
    assert!(!gitlab_protection_matches(&default_rule));
    let user_grant = json!({
        "push_access_levels": [{"access_level": 0}, {"access_level": 40, "user_id": 5}],
        "merge_access_levels": [{"access_level": 40}],
    });
    assert!(!gitlab_protection_matches(&user_grant));
    let force = json!({
        "push_access_levels": [{"access_level": 0}],
        "merge_access_levels": [{"access_level": 40}],
        "allow_force_push": true,
    });
    assert!(!gitlab_protection_matches(&force));
    assert!(github_plan_limited(
        "Upgrade to GitHub Pro or make this repository public to enable this feature."
    ));
    assert!(!github_plan_limited(
        "Resource not accessible by integration"
    ));
}

// ----------------------------------------------------------------------------
// Forgejo / Gitea
// ----------------------------------------------------------------------------

#[tokio::test]
async fn forgejo_for_repository_scopes_pr_calls_and_keeps_credentials() {
    let (url, handle) = spawn_mock_http_server(vec![r(200, "[]")]).await;
    let p = ForgejoGiteaProvider::new(&url, "keel", "infra-gitops", "fg-tok").unwrap();
    let scoped = p.for_repository("keel-tenants", "tenant-acme").unwrap();
    assert!(
        scoped
            .list_open_pull_requests(None)
            .await
            .unwrap()
            .is_empty()
    );
    let reqs = handle.await.unwrap();
    assert!(
        request_line(&reqs[0]).starts_with("GET /api/v1/repos/keel-tenants/tenant-acme/pulls?"),
        "{}",
        reqs[0]
    );
    assert!(
        reqs[0]
            .to_ascii_lowercase()
            .contains("authorization: token fg-tok")
    );
    for (o, n) in [("a/b", "r"), ("o", ".."), ("", "r")] {
        assert!(matches!(
            p.for_repository(o, n),
            Err(ForgeError::InvalidInput(_))
        ));
    }
    assert_eq!(
        p.read_credential_kind().unwrap(),
        RepoCredentialKind::HttpsToken
    );
}

#[tokio::test]
async fn forgejo_ensure_named_repository_existing_and_created_in_org() {
    let (url, handle) = spawn_mock_http_server(vec![r(
        200,
        r#"{"name":"tenant-acme","owner":{"login":"keel-tenants"},"private":true,"default_branch":"main","clone_url":"https://git.example.com/keel-tenants/tenant-acme.git","html_url":"https://git.example.com/keel-tenants/tenant-acme"}"#,
    )])
    .await;
    let p = ForgejoGiteaProvider::new(&url, "keel", "infra-gitops", "fg-tok").unwrap();
    let info = p
        .ensure_named_repository("keel-tenants", "tenant-acme", &RepositoryOptions::default())
        .await
        .unwrap();
    assert_eq!(info.outcome, RepositoryEnsureOutcome::Existed);
    assert_eq!(info.target(), target("keel-tenants", "tenant-acme"));
    assert!(info.private);
    assert_eq!(info.default_branch, "main");
    assert_eq!(
        info.clone_url.as_deref(),
        Some("https://git.example.com/keel-tenants/tenant-acme.git")
    );
    assert_eq!(handle.await.unwrap().len(), 1);

    let (url, handle) = spawn_mock_http_server(vec![
        r(404, "{}"),
        r(200, r#"{"login":"keel"}"#),
        r(200, r#"{"id":3,"username":"keel-tenants"}"#),
        r(
            201,
            r#"{"name":"tenant-acme","owner":{"login":"keel-tenants"},"private":true,"default_branch":"main"}"#,
        ),
    ])
    .await;
    let p = ForgejoGiteaProvider::new(&url, "keel", "infra-gitops", "fg-tok").unwrap();
    let opts = RepositoryOptions {
        description: Some("Tenant acme".to_string()),
        ..RepositoryOptions::default()
    };
    let info = p
        .ensure_named_repository("keel-tenants", "tenant-acme", &opts)
        .await
        .unwrap();
    assert_eq!(info.outcome, RepositoryEnsureOutcome::Created);
    let reqs = handle.await.unwrap();
    assert_eq!(
        request_line(&reqs[0]),
        "GET /api/v1/repos/keel-tenants/tenant-acme HTTP/1.1"
    );
    assert_eq!(request_line(&reqs[1]), "GET /api/v1/user HTTP/1.1");
    assert_eq!(
        request_line(&reqs[2]),
        "GET /api/v1/orgs/keel-tenants HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[3]),
        "POST /api/v1/orgs/keel-tenants/repos HTTP/1.1"
    );
    assert_eq!(
        request_body(&reqs[3]),
        json!({"name": "tenant-acme", "private": true, "auto_init": true, "default_branch": "main", "description": "Tenant acme"})
    );

    // Invalid names never reach the forge.
    for (o, n) in [
        ("a/b", "r"),
        ("%2e%2e", "r"),
        ("o", ".%2E"),
        ("o", "%2f"),
        ("o", "."),
    ] {
        assert!(matches!(
            p.ensure_named_repository(o, n, &RepositoryOptions::default())
                .await,
            Err(ForgeError::InvalidInput(_))
        ));
    }

    // Existing repositories are adopted only when they are what was requested: public (or
    // `private` missing), or answered for another owner/name (rename redirect), are refused.
    let (url, handle) = spawn_mock_http_server(vec![
        r(
            200,
            r#"{"name":"tenant-acme","owner":{"login":"keel-tenants"},"private":false}"#,
        ),
        r(
            200,
            r#"{"name":"tenant-acme","owner":{"login":"keel-tenants"}}"#,
        ),
        r(
            200,
            r#"{"name":"other","owner":{"login":"keel-tenants"},"private":true}"#,
        ),
        r(
            200,
            r#"{"name":"tenant-acme","owner":{"login":"elsewhere"},"private":true}"#,
        ),
        // Not requesting privacy: a public repository is adopted.
        r(
            200,
            r#"{"name":"Tenant-Acme","owner":{"login":"Keel-Tenants"},"private":false}"#,
        ),
    ])
    .await;
    let p = ForgejoGiteaProvider::new(&url, "keel", "infra-gitops", "fg-tok").unwrap();
    for expected in ["not private", "not private", "renamed", "renamed"] {
        let err = p
            .ensure_named_repository("keel-tenants", "tenant-acme", &RepositoryOptions::default())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ForgeError::InvalidInput(m) if m.contains(expected)),
            "{err}"
        );
    }
    let public = RepositoryOptions {
        private: false,
        ..RepositoryOptions::default()
    };
    let info = p
        .ensure_named_repository("keel-tenants", "tenant-acme", &public)
        .await
        .unwrap();
    assert!(!info.private);
    assert_eq!(handle.await.unwrap().len(), 5);
}

#[tokio::test]
async fn forgejo_repo_read_token_is_minted_by_the_bot_and_revocable() {
    let (url, handle) = spawn_mock_http_server(vec![
        r(
            201,
            r#"{"id":9,"name":"keel-tenant-repo-acme","sha1":"s3cr3t-value","scopes":["read:repository"],"repositories":[{"owner":"keel-tenants","name":"tenant-acme"}]}"#,
        ),
        r(204, ""),
        r(404, "{}"),
        r(500, "boom"),
    ])
    .await;
    let pw = password();
    let bot =
        ForgejoGiteaProvider::with_basic_auth(&url, "keel-tenants", "x", "keel-tenant-reader", &pw)
            .unwrap();
    let repo = target("keel-tenants", "tenant-acme");
    let token = bot
        .create_repo_read_token(&repo, "keel-tenant-repo-acme")
        .await
        .unwrap();
    assert_eq!(token.id, "9");
    assert_eq!(token.username, "keel-tenant-reader");
    assert_eq!(token.token, "s3cr3t-value");
    let debug = format!("{token:?}");
    assert!(!debug.contains("s3cr3t-value"), "{debug}");
    // Revoke: 204, then 404 (already gone) both succeed; 500 fails.
    bot.revoke_repo_read_token(&repo, "9").await.unwrap();
    bot.revoke_repo_read_token(&repo, "9").await.unwrap();
    assert!(matches!(
        bot.revoke_repo_read_token(&repo, "9").await,
        Err(ForgeError::Api { status: 500, .. })
    ));
    assert!(matches!(
        bot.revoke_repo_read_token(&repo, "../1").await,
        Err(ForgeError::InvalidInput(_))
    ));
    let reqs = handle.await.unwrap();
    assert_eq!(
        request_line(&reqs[0]),
        "POST /api/v1/users/keel-tenant-reader/tokens HTTP/1.1"
    );
    assert_eq!(
        request_body(&reqs[0]),
        json!({"name": "keel-tenant-repo-acme", "scopes": ["read:repository"], "repositories": [{"owner": "keel-tenants", "name": "tenant-acme"}]})
    );
    assert_eq!(
        request_line(&reqs[1]),
        "DELETE /api/v1/users/keel-tenant-reader/tokens/9 HTTP/1.1"
    );

    // A token-authenticated provider cannot mint (Forgejo requires basic auth as the owner).
    let token_auth = ForgejoGiteaProvider::new(&url, "o", "r", "tok").unwrap();
    assert!(matches!(
        token_auth
            .create_repo_read_token(&repo, "keel-tenant-repo-n")
            .await,
        Err(ForgeError::InvalidInput(_))
    ));
    // Names without the Stage 0 prefix (which Stage 0 would delete) never reach the forge.
    assert!(matches!(
        bot.create_repo_read_token(&repo, "acme").await,
        Err(ForgeError::InvalidInput(ref m)) if m.contains(REPO_READ_TOKEN_NAME_PREFIX)
    ));
}

#[tokio::test]
async fn forgejo_repo_read_token_name_reuse() {
    let taken = r#"{"message":"access token name has been used already"}"#;
    let minted = r#"{"id":21,"name":"keel-tenant-repo-acme","sha1":"fresh","scopes":["read:repository"],"repositories":[{"owner":"keel-tenants","name":"tenant-acme"}]}"#;
    let (url, handle) = spawn_mock_http_server(vec![
        // Taken by this repository's earlier token: deleted and re-minted.
        r(400, taken),
        r(
            200,
            r#"[{"id":20,"name":"keel-tenant-repo-acme","scopes":["read:repository"],"repositories":[{"owner":"keel-tenants","name":"tenant-acme"}]}]"#,
        ),
        r(204, ""),
        r(201, minted),
        // Taken by a token for another repository: refused, nothing deleted.
        r(400, taken),
        r(
            200,
            r#"[{"id":30,"name":"keel-tenant-repo-acme","scopes":["read:repository"],"repositories":[{"owner":"keel-tenants","name":"tenant-other"}]}]"#,
        ),
    ])
    .await;
    let bot = ForgejoGiteaProvider::with_basic_auth(
        &url,
        "keel-tenants",
        "x",
        "keel-tenant-reader",
        password(),
    )
    .unwrap();
    let repo = target("keel-tenants", "tenant-acme");
    let token = bot
        .create_repo_read_token(&repo, "keel-tenant-repo-acme")
        .await
        .unwrap();
    assert_eq!(token.id, "21");
    assert_eq!(token.token, "fresh");
    let err = bot
        .create_repo_read_token(&repo, "keel-tenant-repo-acme")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ForgeError::InvalidInput(m) if m.contains("not restricted")),
        "{err}"
    );
    let reqs = handle.await.unwrap();
    assert!(request_line(&reqs[1]).starts_with("GET /api/v1/users/keel-tenant-reader/tokens?"));
    assert_eq!(
        request_line(&reqs[2]),
        "DELETE /api/v1/users/keel-tenant-reader/tokens/20 HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[3]),
        "POST /api/v1/users/keel-tenant-reader/tokens HTTP/1.1"
    );
    assert_eq!(reqs.len(), 6);
}

#[tokio::test]
async fn forgejo_repo_read_token_fails_closed_when_not_restricted() {
    // Gitea / pre-15 Forgejo ignore `repositories`: the token is deleted and an error returned.
    let (url, handle) = spawn_mock_http_server(vec![
        r(201, r#"{"id":4,"name":"t","sha1":"leaky"}"#),
        r(204, ""),
    ])
    .await;
    let bot = ForgejoGiteaProvider::with_basic_auth(&url, "o", "r", "bot", password()).unwrap();
    let err = bot
        .create_repo_read_token(&target("keel-tenants", "t"), "keel-tenant-repo-t")
        .await
        .unwrap_err();
    assert!(!err.to_string().contains("leaky"));
    let reqs = handle.await.unwrap();
    assert_eq!(
        request_line(&reqs[1]),
        "DELETE /api/v1/users/bot/tokens/4 HTTP/1.1"
    );
}

#[tokio::test]
async fn forgejo_deploy_keys_are_matched_by_fingerprint() {
    let (pk, fp) = public_key();
    let (other, _) = public_key();
    let repo = target("keel-tenants", "tenant-acme");

    // Created: no keys yet.
    let (url, handle) = spawn_mock_http_server(vec![
        r(200, json!([key_json(1, &other, true)]).to_string()),
        r(201, key_json(7, &pk, true).to_string()),
    ])
    .await;
    let p = ForgejoGiteaProvider::new(&url, "o", "r", "tok").unwrap();
    let ensured = p
        .ensure_deploy_key(&repo, "keel tenant-acme", &format!("{pk} comment"), true)
        .await
        .unwrap();
    assert_eq!(ensured.outcome, DeployKeyOutcome::Created);
    assert_eq!(ensured.key.id, "7");
    assert_eq!(ensured.key.fingerprint.as_deref(), Some(fp.as_str()));
    let reqs = handle.await.unwrap();
    assert!(
        request_line(&reqs[0])
            .starts_with("GET /api/v1/repos/keel-tenants/tenant-acme/keys?limit=50&page=1")
    );
    assert_eq!(
        request_line(&reqs[1]),
        "POST /api/v1/repos/keel-tenants/tenant-acme/keys HTTP/1.1"
    );
    let body = request_body(&reqs[1]);
    assert_eq!(body["read_only"], true);
    assert_eq!(body["title"], "keel tenant-acme");
    assert!(!body["key"].as_str().unwrap().contains("comment"));

    // Idempotent: the same key (different comment) already exists read-only.
    let (url, handle) = spawn_mock_http_server(vec![r(
        200,
        json!([key_json(7, &format!("{pk} other-comment"), true)]).to_string(),
    )])
    .await;
    let p = ForgejoGiteaProvider::new(&url, "o", "r", "tok").unwrap();
    let ensured = p.ensure_deploy_key(&repo, "t", &pk, true).await.unwrap();
    assert_eq!(ensured.outcome, DeployKeyOutcome::Existed);
    assert_eq!(handle.await.unwrap().len(), 1);

    // A writable copy of the key is replaced by a read-only one.
    let (url, handle) = spawn_mock_http_server(vec![
        r(200, json!([key_json(7, &pk, false)]).to_string()),
        r(204, ""),
        r(201, key_json(8, &pk, true).to_string()),
    ])
    .await;
    let p = ForgejoGiteaProvider::new(&url, "o", "r", "tok").unwrap();
    let ensured = p.ensure_deploy_key(&repo, "t", &pk, true).await.unwrap();
    assert_eq!(ensured.outcome, DeployKeyOutcome::Replaced);
    assert!(ensured.key.read_only);
    let reqs = handle.await.unwrap();
    assert_eq!(
        request_line(&reqs[1]),
        "DELETE /api/v1/repos/keel-tenants/tenant-acme/keys/7 HTTP/1.1"
    );

    // Concurrent add (422) resolves to the existing identical key; otherwise the 422 surfaces.
    let (url, handle) = spawn_mock_http_server(vec![
        r(200, "[]"),
        r(422, r#"{"message":"Key content has been used"}"#),
        r(200, json!([key_json(9, &pk, true)]).to_string()),
        r(200, "[]"),
        r(422, r#"{"message":"Key content has been used"}"#),
        r(200, "[]"),
    ])
    .await;
    let p = ForgejoGiteaProvider::new(&url, "o", "r", "tok").unwrap();
    let ensured = p.ensure_deploy_key(&repo, "t", &pk, true).await.unwrap();
    assert_eq!(ensured.outcome, DeployKeyOutcome::Existed);
    assert_eq!(ensured.key.id, "9");
    let err = p
        .ensure_deploy_key(&repo, "t", &pk, true)
        .await
        .unwrap_err();
    assert!(matches!(err, ForgeError::Api { status: 422, .. }), "{err}");
    handle.await.unwrap();

    // Invalid keys never reach the forge.
    assert!(matches!(
        p.ensure_deploy_key(&repo, "t", "not-a-key", true).await,
        Err(ForgeError::InvalidInput(_))
    ));

    // List + revoke (404 = ok) + revoke by fingerprint.
    let (url, handle) =
        spawn_mock_http_server(vec![
        r(
            200,
            json!([key_json(1, &pk, true), key_json(2, &other, true), {"id": 3, "key": "garbage"}])
                .to_string(),
        ),
        r(404, "{}"),
        r(403, r#"{"message":"forbidden"}"#),
        r(200, json!([key_json(1, &pk, true), key_json(2, &other, false)]).to_string()),
        r(204, ""),
    ])
        .await;
    let p = ForgejoGiteaProvider::new(&url, "o", "r", "tok").unwrap();
    let keys = p.list_deploy_keys(&repo).await.unwrap();
    assert_eq!(keys.len(), 3);
    assert_eq!(keys[0].fingerprint.as_deref(), Some(fp.as_str()));
    assert!(keys[2].fingerprint.is_none());
    p.revoke_deploy_key(&repo, "1").await.unwrap();
    assert!(matches!(
        p.revoke_deploy_key(&repo, "1").await,
        Err(ForgeError::Api { status: 403, .. })
    ));
    assert_eq!(
        p.revoke_deploy_keys_by_fingerprint(&repo, &fp)
            .await
            .unwrap(),
        1
    );
    let reqs = handle.await.unwrap();
    assert_eq!(
        request_line(&reqs[4]),
        "DELETE /api/v1/repos/keel-tenants/tenant-acme/keys/1 HTTP/1.1"
    );
}

#[tokio::test]
async fn forgejo_main_protection_get_then_post_or_patch() {
    let repo = target("keel-tenants", "tenant-acme");
    let matching = json!({
        "rule_name": "main", "enable_push": false, "enable_push_whitelist": false,
        "enable_merge_whitelist": false, "required_approvals": 0, "apply_to_admins": true,
        "unprotected_file_patterns": "",
    });
    let branch = r#"{"name":"main"}"#;
    let (url, handle) = spawn_mock_http_server(vec![
        // Created
        r(200, branch),
        r(404, "{}"),
        r(201, matching.to_string()),
        // Unchanged
        r(200, branch),
        r(200, matching.to_string()),
        // Updated (drift: an unprotected file pattern bypasses `enable_push: false`)
        r(200, branch),
        r(
            200,
            json!({"rule_name": "main", "enable_push": false, "enable_push_whitelist": false,
                "enable_merge_whitelist": false, "required_approvals": 0,
                "apply_to_admins": true, "unprotected_file_patterns": "*.yaml"})
            .to_string(),
        ),
        r(200, matching.to_string()),
        // Concurrent create: POST 403, re-read finds Keel's rule.
        r(200, branch),
        r(404, "{}"),
        r(403, r#"{"message":"Branch protection already exist"}"#),
        r(200, matching.to_string()),
        // Real permission failure: POST 403, rule still missing.
        r(200, branch),
        r(404, "{}"),
        r(403, r#"{"message":"token does not have required scope"}"#),
        r(404, "{}"),
        // `main` does not exist yet: refused before touching protection.
        r(404, "{}"),
    ])
    .await;
    let p = ForgejoGiteaProvider::new(&url, "o", "r", "tok").unwrap();
    assert_eq!(
        p.ensure_main_protection(&repo).await.unwrap(),
        BranchProtectionOutcome::Created
    );
    assert_eq!(
        p.ensure_main_protection(&repo).await.unwrap(),
        BranchProtectionOutcome::Unchanged
    );
    assert_eq!(
        p.ensure_main_protection(&repo).await.unwrap(),
        BranchProtectionOutcome::Updated
    );
    assert_eq!(
        p.ensure_main_protection(&repo).await.unwrap(),
        BranchProtectionOutcome::Unchanged
    );
    assert!(matches!(
        p.ensure_main_protection(&repo).await,
        Err(ForgeError::Api { status: 403, .. })
    ));
    assert!(matches!(
        p.ensure_main_protection(&repo).await,
        Err(ForgeError::InvalidInput(ref m)) if m.contains("does not exist yet")
    ));
    let reqs = handle.await.unwrap();
    assert_eq!(reqs.len(), 17);
    assert_eq!(
        request_line(&reqs[0]),
        "GET /api/v1/repos/keel-tenants/tenant-acme/branches/main HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[1]),
        "GET /api/v1/repos/keel-tenants/tenant-acme/branch_protections/main HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[2]),
        "POST /api/v1/repos/keel-tenants/tenant-acme/branch_protections HTTP/1.1"
    );
    let body = request_body(&reqs[2]);
    assert_eq!(body["rule_name"], "main");
    assert_eq!(body["enable_push"], false);
    assert_eq!(body["apply_to_admins"], true);
    assert_eq!(body["required_approvals"], 0);
    assert_eq!(body["unprotected_file_patterns"], "");
    assert_eq!(
        request_line(&reqs[7]),
        "PATCH /api/v1/repos/keel-tenants/tenant-acme/branch_protections/main HTTP/1.1"
    );
    assert!(request_body(&reqs[7]).get("rule_name").is_none());
}

#[tokio::test]
async fn forgejo_fork_policy_is_the_instance_setting() {
    let (url, handle) = spawn_mock_http_server(vec![
        r(200, r#"{"forks_disabled":true}"#),
        r(200, r#"{"forks_disabled":false}"#),
        r(200, r#"{}"#),
    ])
    .await;
    let p = ForgejoGiteaProvider::new(&url, "o", "r", "tok").unwrap();
    let repo = target("keel-tenants", "t");
    assert_eq!(
        p.disable_forks(&repo).await.unwrap(),
        ForkPolicyOutcome::AlreadyDisabled
    );
    for _ in 0..2 {
        assert!(matches!(
            p.disable_forks(&repo).await.unwrap(),
            ForkPolicyOutcome::RequiresAdminSetting(ref m) if m.contains("DISABLE_FORKS")
        ));
    }
    let reqs = handle.await.unwrap();
    assert_eq!(
        request_line(&reqs[0]),
        "GET /api/v1/settings/repository HTTP/1.1"
    );
}

#[tokio::test]
async fn forgejo_read_collaborators_and_forks() {
    let repo = target("keel-tenants", "tenant-acme");
    let read = r#"{"permission":"read","role_name":"read"}"#;
    let (url, handle) = spawn_mock_http_server(vec![
        // Added (then effective permission re-read)
        r(404, "{}"),
        r(204, ""),
        r(200, read),
        // Unchanged
        r(204, ""),
        r(200, read),
        // Updated (write -> read)
        r(204, ""),
        r(200, r#"{"permission":"write"}"#),
        r(204, ""),
        r(200, read),
        // Error
        r(404, "{}"),
        r(403, r#"{"message":"no"}"#),
        // Remove: 204 then no access left; 404 (not a collaborator) then no access left
        r(204, ""),
        r(200, r#"{"permission":"none"}"#),
        r(404, "{}"),
        r(404, "{}"),
        // Forks
        r(
            200,
            r#"[{"full_name":"alice/tenant-acme","html_url":"https://f/alice/tenant-acme"},{"name":"no-full-name"}]"#,
        ),
    ])
    .await;
    let p = ForgejoGiteaProvider::new(&url, "o", "r", "tok").unwrap();
    assert_eq!(
        p.ensure_read_collaborator(&repo, "alice").await.unwrap(),
        CollaboratorOutcome::Added
    );
    assert_eq!(
        p.ensure_read_collaborator(&repo, "alice").await.unwrap(),
        CollaboratorOutcome::Unchanged
    );
    assert_eq!(
        p.ensure_read_collaborator(&repo, "alice").await.unwrap(),
        CollaboratorOutcome::Updated
    );
    assert!(matches!(
        p.ensure_read_collaborator(&repo, "alice").await,
        Err(ForgeError::Api { status: 403, .. })
    ));
    p.remove_collaborator(&repo, "alice").await.unwrap();
    p.remove_collaborator(&repo, "alice").await.unwrap();
    let forks = p.list_forks(&repo).await.unwrap();
    assert_eq!(
        forks,
        vec![ForkInfo {
            full_name: "alice/tenant-acme".to_string(),
            html_url: Some("https://f/alice/tenant-acme".to_string()),
        }]
    );
    let reqs = handle.await.unwrap();
    assert_eq!(
        request_line(&reqs[0]),
        "GET /api/v1/repos/keel-tenants/tenant-acme/collaborators/alice HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[1]),
        "PUT /api/v1/repos/keel-tenants/tenant-acme/collaborators/alice HTTP/1.1"
    );
    assert_eq!(request_body(&reqs[1]), json!({"permission": "read"}));
    assert_eq!(
        request_line(&reqs[2]),
        "GET /api/v1/repos/keel-tenants/tenant-acme/collaborators/alice/permission HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[11]),
        "DELETE /api/v1/repos/keel-tenants/tenant-acme/collaborators/alice HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[12]),
        "GET /api/v1/repos/keel-tenants/tenant-acme/collaborators/alice/permission HTTP/1.1"
    );
    assert!(
        request_line(&reqs[15]).starts_with("GET /api/v1/repos/keel-tenants/tenant-acme/forks?")
    );
    for bad in ["a/b", "%2e%2e", ".%2E", "%2f", "."] {
        assert!(matches!(
            p.ensure_read_collaborator(&repo, bad).await,
            Err(ForgeError::InvalidInput(_))
        ));
        assert!(matches!(
            p.remove_collaborator(&repo, bad).await,
            Err(ForgeError::InvalidInput(_))
        ));
    }
    // A crafted owner/name on a hand-built target never reaches the forge either.
    let crafted = RepositoryTarget {
        owner: "%2e%2e".to_string(),
        name: "r".to_string(),
    };
    let (pk, _) = public_key();
    assert!(matches!(
        p.ensure_deploy_key(&crafted, "t", &pk, true).await,
        Err(ForgeError::InvalidInput(_))
    ));
    assert!(matches!(
        p.remove_collaborator(&crafted, "alice").await,
        Err(ForgeError::InvalidInput(_))
    ));
    for (o, n) in [("%2e%2e", "r"), ("o", ".%2E"), ("o", "."), ("o", "a%2fb")] {
        assert!(matches!(
            p.for_repository(o, n),
            Err(ForgeError::InvalidInput(_))
        ));
    }
}

#[tokio::test]
async fn forgejo_collaborator_with_organisation_access_fails_closed() {
    let repo = target("keel-tenants", "tenant-acme");
    let (url, handle) = spawn_mock_http_server(vec![
        // Direct grant set to read, but the user is an organisation owner.
        r(204, ""),
        r(200, r#"{"permission":"owner","role_name":"owner"}"#),
        r(204, ""),
        r(200, r#"{"permission":"owner","role_name":"owner"}"#),
        // New collaborator who is in a write team.
        r(404, "{}"),
        r(204, ""),
        r(200, r#"{"permission":"write","role_name":"write"}"#),
        // Removal leaves team access.
        r(204, ""),
        r(200, r#"{"permission":"write","role_name":"write"}"#),
    ])
    .await;
    let p = ForgejoGiteaProvider::new(&url, "o", "r", "tok").unwrap();
    for expected in ["owner", "write"] {
        let err = p
            .ensure_read_collaborator(&repo, "alice")
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ForgeError::ElevatedAccess { user, permission, .. } if user == "alice" && permission == expected),
            "{err}"
        );
    }
    let err = p.remove_collaborator(&repo, "alice").await.unwrap_err();
    assert!(
        matches!(&err, ForgeError::ElevatedAccess { repository, .. } if repository == "keel-tenants/tenant-acme"),
        "{err}"
    );
    assert_eq!(handle.await.unwrap().len(), 9);
}

#[tokio::test]
async fn listings_follow_full_pages() {
    let page: Vec<Value> = (0..LIST_PAGE_SIZE)
        .map(|i| json!({"full_name": format!("u{i}/r")}))
        .collect();
    let (url, handle) = spawn_mock_http_server(vec![
        r(200, Value::Array(page).to_string()),
        r(200, r#"[{"full_name":"last/r"}]"#),
    ])
    .await;
    let p = ForgejoGiteaProvider::new(&url, "o", "r", "tok").unwrap();
    let forks = p.list_forks(&target("o", "r")).await.unwrap();
    assert_eq!(forks.len(), LIST_PAGE_SIZE + 1);
    let reqs = handle.await.unwrap();
    assert!(request_line(&reqs[1]).contains("page=2"));
}

/// Forgejo 15 unregisters `GET .../forks` while `DISABLE_FORKS` is set (plain-text 404).
#[tokio::test]
async fn forgejo_forks_listing_with_forks_disabled_uses_forks_count() {
    let route_gone = || r(404, "404 page not found");
    let (url, handle) = spawn_mock_http_server(vec![
        // No forks: empty listing.
        route_gone(),
        r(200, r#"{"name":"r","owner":{"login":"o"},"forks_count":0}"#),
        // Forks created before DISABLE_FORKS was set cannot be listed: fail closed.
        route_gone(),
        r(200, r#"{"name":"r","owner":{"login":"o"},"forks_count":2}"#),
        // No forks_count reported: fail closed.
        route_gone(),
        r(200, r#"{"name":"r","owner":{"login":"o"}}"#),
        // Missing repository stays NotFound.
        route_gone(),
        r(404, r#"{"message":"The target couldn't be found."}"#),
    ])
    .await;
    let p = ForgejoGiteaProvider::new(&url, "o", "r", "tok").unwrap();
    let repo = target("o", "r");
    assert!(p.list_forks(&repo).await.unwrap().is_empty());
    let err = p.list_forks(&repo).await.unwrap_err().to_string();
    assert!(
        err.contains("2 fork(s)") && err.contains("DISABLE_FORKS"),
        "{err}"
    );
    let err = p.list_forks(&repo).await.unwrap_err().to_string();
    assert!(err.contains("an unknown number of fork(s)"), "{err}");
    assert!(matches!(
        p.list_forks(&repo).await,
        Err(ForgeError::NotFound(_))
    ));
    let reqs = handle.await.unwrap();
    assert!(request_line(&reqs[0]).starts_with("GET /api/v1/repos/o/r/forks?"));
    assert_eq!(request_line(&reqs[1]), "GET /api/v1/repos/o/r HTTP/1.1");
}

// ----------------------------------------------------------------------------
// GitHub
// ----------------------------------------------------------------------------

#[tokio::test]
async fn github_for_repository_credentials_and_named_repository() {
    let p0 = GitHubProvider::new("https://api.github.com", "acme", "infra", "gh-tok").unwrap();
    assert_eq!(
        p0.read_credential_kind().unwrap(),
        RepoCredentialKind::SshDeployKey
    );
    let repo = target("acme", "tenant-a");
    assert!(matches!(
        p0.create_repo_read_token(&repo, "t").await,
        Err(ForgeError::Unsupported(ref m)) if m.contains("deploy key")
    ));
    assert!(matches!(
        p0.revoke_repo_read_token(&repo, "1").await,
        Err(ForgeError::Unsupported(_))
    ));

    let (url, handle) = spawn_mock_http_server(vec![
        // scoped PR listing
        r(200, "[]"),
        // created in org (auto_init, no default_branch on GitHub)
        r(404, "{}"),
        r(200, r#"{"login":"acme","type":"Organization"}"#),
        r(
            201,
            r#"{"name":"tenant-a","owner":{"login":"acme"},"private":true,"default_branch":"main","clone_url":"https://github.com/acme/tenant-a.git"}"#,
        ),
        // concurrent create: 422 "name already exists" => existing
        r(404, "{}"),
        r(200, r#"{"login":"acme","type":"Organization"}"#),
        r(
            422,
            r#"{"message":"Repository creation failed.","errors":[{"message":"name already exists on this account"}]}"#,
        ),
        r(200, r#"{"name":"tenant-a","owner":{"login":"acme"},"private":true}"#),
        // unknown owner
        r(404, "{}"),
        r(404, "{}"),
    ])
    .await;
    let p = GitHubProvider::new(&url, "acme", "infra", "gh-tok").unwrap();
    let scoped = p.for_repository("acme", "tenant-a").unwrap();
    scoped.list_open_pull_requests(None).await.unwrap();
    let created = p
        .ensure_named_repository("acme", "tenant-a", &RepositoryOptions::default())
        .await
        .unwrap();
    assert_eq!(created.outcome, RepositoryEnsureOutcome::Created);
    assert_eq!(created.default_branch, "main");
    let existed = p
        .ensure_named_repository("acme", "tenant-a", &RepositoryOptions::default())
        .await
        .unwrap();
    assert_eq!(existed.outcome, RepositoryEnsureOutcome::Existed);
    assert!(matches!(
        p.ensure_named_repository("ghost", "r", &RepositoryOptions::default())
            .await,
        Err(ForgeError::NotFound(ref o)) if o == "ghost"
    ));
    let reqs = handle.await.unwrap();
    assert!(request_line(&reqs[0]).starts_with("GET /repos/acme/tenant-a/pulls?"));
    assert_eq!(request_line(&reqs[3]), "POST /orgs/acme/repos HTTP/1.1");
    assert_eq!(
        request_body(&reqs[3]),
        json!({"name": "tenant-a", "private": true, "auto_init": true})
    );
    assert_eq!(request_line(&reqs[7]), "GET /repos/acme/tenant-a HTTP/1.1");
}

#[tokio::test]
async fn github_named_repository_renames_a_non_main_initial_branch() {
    let (url, handle) = spawn_mock_http_server(vec![
        r(404, "{}"),
        r(200, r#"{"login":"acme","type":"Organization"}"#),
        r(
            201,
            r#"{"name":"tenant-a","owner":{"login":"acme"},"private":true,"default_branch":"master"}"#,
        ),
        r(201, r#"{"name":"main"}"#),
        // Rename refused: surfaces.
        r(404, "{}"),
        r(200, r#"{"login":"acme","type":"Organization"}"#),
        r(
            201,
            r#"{"name":"tenant-b","owner":{"login":"acme"},"private":true,"default_branch":"master"}"#,
        ),
        r(403, r#"{"message":"Resource not accessible"}"#),
        // Without auto_init nothing is renamed (no branch exists yet).
        r(404, "{}"),
        r(200, r#"{"login":"acme","type":"Organization"}"#),
        r(
            201,
            r#"{"name":"tenant-c","owner":{"login":"acme"},"private":true,"default_branch":"master"}"#,
        ),
    ])
    .await;
    let p = GitHubProvider::new(&url, "acme", "infra", "gh-tok").unwrap();
    let info = p
        .ensure_named_repository("acme", "tenant-a", &RepositoryOptions::default())
        .await
        .unwrap();
    assert_eq!(info.default_branch, "main");
    assert!(matches!(
        p.ensure_named_repository("acme", "tenant-b", &RepositoryOptions::default())
            .await,
        Err(ForgeError::Api { status: 403, .. })
    ));
    let bare = RepositoryOptions {
        auto_init: false,
        ..RepositoryOptions::default()
    };
    let info = p
        .ensure_named_repository("acme", "tenant-c", &bare)
        .await
        .unwrap();
    assert_eq!(info.default_branch, "master");
    let reqs = handle.await.unwrap();
    assert_eq!(
        request_line(&reqs[3]),
        "POST /repos/acme/tenant-a/branches/master/rename HTTP/1.1"
    );
    assert_eq!(request_body(&reqs[3]), json!({"new_name": "main"}));
    assert_eq!(reqs.len(), 11);
}

#[tokio::test]
async fn github_deploy_keys_and_host_keys() {
    let (pk, fp) = public_key();
    let repo = target("acme", "tenant-a");
    let (url, handle) = spawn_mock_http_server(vec![
        r(200, "[]"),
        r(201, key_json(11, &pk, true).to_string()),
        r(200, json!([key_json(11, &pk, true)]).to_string()),
        r(204, ""),
        r(
            200,
            json!({"ssh_keys": [
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl",
                "bogus"
            ]})
            .to_string(),
        ),
        r(200, "{}"),
    ])
    .await;
    let p = GitHubProvider::new(&url, "acme", "infra", "gh-tok").unwrap();
    let ensured = p.ensure_deploy_key(&repo, "keel", &pk, true).await.unwrap();
    assert_eq!(ensured.outcome, DeployKeyOutcome::Created);
    let keys = p.list_deploy_keys(&repo).await.unwrap();
    assert_eq!(keys[0].fingerprint.as_deref(), Some(fp.as_str()));
    p.revoke_deploy_key(&repo, "11").await.unwrap();
    let host_keys = p.ssh_host_keys().await.unwrap();
    assert_eq!(host_keys.len(), 1);
    assert!(p.ssh_host_keys().await.is_err());
    let reqs = handle.await.unwrap();
    assert!(request_line(&reqs[0]).starts_with("GET /repos/acme/tenant-a/keys?per_page=50&page=1"));
    assert_eq!(
        request_line(&reqs[1]),
        "POST /repos/acme/tenant-a/keys HTTP/1.1"
    );
    assert_eq!(request_body(&reqs[1])["read_only"], true);
    assert_eq!(
        request_line(&reqs[3]),
        "DELETE /repos/acme/tenant-a/keys/11 HTTP/1.1"
    );
    assert_eq!(request_line(&reqs[4]), "GET /meta HTTP/1.1");
}

#[tokio::test]
async fn github_main_protection_put_and_plan_limits() {
    let repo = target("acme", "tenant-a");
    let matching = json!({
        "enforce_admins": {"enabled": true},
        "allow_force_pushes": {"enabled": false},
        "allow_deletions": {"enabled": false},
        "required_pull_request_reviews": {"required_approving_review_count": 0},
    });
    let upgrade = r#"{"message":"Upgrade to GitHub Pro or make this repository public to enable this feature."}"#;
    let mut bypass = matching.clone();
    bypass["required_pull_request_reviews"]["bypass_pull_request_allowances"] =
        json!({"users": [{"login": "mallory"}], "teams": [], "apps": []});
    let branch = r#"{"name":"main"}"#;
    let (url, handle) = spawn_mock_http_server(vec![
        // Created
        r(200, branch),
        r(404, r#"{"message":"Branch not protected"}"#),
        r(200, matching.to_string()),
        // Unchanged
        r(200, branch),
        r(200, matching.to_string()),
        // Updated
        r(200, branch),
        r(
            200,
            json!({"enforce_admins": {"enabled": false}}).to_string(),
        ),
        r(200, matching.to_string()),
        // Updated: a bypass actor could push past the PR requirement
        r(200, branch),
        r(200, bypass.to_string()),
        r(200, matching.to_string()),
        // Free private: GET 403 => UnsupportedPlan
        r(200, branch),
        r(403, upgrade),
        // PUT 403 => UnsupportedPlan
        r(200, branch),
        r(404, "{}"),
        r(403, upgrade),
        // Other 403 => error
        r(200, branch),
        r(
            403,
            r#"{"message":"Resource not accessible by personal access token"}"#,
        ),
        // Missing `main`
        r(404, r#"{"message":"Branch not found"}"#),
    ])
    .await;
    let p = GitHubProvider::new(&url, "acme", "infra", "gh-tok").unwrap();
    for expected in [
        BranchProtectionOutcome::Created,
        BranchProtectionOutcome::Unchanged,
        BranchProtectionOutcome::Updated,
        BranchProtectionOutcome::Updated,
        BranchProtectionOutcome::UnsupportedPlan,
        BranchProtectionOutcome::UnsupportedPlan,
    ] {
        assert_eq!(p.ensure_main_protection(&repo).await.unwrap(), expected);
    }
    assert!(matches!(
        p.ensure_main_protection(&repo).await,
        Err(ForgeError::Api { status: 403, .. })
    ));
    assert!(matches!(
        p.ensure_main_protection(&repo).await,
        Err(ForgeError::InvalidInput(_))
    ));
    let reqs = handle.await.unwrap();
    assert_eq!(reqs.len(), 19);
    assert_eq!(
        request_line(&reqs[0]),
        "GET /repos/acme/tenant-a/branches/main HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[1]),
        "GET /repos/acme/tenant-a/branches/main/protection HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[2]),
        "PUT /repos/acme/tenant-a/branches/main/protection HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[10]),
        "PUT /repos/acme/tenant-a/branches/main/protection HTTP/1.1"
    );
    let body = request_body(&reqs[2]);
    assert_eq!(body["lock_branch"], false);
    assert_eq!(body["enforce_admins"], true);
    assert_eq!(body["allow_force_pushes"], false);
    assert_eq!(body["allow_deletions"], false);
    assert_eq!(
        body["required_pull_request_reviews"]["required_approving_review_count"],
        0
    );
    assert!(body["restrictions"].is_null());
}

#[tokio::test]
async fn github_fork_policy_per_repository() {
    let repo = target("acme", "tenant-a");
    let (url, handle) = spawn_mock_http_server(vec![
        r(200, r#"{"private":true,"allow_forking":false}"#),
        r(200, r#"{"private":true,"allow_forking":true}"#),
        r(200, r#"{"private":true,"allow_forking":false}"#),
        r(200, r#"{"private":true,"allow_forking":true}"#),
        r(
            422,
            r#"{"message":"forking is disabled by the organization"}"#,
        ),
        r(200, r#"{"private":false,"allow_forking":true}"#),
        r(404, "{}"),
        // A token without admin rights is a credential error, not an org setting.
        r(200, r#"{"private":true,"allow_forking":true}"#),
        r(
            403,
            r#"{"message":"Must have admin rights to Repository.","documentation_url":"https://docs.github.com/rest/repos/repos#update-a-repository"}"#,
        ),
    ])
    .await;
    let p = GitHubProvider::new(&url, "acme", "infra", "gh-tok").unwrap();
    assert_eq!(
        p.disable_forks(&repo).await.unwrap(),
        ForkPolicyOutcome::AlreadyDisabled
    );
    assert_eq!(
        p.disable_forks(&repo).await.unwrap(),
        ForkPolicyOutcome::Disabled
    );
    assert!(matches!(
        p.disable_forks(&repo).await.unwrap(),
        ForkPolicyOutcome::RequiresAdminSetting(ref m) if m.contains("members_can_fork_private_repositories")
    ));
    assert!(matches!(
        p.disable_forks(&repo).await.unwrap(),
        ForkPolicyOutcome::RequiresAdminSetting(ref m) if m.contains("public")
    ));
    assert!(matches!(
        p.disable_forks(&repo).await,
        Err(ForgeError::NotFound(_))
    ));
    assert!(matches!(
        p.disable_forks(&repo).await,
        Err(ForgeError::Api { status: 403, .. })
    ));
    let reqs = handle.await.unwrap();
    assert_eq!(
        request_line(&reqs[2]),
        "PATCH /repos/acme/tenant-a HTTP/1.1"
    );
    assert_eq!(request_body(&reqs[2]), json!({"allow_forking": false}));
    assert_eq!(reqs.len(), 9);
}

#[tokio::test]
async fn github_collaborators_invitations_and_forks() {
    let repo = target("acme", "tenant-a");
    let read = r#"{"permission":"read","role_name":"read"}"#;
    let (url, handle) = spawn_mock_http_server(vec![
        // Invited (201): no access until accepted
        r(404, "{}"),
        r(201, r#"{"id":5}"#),
        r(200, r#"{"permission":"none","role_name":"none"}"#),
        // Added (204)
        r(404, "{}"),
        r(204, ""),
        r(200, read),
        // Unchanged
        r(204, ""),
        r(200, read),
        // Updated (direct admin -> pull)
        r(204, ""),
        r(200, r#"{"permission":"admin","role_name":"admin"}"#),
        r(204, ""),
        r(200, read),
        // Remove: collaborator 204, invitations listing, cancel alice's invitation, no access left
        r(204, ""),
        r(
            200,
            r#"[{"id":5,"invitee":{"login":"Alice"}},{"id":6,"invitee":{"login":"bob"}}]"#,
        ),
        r(204, ""),
        r(404, "{}"),
        // Forks
        r(200, r#"[{"full_name":"mallory/tenant-a","html_url":"https://github.com/mallory/tenant-a"}]"#),
    ])
    .await;
    let p = GitHubProvider::new(&url, "acme", "infra", "gh-tok").unwrap();
    for expected in [
        CollaboratorOutcome::Invited,
        CollaboratorOutcome::Added,
        CollaboratorOutcome::Unchanged,
        CollaboratorOutcome::Updated,
    ] {
        assert_eq!(
            p.ensure_read_collaborator(&repo, "alice").await.unwrap(),
            expected
        );
    }
    p.remove_collaborator(&repo, "alice").await.unwrap();
    assert_eq!(p.list_forks(&repo).await.unwrap().len(), 1);
    let reqs = handle.await.unwrap();
    assert_eq!(
        request_line(&reqs[1]),
        "PUT /repos/acme/tenant-a/collaborators/alice HTTP/1.1"
    );
    assert_eq!(request_body(&reqs[1]), json!({"permission": "pull"}));
    assert_eq!(
        request_line(&reqs[2]),
        "GET /repos/acme/tenant-a/collaborators/alice/permission HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[12]),
        "DELETE /repos/acme/tenant-a/collaborators/alice HTTP/1.1"
    );
    assert!(request_line(&reqs[13]).starts_with("GET /repos/acme/tenant-a/invitations?"));
    assert_eq!(
        request_line(&reqs[14]),
        "DELETE /repos/acme/tenant-a/invitations/5 HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[15]),
        "GET /repos/acme/tenant-a/collaborators/alice/permission HTTP/1.1"
    );
    assert!(request_line(&reqs[16]).starts_with("GET /repos/acme/tenant-a/forks?"));
}

#[tokio::test]
async fn github_collaborator_with_team_or_org_access_fails_closed() {
    let repo = target("acme", "tenant-a");
    let (url, handle) = spawn_mock_http_server(vec![
        // Org admin: direct `pull` set, effective admin remains.
        r(204, ""),
        r(200, r#"{"permission":"admin","role_name":"admin"}"#),
        r(204, ""),
        r(200, r#"{"permission":"admin","role_name":"admin"}"#),
        // Triage through a team is more than read.
        r(204, ""),
        r(200, r#"{"permission":"read","role_name":"triage"}"#),
        r(204, ""),
        r(200, r#"{"permission":"read","role_name":"triage"}"#),
        // Removal: base permission keeps write access.
        r(204, ""),
        r(200, "[]"),
        r(200, r#"{"permission":"write","role_name":"write"}"#),
    ])
    .await;
    let p = GitHubProvider::new(&url, "acme", "infra", "gh-tok").unwrap();
    for expected in ["admin", "triage"] {
        let err = p
            .ensure_read_collaborator(&repo, "alice")
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ForgeError::ElevatedAccess { permission, .. } if permission == expected),
            "{err}"
        );
    }
    assert!(matches!(
        p.remove_collaborator(&repo, "alice").await,
        Err(ForgeError::ElevatedAccess { .. })
    ));
    assert_eq!(handle.await.unwrap().len(), 11);
}

// ----------------------------------------------------------------------------
// GitLab
// ----------------------------------------------------------------------------

#[tokio::test]
async fn gitlab_for_repository_and_named_repository() {
    let (url, handle) = spawn_mock_http_server(vec![
        // scoped MR listing
        r(200, "[]"),
        // existing
        r(
            200,
            r#"{"path_with_namespace":"team/tenants/tenant-a","visibility":"private","default_branch":"main","http_url_to_repo":"https://gl/team/tenants/tenant-a.git","web_url":"https://gl/team/tenants/tenant-a"}"#,
        ),
        // created
        r(404, "{}"),
        r(200, r#"{"id":42,"kind":"group"}"#),
        r(
            201,
            r#"{"path_with_namespace":"team/tenants/tenant-b","visibility":"private","default_branch":"main"}"#,
        ),
        // concurrent create
        r(404, "{}"),
        r(200, r#"{"id":42,"kind":"group"}"#),
        r(400, r#"{"message":{"name":["has already been taken"]}}"#),
        r(200, r#"{"path_with_namespace":"team/tenants/tenant-c","visibility":"private"}"#),
    ])
    .await;
    let p = GitLabProvider::new(&url, "team/infra", "gl-tok").unwrap();
    assert_eq!(
        p.read_credential_kind().unwrap(),
        RepoCredentialKind::HttpsToken
    );
    let scoped = p.for_repository("team/tenants", "tenant-a").unwrap();
    scoped.list_open_pull_requests(None).await.unwrap();
    let existing = p
        .ensure_named_repository("team/tenants", "tenant-a", &RepositoryOptions::default())
        .await
        .unwrap();
    assert_eq!(existing.outcome, RepositoryEnsureOutcome::Existed);
    assert_eq!(existing.owner, "team/tenants");
    assert_eq!(existing.name, "tenant-a");
    assert!(existing.private);
    assert_eq!(
        existing.clone_url.as_deref(),
        Some("https://gl/team/tenants/tenant-a.git")
    );
    let created = p
        .ensure_named_repository("team/tenants", "tenant-b", &RepositoryOptions::default())
        .await
        .unwrap();
    assert_eq!(created.outcome, RepositoryEnsureOutcome::Created);
    let raced = p
        .ensure_named_repository("team/tenants", "tenant-c", &RepositoryOptions::default())
        .await
        .unwrap();
    assert_eq!(raced.outcome, RepositoryEnsureOutcome::Existed);
    assert!(matches!(
        p.for_repository("team", "a/b"),
        Err(ForgeError::InvalidInput(_))
    ));
    let reqs = handle.await.unwrap();
    assert!(
        request_line(&reqs[0])
            .starts_with("GET /api/v4/projects/team%2Ftenants%2Ftenant-a/merge_requests?")
    );
    assert_eq!(
        request_line(&reqs[3]),
        "GET /api/v4/namespaces/team%2Ftenants HTTP/1.1"
    );
    assert_eq!(
        request_body(&reqs[4]),
        json!({"name": "tenant-b", "path": "tenant-b", "namespace_id": 42, "visibility": "private", "initialize_with_readme": true, "default_branch": "main"})
    );
}

#[tokio::test]
async fn gitlab_named_repository_owner_and_adoption_checks() {
    let (url, handle) = spawn_mock_http_server(vec![
        // Another user's personal namespace (admin token): refused before any POST.
        r(404, "{}"),
        r(
            200,
            r#"{"id":7,"kind":"user","full_path":"victim","path":"victim"}"#,
        ),
        r(200, r#"{"id":1,"username":"keel-bot"}"#),
        // The token owner's own namespace is allowed.
        r(404, "{}"),
        r(
            200,
            r#"{"id":8,"kind":"user","full_path":"keel-bot","path":"keel-bot"}"#,
        ),
        r(200, r#"{"id":1,"username":"Keel-Bot"}"#),
        r(
            201,
            r#"{"path_with_namespace":"keel-bot/tenant-a","visibility":"private"}"#,
        ),
        // Unknown namespace kind: refused.
        r(404, "{}"),
        r(200, r#"{"id":9}"#),
        // Existing internal project when private was requested: refused.
        r(
            200,
            r#"{"path_with_namespace":"team/tenant-a","visibility":"internal"}"#,
        ),
        // Existing project answered under another path (rename redirect): refused.
        r(
            200,
            r#"{"path_with_namespace":"team/renamed","visibility":"private"}"#,
        ),
    ])
    .await;
    let p = GitLabProvider::new(&url, "team/infra", "gl-tok").unwrap();
    let opts = RepositoryOptions::default();
    let err = p
        .ensure_named_repository("victim", "tenant-a", &opts)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ForgeError::InvalidInput(m) if m.contains("another user")),
        "{err}"
    );
    let own = p
        .ensure_named_repository("keel-bot", "tenant-a", &opts)
        .await
        .unwrap();
    assert_eq!(own.outcome, RepositoryEnsureOutcome::Created);
    assert!(matches!(
        p.ensure_named_repository("odd", "tenant-a", &opts).await,
        Err(ForgeError::InvalidInput(_))
    ));
    for expected in ["not private", "renamed"] {
        let err = p
            .ensure_named_repository("team", "tenant-a", &opts)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ForgeError::InvalidInput(m) if m.contains(expected)),
            "{err}"
        );
    }
    let reqs = handle.await.unwrap();
    assert_eq!(reqs.len(), 11);
    assert_eq!(request_line(&reqs[2]), "GET /api/v4/user HTTP/1.1");
    assert_eq!(request_line(&reqs[6]), "POST /api/v4/projects HTTP/1.1");
    assert_eq!(request_body(&reqs[6])["namespace_id"], 8);
    // Crafted namespace segments never reach the forge.
    for owner in ["team/%2e%2e", "team/..", "team/.", "%2e%2e"] {
        assert!(matches!(
            p.ensure_named_repository(owner, "r", &opts).await,
            Err(ForgeError::InvalidInput(_))
        ));
        assert!(matches!(
            p.for_repository(owner, "r"),
            Err(ForgeError::InvalidInput(_))
        ));
    }
}

#[tokio::test]
async fn gitlab_deploy_tokens_are_read_repository_only() {
    let repo = target("team/tenants", "tenant-a");
    let (url, handle) = spawn_mock_http_server(vec![
        r(
            201,
            r#"{"id":3,"name":"keel","username":"gitlab+deploy-token-3","token":"glsecret","scopes":["read_repository"]}"#,
        ),
        // Unexpected scopes: revoked, error.
        r(
            201,
            r#"{"id":4,"name":"keel","username":"u","token":"glsecret2","scopes":["read_repository","write_repository"]}"#,
        ),
        r(204, ""),
        // Revoke: 404 ok
        r(404, "{}"),
    ])
    .await;
    let p = GitLabProvider::new(&url, "team/infra", "gl-tok").unwrap();
    let token = p
        .create_repo_read_token(&repo, "keel-tenant-repo-a")
        .await
        .unwrap();
    assert_eq!(token.id, "3");
    assert_eq!(token.username, "gitlab+deploy-token-3");
    assert_eq!(token.token, "glsecret");
    assert!(!format!("{token:?}").contains("glsecret"));
    let err = p
        .create_repo_read_token(&repo, "keel-tenant-repo-a")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("read_repository"), "{err}");
    assert!(!err.to_string().contains("glsecret2"));
    p.revoke_repo_read_token(&repo, "3").await.unwrap();
    for bad in [" ", "keel"] {
        assert!(matches!(
            p.create_repo_read_token(&repo, bad).await,
            Err(ForgeError::InvalidInput(_))
        ));
    }
    let reqs = handle.await.unwrap();
    assert_eq!(
        request_line(&reqs[0]),
        "POST /api/v4/projects/team%2Ftenants%2Ftenant-a/deploy_tokens HTTP/1.1"
    );
    assert_eq!(
        request_body(&reqs[0]),
        json!({"name": "keel-tenant-repo-a", "scopes": ["read_repository"]})
    );
    assert_eq!(
        request_line(&reqs[2]),
        "DELETE /api/v4/projects/team%2Ftenants%2Ftenant-a/deploy_tokens/4 HTTP/1.1"
    );
}

#[tokio::test]
async fn gitlab_deploy_keys_use_can_push() {
    let (pk, fp) = public_key();
    let repo = target("team", "tenant-a");
    let gl_key =
        |id: u64, can_push: bool| json!({"id": id, "title": "k", "key": pk, "can_push": can_push});
    let (url, handle) = spawn_mock_http_server(vec![
        r(200, "[]"),
        r(201, gl_key(5, false).to_string()),
        r(200, json!([gl_key(5, false)]).to_string()),
        r(200, json!([gl_key(5, true)]).to_string()),
        r(204, ""),
        r(201, gl_key(6, false).to_string()),
        r(404, "{}"),
    ])
    .await;
    let p = GitLabProvider::new(&url, "team/infra", "gl-tok").unwrap();
    assert_eq!(
        p.ensure_deploy_key(&repo, "keel", &pk, true)
            .await
            .unwrap()
            .outcome,
        DeployKeyOutcome::Created
    );
    let existed = p.ensure_deploy_key(&repo, "keel", &pk, true).await.unwrap();
    assert_eq!(existed.outcome, DeployKeyOutcome::Existed);
    assert_eq!(existed.key.fingerprint.as_deref(), Some(fp.as_str()));
    assert!(existed.key.read_only);
    assert_eq!(
        p.ensure_deploy_key(&repo, "keel", &pk, true)
            .await
            .unwrap()
            .outcome,
        DeployKeyOutcome::Replaced
    );
    p.revoke_deploy_key(&repo, "6").await.unwrap();
    let reqs = handle.await.unwrap();
    assert_eq!(
        request_line(&reqs[1]),
        "POST /api/v4/projects/team%2Ftenant-a/deploy_keys HTTP/1.1"
    );
    assert_eq!(request_body(&reqs[1])["can_push"], false);
    assert_eq!(
        request_line(&reqs[4]),
        "DELETE /api/v4/projects/team%2Ftenant-a/deploy_keys/5 HTTP/1.1"
    );
}

#[tokio::test]
async fn gitlab_main_protection_deletes_and_recreates_default_rule() {
    let repo = target("team", "tenant-a");
    let keel_rule = json!({
        "name": "main",
        "push_access_levels": [{"access_level": 0}],
        "merge_access_levels": [{"access_level": 40}],
        "allow_force_push": false,
    });
    let branch = r#"{"name":"main"}"#;
    let (url, handle) = spawn_mock_http_server(vec![
        // Default GitLab rule (Maintainers may push) => DELETE + POST
        r(200, branch),
        r(
            200,
            r#"{"name":"main","push_access_levels":[{"access_level":40}],"merge_access_levels":[{"access_level":40}],"allow_force_push":false}"#,
        ),
        r(204, ""),
        r(201, keel_rule.to_string()),
        // Unchanged
        r(200, branch),
        r(200, keel_rule.to_string()),
        // Unprotected => POST
        r(200, branch),
        r(404, "{}"),
        r(201, keel_rule.to_string()),
        // POST failure surfaces
        r(200, branch),
        r(404, "{}"),
        r(403, r#"{"message":"403 Forbidden"}"#),
        // Missing `main` (empty project)
        r(404, r#"{"message":"404 Branch Not Found"}"#),
    ])
    .await;
    let p = GitLabProvider::new(&url, "team/infra", "gl-tok").unwrap();
    for expected in [
        BranchProtectionOutcome::Updated,
        BranchProtectionOutcome::Unchanged,
        BranchProtectionOutcome::Created,
    ] {
        assert_eq!(p.ensure_main_protection(&repo).await.unwrap(), expected);
    }
    assert!(matches!(
        p.ensure_main_protection(&repo).await,
        Err(ForgeError::Api { status: 403, .. })
    ));
    assert!(matches!(
        p.ensure_main_protection(&repo).await,
        Err(ForgeError::InvalidInput(_))
    ));
    let reqs = handle.await.unwrap();
    assert_eq!(reqs.len(), 13);
    assert_eq!(
        request_line(&reqs[0]),
        "GET /api/v4/projects/team%2Ftenant-a/repository/branches/main HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[1]),
        "GET /api/v4/projects/team%2Ftenant-a/protected_branches/main HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[2]),
        "DELETE /api/v4/projects/team%2Ftenant-a/protected_branches/main HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[3]),
        "POST /api/v4/projects/team%2Ftenant-a/protected_branches HTTP/1.1"
    );
    assert_eq!(
        request_body(&reqs[3]),
        json!({"name": "main", "push_access_level": 0, "merge_access_level": 40, "allow_force_push": false})
    );
}

#[tokio::test]
async fn gitlab_forks_and_members() {
    let repo = target("team", "tenant-a");
    let (url, handle) = spawn_mock_http_server(vec![
        // disable_forks: enabled -> PUT
        r(200, r#"{"forking_access_level":"enabled"}"#),
        r(200, r#"{"forking_access_level":"disabled"}"#),
        // already disabled
        r(200, r#"{"forking_access_level":"disabled"}"#),
        // ensure member: Added (then effective level checked)
        r(200, r#"[{"id":11,"username":"alice"}]"#),
        r(404, "{}"),
        r(201, r#"{"id":11,"access_level":20}"#),
        r(200, r#"{"id":11,"access_level":20}"#),
        // Updated (developer -> reporter)
        r(200, r#"[{"id":11,"username":"alice"}]"#),
        r(200, r#"{"id":11,"access_level":30}"#),
        r(200, r#"{"id":11,"access_level":20}"#),
        r(200, r#"{"id":11,"access_level":20}"#),
        // Unchanged
        r(200, r#"[{"id":11,"username":"alice"}]"#),
        r(200, r#"{"id":11,"access_level":20}"#),
        r(200, r#"{"id":11,"access_level":20}"#),
        // Unknown user
        r(200, r#"[{"id":12,"username":"alice2"}]"#),
        // Remove: unknown user is a no-op; known user DELETE (404 ok), no access left
        r(200, "[]"),
        r(200, r#"[{"id":11,"username":"alice"}]"#),
        r(404, "{}"),
        r(404, "{}"),
        // forks
        r(
            200,
            r#"[{"path_with_namespace":"mallory/tenant-a","web_url":"https://gl/mallory/tenant-a"}]"#,
        ),
    ])
    .await;
    let p = GitLabProvider::new(&url, "team/infra", "gl-tok").unwrap();
    assert_eq!(
        p.disable_forks(&repo).await.unwrap(),
        ForkPolicyOutcome::Disabled
    );
    assert_eq!(
        p.disable_forks(&repo).await.unwrap(),
        ForkPolicyOutcome::AlreadyDisabled
    );
    for expected in [
        CollaboratorOutcome::Added,
        CollaboratorOutcome::Updated,
        CollaboratorOutcome::Unchanged,
    ] {
        assert_eq!(
            p.ensure_read_collaborator(&repo, "alice").await.unwrap(),
            expected
        );
    }
    assert!(matches!(
        p.ensure_read_collaborator(&repo, "alice").await,
        Err(ForgeError::NotFound(ref u)) if u == "alice"
    ));
    p.remove_collaborator(&repo, "alice").await.unwrap();
    p.remove_collaborator(&repo, "alice").await.unwrap();
    let forks = p.list_forks(&repo).await.unwrap();
    assert_eq!(forks[0].full_name, "mallory/tenant-a");
    let reqs = handle.await.unwrap();
    assert_eq!(
        request_line(&reqs[1]),
        "PUT /api/v4/projects/team%2Ftenant-a HTTP/1.1"
    );
    assert_eq!(
        request_body(&reqs[1]),
        json!({"forking_access_level": "disabled"})
    );
    assert_eq!(
        request_line(&reqs[3]),
        "GET /api/v4/users?username=alice HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[5]),
        "POST /api/v4/projects/team%2Ftenant-a/members HTTP/1.1"
    );
    assert_eq!(
        request_body(&reqs[5]),
        json!({"user_id": 11, "access_level": 20})
    );
    assert_eq!(
        request_line(&reqs[6]),
        "GET /api/v4/projects/team%2Ftenant-a/members/all/11 HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[9]),
        "PUT /api/v4/projects/team%2Ftenant-a/members/11 HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[17]),
        "DELETE /api/v4/projects/team%2Ftenant-a/members/11 HTTP/1.1"
    );
    assert_eq!(
        request_line(&reqs[18]),
        "GET /api/v4/projects/team%2Ftenant-a/members/all/11 HTTP/1.1"
    );
    assert!(request_line(&reqs[19]).starts_with("GET /api/v4/projects/team%2Ftenant-a/forks?"));
}

#[tokio::test]
async fn gitlab_member_with_inherited_access_fails_closed() {
    let repo = target("team", "tenant-a");
    let alice = r#"[{"id":11,"username":"alice"}]"#;
    let (url, handle) = spawn_mock_http_server(vec![
        // Direct Reporter, but inherited Maintainer: not Unchanged.
        r(200, alice),
        r(200, r#"{"id":11,"access_level":20}"#),
        r(200, r#"{"id":11,"access_level":40}"#),
        // No direct membership; GitLab refuses the lower direct grant with 400.
        r(200, alice),
        r(404, "{}"),
        r(
            400,
            r#"{"message":{"access_level":["should be greater than or equal to Developer inherited membership from group team"]}}"#,
        ),
        r(200, r#"{"id":11,"access_level":30}"#),
        // A 400 for another reason surfaces as is.
        r(200, alice),
        r(404, "{}"),
        r(400, r#"{"message":"bad request"}"#),
        r(200, r#"{"id":11,"access_level":20}"#),
        // Removal leaves inherited access.
        r(200, alice),
        r(204, ""),
        r(200, r#"{"id":11,"access_level":30}"#),
    ])
    .await;
    let p = GitLabProvider::new(&url, "team/infra", "gl-tok").unwrap();
    for expected in ["access_level 40", "access_level 30"] {
        let err = p
            .ensure_read_collaborator(&repo, "alice")
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ForgeError::ElevatedAccess { permission, .. } if permission == expected),
            "{err}"
        );
    }
    assert!(matches!(
        p.ensure_read_collaborator(&repo, "alice").await,
        Err(ForgeError::Api { status: 400, .. })
    ));
    assert!(matches!(
        p.remove_collaborator(&repo, "alice").await,
        Err(ForgeError::ElevatedAccess { .. })
    ));
    let reqs = handle.await.unwrap();
    assert_eq!(reqs.len(), 14);
    assert!(matches!(
        p.ensure_read_collaborator(&repo, "%2e%2e").await,
        Err(ForgeError::InvalidInput(_))
    ));
}
