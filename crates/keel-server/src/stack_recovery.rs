//! Requests reconciliation of tenant PKO `Stack`s that PKO stalled (`SpecInvalid`) on a Secret
//! which exists again (ADDON-15, [`keel_kube::recover_stalled_stacks`]). Not a controller: a
//! one-off call after keel-server (re)writes a tenant's Secrets, plus a periodic sweep that also
//! covers Secrets restored by `keel bootstrap init` or by hand. Replica-safe: the request value
//! is derived from the Secret's `resourceVersion`, so concurrent passes write the same value.

use std::time::Duration;

use crate::state::AppState;

/// Interval of the periodic stalled-Stack sweep (jittered by up to +25%).
pub const STALLED_STACK_RECOVERY_INTERVAL: Duration = Duration::from_secs(60);

/// Nudges the Stacks of `tenant` stalled on a Secret that now exists. Best effort: errors are
/// logged (the periodic sweep retries). Returns the number of reconcile requests made.
pub async fn recover_tenant_stalled_stacks(state: &AppState, tenant: &str) -> usize {
    let Some(cluster) = state.stack_recovery.as_ref() else {
        return 0;
    };
    let Ok(namespace) = keel_kube::tenant_namespace(tenant) else {
        return 0;
    };
    match keel_kube::recover_stalled_stacks(cluster.as_ref(), Some(&namespace)).await {
        Ok(nudged) => nudged.len(),
        Err(err) => {
            tracing::warn!(tenant, error = %err, "Could not check tenant Stacks stalled on a missing Secret");
            0
        }
    }
}

/// One sweep over every `tenant-<id>` namespace. Returns the number of reconcile requests made.
pub async fn recover_stalled_stacks_once(state: &AppState) -> usize {
    let Some(cluster) = state.stack_recovery.as_ref() else {
        return 0;
    };
    match keel_kube::recover_stalled_stacks(cluster.as_ref(), None).await {
        Ok(nudged) => nudged.len(),
        Err(err) => {
            tracing::debug!(error = %err, "Stalled Stack sweep failed");
            0
        }
    }
}

/// Spawns the periodic stalled-Stack sweep (no-op without a live cluster).
pub fn spawn_stalled_stack_recovery(
    state: AppState,
    interval: Duration,
) -> Option<tokio::task::JoinHandle<()>> {
    state.stack_recovery.as_ref()?;
    Some(tokio::spawn(async move {
        loop {
            tokio::time::sleep(crate::preview_lifecycle::jitter(interval)).await;
            let nudged = recover_stalled_stacks_once(&state).await;
            if nudged > 0 {
                tracing::info!(nudged, "stalled Stack recovery pass");
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use keel_kube::{KubeError, StackRecoveryCluster};
    use serde_json::{Value, json};

    use super::*;

    #[derive(Default)]
    struct Fake {
        secrets: BTreeMap<(String, String), String>,
        listed: Mutex<Vec<Option<String>>>,
        patched: Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait]
    impl StackRecoveryCluster for Fake {
        async fn list_stacks(&self, namespace: Option<&str>) -> Result<Vec<Value>, KubeError> {
            self.listed
                .lock()
                .unwrap()
                .push(namespace.map(str::to_string));
            Ok(["tenant-acme", "tenant-beta"]
                .into_iter()
                .filter(|ns| namespace.is_none_or(|n| n == *ns))
                .map(|ns| {
                    json!({
                        "metadata": {"namespace": ns, "name": "net"},
                        "status": {"conditions": [{
                            "type": "Stalled", "status": "True", "reason": "SpecInvalid",
                            "message": format!("unable to get secret {ns}/keel-gitops-credentials: secrets \"keel-gitops-credentials\" not found")
                        }]}
                    })
                })
                .collect())
        }
        async fn secret_revision(&self, ns: &str, name: &str) -> Result<Option<String>, KubeError> {
            Ok(self
                .secrets
                .get(&(ns.to_string(), name.to_string()))
                .cloned())
        }
        async fn request_stack_reconcile(
            &self,
            ns: &str,
            name: &str,
            request: &str,
        ) -> Result<(), KubeError> {
            self.patched
                .lock()
                .unwrap()
                .push((ns.into(), name.into(), request.into()));
            Ok(())
        }
    }

    fn state(cluster: Option<Arc<Fake>>) -> AppState {
        let dir = std::env::temp_dir().join("keel-stack-recovery-test");
        let state = AppState::new(
            Arc::new(keel_git::LocalGitProvider::new(&dir)),
            Arc::new(keel_kube::InMemorySecretStore::new()),
            keel_kube::TelemetryMux::default(),
            keel_git::GitRunner::new(),
            &dir,
            dir.to_string_lossy().to_string(),
            keel_core::AuthPhase::OidcActive,
        );
        state.with_stack_recovery(cluster.map(|c| c as Arc<dyn StackRecoveryCluster>))
    }

    #[tokio::test]
    async fn tenant_and_sweep_recovery_request_reconciles_through_the_cluster() {
        let fake = Arc::new(Fake {
            secrets: BTreeMap::from([
                (
                    ("tenant-acme".into(), "keel-gitops-credentials".into()),
                    "3".into(),
                ),
                (
                    ("tenant-beta".into(), "keel-gitops-credentials".into()),
                    "4".into(),
                ),
            ]),
            ..Fake::default()
        });
        let s = state(Some(fake.clone()));
        assert_eq!(recover_tenant_stalled_stacks(&s, "acme").await, 1);
        assert_eq!(recover_tenant_stalled_stacks(&s, "Not A Tenant").await, 0);
        assert_eq!(recover_stalled_stacks_once(&s).await, 2);
        assert_eq!(
            *fake.listed.lock().unwrap(),
            [Some("tenant-acme".to_string()), None]
        );
        assert_eq!(
            fake.patched.lock().unwrap()[0],
            (
                "tenant-acme".to_string(),
                "net".to_string(),
                "keel-secret-restored:keel-gitops-credentials@3".to_string()
            )
        );
    }

    #[tokio::test]
    async fn without_a_cluster_recovery_is_a_no_op() {
        let s = state(None);
        assert_eq!(recover_tenant_stalled_stacks(&s, "acme").await, 0);
        assert_eq!(recover_stalled_stacks_once(&s).await, 0);
        assert!(spawn_stalled_stack_recovery(s, Duration::from_secs(60)).is_none());
    }
}
