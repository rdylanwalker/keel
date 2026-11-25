//! `keel gitops pull`: fast-forwards the local GitOps working copy (`./keel-gitops`) from its
//! origin, with or without a running `keel serve` (`SDD.md` §7.2).
//!
//! With `--with-forge` the working copy's `origin` is the loopback port-forward to the in-cluster
//! Forgejo that `keel serve` opens, and the Forgejo token reaches `git` only through the
//! repository-local credential store `keel serve` writes for the lifetime of that forward
//! (`GitRunner::configure_remote_credential_store`), while it holds the [`ServeLock`]. Without a
//! live `keel serve` holding that lock, this command opens its own temporary forward, writes the
//! same store for the duration of one `git pull --ff-only`, and afterwards removes the store,
//! restores `origin` and closes the forward again, also when the pull fails or the command is
//! interrupted (`SIGINT`/`SIGTERM`/`SIGHUP`). The token is never printed, logged, or put in the
//! remote URL.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use keel_core::{ForgeKind, KEEL_SYSTEM_NAMESPACE, KeelConfig};
use keel_git::{GitRunner, REMOTE_CREDENTIAL_STORE_FILE, parse_remote_repo_url};
use zeroize::Zeroizing;

use crate::cli::GitopsPullArgs;

use super::bootstrap::kube::KubeTarget;
use super::bootstrap::{FORGE_TOKEN_SECRET_NAME, TempPortForward, start_forgejo_port_forward};
use super::{CliError, absolutize_path};

/// HTTPS username paired with the Forgejo token in the credential store (as `keel serve` does).
const FORGE_CREDENTIAL_USERNAME: &str = "keel";

/// Lock file (inside the working copy's `.git`) a running `keel serve` holds; see [`ServeLock`].
pub(crate) const SERVE_LOCK_FILE: &str = "keel-serve.lock";

fn serve_lock_path(repo: &Path) -> PathBuf {
    repo.join(".git").join(SERVE_LOCK_FILE)
}

/// An exclusive advisory lock (`flock`) on `.git/keel-serve.lock` that `keel serve` holds while
/// the working copy's `origin` and credential store point at its Forgejo forward. The kernel
/// releases it when the process ends, also on `kill -9`, and it is not inherited by children
/// (Rust opens files close-on-exec), so an orphaned `kubectl port-forward` still answering on the
/// old port cannot make a killed `keel serve` look alive.
pub(crate) struct ServeLock {
    _file: File,
}

impl ServeLock {
    /// Takes the lock for the working copy at `repo`; fails if another process holds it.
    pub(crate) fn acquire(repo: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(serve_lock_path(repo))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another `keel serve` holds the working copy's lock",
            )),
            Err(TryLockError::Error(err)) => Err(err),
        }
    }

    /// Whether a live process (a running `keel serve`) holds the lock for `repo`.
    pub(crate) fn is_held(repo: &Path) -> bool {
        let Ok(file) = File::open(serve_lock_path(repo)) else {
            return false;
        };
        matches!(file.try_lock_shared(), Err(TryLockError::WouldBlock))
    }
}

/// Whether `cfg` points at the Stage 0 in-cluster Forgejo (reachable from a laptop only through
/// a port-forward), as opposed to a local bare repository or an external forge.
pub(crate) fn is_in_cluster_forge(cfg: &KeelConfig) -> bool {
    cfg.forge.provider != ForgeKind::Local
        && cfg
            .forge
            .api_url
            .as_deref()
            .is_some_and(|u| u.contains(".svc.cluster.local"))
}

/// Side effects of [`pull_working_copy`] that need a cluster (or, for `pull`, a forge), so the
/// orchestration can be tested with fakes.
pub(crate) trait PullEnv {
    /// Keeps a port-forward open until dropped.
    type Forward;

    /// The platform Forgejo token (`keel-system/forge-token`).
    async fn forge_token(&self) -> Result<Zeroizing<String>, CliError>;

    /// Opens a port-forward to the in-cluster Forgejo; returns its local port and its guard.
    async fn start_forward(&self) -> Result<(u16, Self::Forward), CliError>;

    /// Runs `git pull --ff-only` in `repo`. Dropping the future stops the pull.
    async fn pull(&self, git: &GitRunner, repo: &Path) -> Result<String, CliError>;
}

/// How [`pull_working_copy`] reached the origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PullRoute {
    /// The origin is not the in-cluster Forgejo (local bare repository, external forge): plain
    /// `git pull` with the user's own credentials.
    Direct,
    /// Through the port-forward and credential store of a running `keel serve`.
    ServeForward { port: u16 },
    /// Through a temporary port-forward and credential store, both gone again.
    TemporaryForward { port: u16 },
}

/// Result of [`pull_working_copy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullOutcome {
    pub(crate) route: PullRoute,
    /// `git pull` output (never contains the token: it only reaches git through the store).
    pub(crate) output: String,
}

/// The loopback `http://127.0.0.1:<port>/...` port of `origin`, if it is one.
fn loopback_http_port(origin: &str) -> Option<u16> {
    let url = url::Url::parse(origin).ok()?;
    (url.scheme() == "http" && url.host_str() == Some("127.0.0.1"))
        .then(|| url.port())
        .flatten()
}

/// Runs `work` unless `interrupt` resolves first, which drops (and so stops) `work`.
async fn until_interrupted<T, I: Future<Output = ()>>(
    work: impl Future<Output = Result<T, CliError>>,
    interrupt: Pin<&mut I>,
) -> Result<T, CliError> {
    tokio::select! {
        res = work => res,
        () = interrupt => Err(CliError::Execution("interrupted".to_string())),
    }
}

/// Fast-forwards the working copy at `repo` (see the module docs). `cfg` is its
/// `.keel/config.yaml`, if any. When `interrupt` resolves, the pull is stopped and, on the
/// temporary route, the credential store, `origin` and the forward are still cleaned up.
pub(crate) async fn pull_working_copy<E: PullEnv>(
    env: &E,
    git: &GitRunner,
    repo: &Path,
    cfg: Option<&KeelConfig>,
    interrupt: impl Future<Output = ()>,
) -> Result<PullOutcome, CliError> {
    let mut interrupt = std::pin::pin!(interrupt);
    let Some(cfg) = cfg.filter(|c| is_in_cluster_forge(c)) else {
        let output = until_interrupted(env.pull(git, repo), interrupt.as_mut()).await?;
        return Ok(PullOutcome {
            route: PullRoute::Direct,
            output,
        });
    };
    let slug = parse_remote_repo_url(&cfg.forge.repo_url)?;
    let origin = git.get_remote_url(repo, "origin").await?;

    // `keel serve` writes the store while holding the `ServeLock` and removes it on shutdown; a
    // store without a held lock is the leftover of a killed server (whose orphaned `kubectl
    // port-forward` may still answer on the port), so it is replaced and removed below.
    let store = repo.join(".git").join(REMOTE_CREDENTIAL_STORE_FILE);
    if let Some(port) = loopback_http_port(&origin)
        && tokio::fs::try_exists(&store).await.unwrap_or(false)
        && ServeLock::is_held(repo)
    {
        let output = until_interrupted(env.pull(git, repo), interrupt.as_mut()).await?;
        return Ok(PullOutcome {
            route: PullRoute::ServeForward { port },
            output,
        });
    }

    let (token, (port, forward)) = until_interrupted(
        async { Ok((env.forge_token().await?, env.start_forward().await?)) },
        interrupt.as_mut(),
    )
    .await?;
    let remote = format!("http://127.0.0.1:{port}/{}/{}.git", slug.owner, slug.repo);
    let pulled = until_interrupted(
        async {
            git.set_remote_url(repo, "origin", &remote).await?;
            git.configure_remote_credential_store(
                repo,
                "origin",
                FORGE_CREDENTIAL_USERNAME,
                &token,
            )
            .await?;
            env.pull(git, repo).await
        },
        interrupt.as_mut(),
    )
    .await;
    // Remove the token before the forward it is scoped to closes, whatever happened above, and
    // point `origin` back where it was instead of at the port about to close.
    let removed = git.remove_remote_credential_store(repo).await;
    let restored = git.set_remote_url(repo, "origin", &origin).await;
    drop(forward);
    let pull = match &pulled {
        Ok(_) => "pulled".to_string(),
        Err(pull_err) => format!("the pull failed ({pull_err})"),
    };
    if let Err(err) = removed {
        return Err(CliError::Execution(format!(
            "{pull}, but could not remove the temporary git credential store: {err}; delete {} \
             by hand",
            store.display()
        )));
    }
    if let Err(err) = restored {
        return Err(CliError::Execution(format!(
            "{pull}, but could not point origin back at its previous URL: {err}; run `git \
             remote set-url origin <url>` in {}",
            repo.display()
        )));
    }
    let output = pulled?;
    Ok(PullOutcome {
        route: PullRoute::TemporaryForward { port },
        output,
    })
}

/// [`PullEnv`] against the management cluster through `kubectl`.
struct KubePullEnv {
    context: Option<String>,
    kubeconfig: Option<PathBuf>,
}

impl KubePullEnv {
    fn target(&self) -> KubeTarget<'_> {
        KubeTarget::new(self.context.as_deref(), self.kubeconfig.as_deref())
    }
}

impl PullEnv for KubePullEnv {
    type Forward = TempPortForward;

    async fn forge_token(&self) -> Result<Zeroizing<String>, CliError> {
        self.target()
            .read_secret_key(KEEL_SYSTEM_NAMESPACE, FORGE_TOKEN_SECRET_NAME, "token")
            .await
            .map(Zeroizing::new)
            .ok_or_else(|| {
                CliError::Execution(format!(
                    "could not read {KEEL_SYSTEM_NAMESPACE}/{FORGE_TOKEN_SECRET_NAME} from the \
                     management cluster; check that kubectl reaches it (--context/--kubeconfig) \
                     with cluster-admin rights"
                ))
            })
    }

    async fn start_forward(&self) -> Result<(u16, TempPortForward), CliError> {
        let pf =
            start_forgejo_port_forward(self.context.as_deref(), self.kubeconfig.as_deref()).await?;
        Ok((pf.local_port, pf))
    }

    async fn pull(&self, git: &GitRunner, repo: &Path) -> Result<String, CliError> {
        Ok(git.pull_ff_only(repo).await?)
    }
}

/// Formats [`PullOutcome`] for stdout.
fn render_outcome(outcome: &PullOutcome) -> String {
    let route = match outcome.route {
        PullRoute::Direct => "Pulled from origin".to_string(),
        PullRoute::ServeForward { port } => {
            format!("Pulled through the running `keel serve` (Forgejo on 127.0.0.1:{port})")
        }
        PullRoute::TemporaryForward { port } => format!(
            "Pulled through a temporary port-forward to the in-cluster Forgejo (127.0.0.1:{port}); \
             the forward is closed, the temporary credential removed and origin restored"
        ),
    };
    let output = outcome.output.trim();
    if output.is_empty() {
        route
    } else {
        format!("{output}\n{route}")
    }
}

/// `keel gitops pull`.
pub async fn execute_gitops_pull(args: &GitopsPullArgs) -> Result<String, CliError> {
    let repo = absolutize_path(&args.repo_dir)?;
    if !repo.join(".git").exists() {
        return Err(CliError::Validation(format!(
            "{} is not a git working copy (pass the `--repo-dir` of `keel bootstrap init`)",
            repo.display()
        )));
    }
    let config_path = repo.join(".keel/config.yaml");
    let cfg = if tokio::fs::try_exists(&config_path).await.unwrap_or(false) {
        Some(KeelConfig::from_yaml(
            &tokio::fs::read_to_string(&config_path).await?,
        )?)
    } else {
        None
    };
    let env = KubePullEnv {
        context: args.context.clone(),
        kubeconfig: args.kubeconfig.clone(),
    };
    // Registered before the pull starts: from here on a signal ends in the cleanup above
    // instead of the default "terminate" action.
    let interrupt = super::signal::shutdown_signal();
    let outcome =
        pull_working_copy(&env, &GitRunner::new(), &repo, cfg.as_ref(), interrupt).await?;
    Ok(render_outcome(&outcome))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use keel_git::CommitAuthor;
    use tempfile::TempDir;

    use super::*;

    const TOKEN: &str = "s3cr3t-forge-token";

    /// Sets its flag when dropped (the fake forward closing).
    struct FakeForward(Arc<AtomicBool>);

    impl Drop for FakeForward {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[derive(Default)]
    struct FakeEnv {
        token_missing: bool,
        pull_fails: bool,
        /// The pull never finishes (a hung forge), so only an interrupt ends it.
        pull_hangs: bool,
        forward_port: u16,
        forward_closed: Arc<AtomicBool>,
        forwards_started: Mutex<u32>,
        /// Credential store contents and forward state seen while pulling.
        seen_at_pull: Mutex<Option<(Option<String>, bool, String)>>,
    }

    impl PullEnv for FakeEnv {
        type Forward = FakeForward;

        async fn forge_token(&self) -> Result<Zeroizing<String>, CliError> {
            if self.token_missing {
                return Err(CliError::Execution("no token".to_string()));
            }
            Ok(Zeroizing::new(TOKEN.to_string()))
        }

        async fn start_forward(&self) -> Result<(u16, FakeForward), CliError> {
            *self.forwards_started.lock().unwrap() += 1;
            Ok((
                self.forward_port,
                FakeForward(Arc::clone(&self.forward_closed)),
            ))
        }

        async fn pull(&self, git: &GitRunner, repo: &Path) -> Result<String, CliError> {
            let store =
                std::fs::read_to_string(repo.join(".git").join(REMOTE_CREDENTIAL_STORE_FILE)).ok();
            let origin = git.get_remote_url(repo, "origin").await?;
            *self.seen_at_pull.lock().unwrap() =
                Some((store, self.forward_closed.load(Ordering::SeqCst), origin));
            if self.pull_hangs {
                std::future::pending::<()>().await;
            }
            if self.pull_fails {
                return Err(CliError::Execution(
                    "fatal: Not possible to fast-forward".into(),
                ));
            }
            Ok("Already up to date.\n".to_string())
        }
    }

    fn forge_cfg() -> KeelConfig {
        let mut cfg = KeelConfig::default();
        cfg.forge.provider = ForgeKind::Forgejo;
        cfg.forge.repo_url =
            "http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git"
                .to_string();
        cfg.forge.api_url =
            Some("http://forgejo-http.keel-system.svc.cluster.local:3000".to_string());
        cfg
    }

    /// `git init -b main` at `repo` with `origin` set to `origin`.
    fn init_repo(repo: &Path, origin: &str) {
        std::fs::create_dir_all(repo).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["remote", "add", "origin", origin][..],
        ] {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .status()
                .unwrap();
            assert!(status.success());
        }
    }

    async fn working_copy(tmp: &TempDir, origin: &str) -> PathBuf {
        let repo = tmp.path().join("keel-gitops");
        init_repo(&repo, origin);
        repo
    }

    /// An interrupt that never comes.
    fn never() -> std::future::Pending<()> {
        std::future::pending()
    }

    async fn origin_of(repo: &Path) -> String {
        GitRunner::new()
            .get_remote_url(repo, "origin")
            .await
            .unwrap()
    }

    fn store_path(repo: &Path) -> PathBuf {
        repo.join(".git").join(REMOTE_CREDENTIAL_STORE_FILE)
    }

    async fn helper_keys(repo: &Path) -> String {
        let out = tokio::process::Command::new("git")
            .args(["config", "--local", "--get-regexp", "^credential\\."])
            .current_dir(repo)
            .output()
            .await
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    /// Without `keel serve`, the pull runs through a temporary forward with the token in the
    /// store only for its duration; afterwards the store, the helper and the forward are gone.
    #[tokio::test]
    async fn pulls_through_a_temporary_forward_and_cleans_up() {
        let tmp = TempDir::new().unwrap();
        let repo = working_copy(&tmp, "http://127.0.0.1:3000/keel/infra-gitops.git").await;
        let env = FakeEnv {
            forward_port: 41234,
            ..FakeEnv::default()
        };
        let outcome =
            pull_working_copy(&env, &GitRunner::new(), &repo, Some(&forge_cfg()), never())
                .await
                .unwrap();
        assert_eq!(outcome.route, PullRoute::TemporaryForward { port: 41234 });
        let (store, closed, origin) = env.seen_at_pull.lock().unwrap().clone().unwrap();
        assert_eq!(
            store.as_deref(),
            Some(format!("http://keel:{TOKEN}@127.0.0.1:41234\n").as_str())
        );
        assert!(!closed, "the forward must stay open while pulling");
        assert_eq!(origin, "http://127.0.0.1:41234/keel/infra-gitops.git");
        assert!(env.forward_closed.load(Ordering::SeqCst));
        assert!(!store_path(&repo).exists());
        assert_eq!(helper_keys(&repo).await, "");
        assert_eq!(
            origin_of(&repo).await,
            "http://127.0.0.1:3000/keel/infra-gitops.git",
            "origin must not keep pointing at the closed temporary forward"
        );
        let rendered = render_outcome(&outcome);
        assert!(rendered.contains("temporary port-forward"), "{rendered}");
        assert!(!rendered.contains(TOKEN));
    }

    /// A failed pull still removes the credential store (also a stale one left by a killed
    /// `keel serve`) and closes the forward.
    #[tokio::test]
    async fn a_failed_pull_still_removes_the_credential_and_closes_the_forward() {
        let tmp = TempDir::new().unwrap();
        let repo = working_copy(&tmp, "http://127.0.0.1:3000/keel/infra-gitops.git").await;
        std::fs::write(store_path(&repo), "http://keel:stale@127.0.0.1:3000\n").unwrap();
        let env = FakeEnv {
            pull_fails: true,
            forward_port: 3000,
            ..FakeEnv::default()
        };
        let err = pull_working_copy(&env, &GitRunner::new(), &repo, Some(&forge_cfg()), never())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("fast-forward"), "{err}");
        assert!(env.seen_at_pull.lock().unwrap().is_some());
        assert!(env.forward_closed.load(Ordering::SeqCst));
        assert!(!store_path(&repo).exists());
        assert_eq!(helper_keys(&repo).await, "");
    }

    /// Without the token no forward is opened and nothing is written.
    #[tokio::test]
    async fn a_missing_token_opens_no_forward() {
        let tmp = TempDir::new().unwrap();
        let repo = working_copy(&tmp, "http://127.0.0.1:3000/keel/infra-gitops.git").await;
        let env = FakeEnv {
            token_missing: true,
            ..FakeEnv::default()
        };
        pull_working_copy(&env, &GitRunner::new(), &repo, Some(&forge_cfg()), never())
            .await
            .unwrap_err();
        assert_eq!(*env.forwards_started.lock().unwrap(), 0);
        assert!(env.seen_at_pull.lock().unwrap().is_none());
        assert!(!store_path(&repo).exists());
    }

    /// Ctrl-C/SIGTERM/SIGHUP during a hung pull stops it and still removes the store and the
    /// helper, restores origin and closes the forward.
    #[tokio::test]
    async fn an_interrupted_pull_still_cleans_up() {
        let tmp = TempDir::new().unwrap();
        let repo = working_copy(&tmp, "http://127.0.0.1:3000/keel/infra-gitops.git").await;
        let env = FakeEnv {
            pull_hangs: true,
            forward_port: 41235,
            ..FakeEnv::default()
        };
        // The fake signal arrives once the pull is under way (store written, forward open).
        let interrupt = async {
            while env.seen_at_pull.lock().unwrap().is_none() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let err = tokio::time::timeout(
            Duration::from_secs(30),
            pull_working_copy(
                &env,
                &GitRunner::new(),
                &repo,
                Some(&forge_cfg()),
                interrupt,
            ),
        )
        .await
        .expect("the interrupt must end the hung pull")
        .unwrap_err();
        assert!(err.to_string().contains("interrupted"), "{err}");
        assert!(!err.to_string().contains(TOKEN));
        let (store, closed, _) = env.seen_at_pull.lock().unwrap().clone().unwrap();
        assert!(store.unwrap().contains(TOKEN));
        assert!(!closed);
        assert!(env.forward_closed.load(Ordering::SeqCst));
        assert!(!store_path(&repo).exists());
        assert_eq!(helper_keys(&repo).await, "");
        assert_eq!(
            origin_of(&repo).await,
            "http://127.0.0.1:3000/keel/infra-gitops.git"
        );
    }

    /// An interrupt while the forward is still starting leaves nothing behind.
    #[tokio::test]
    async fn an_interrupt_before_the_pull_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let repo = working_copy(&tmp, "http://127.0.0.1:3000/keel/infra-gitops.git").await;
        let env = FakeEnv::default();
        let err = pull_working_copy(
            &env,
            &GitRunner::new(),
            &repo,
            Some(&forge_cfg()),
            std::future::ready(()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("interrupted"), "{err}");
        assert!(env.seen_at_pull.lock().unwrap().is_none());
        assert!(!store_path(&repo).exists());
    }

    /// A running `keel serve` (its store present and its lock held) is used as is: no forward,
    /// the store stays.
    #[tokio::test]
    async fn uses_the_running_keel_serve_forward() {
        let tmp = TempDir::new().unwrap();
        let repo = working_copy(&tmp, "http://127.0.0.1:3000/keel/infra-gitops.git").await;
        let git = GitRunner::new();
        let _serve = ServeLock::acquire(&repo).unwrap();
        git.configure_remote_credential_store(&repo, "origin", "keel", TOKEN)
            .await
            .unwrap();
        let env = FakeEnv::default();
        let outcome = pull_working_copy(&env, &git, &repo, Some(&forge_cfg()), never())
            .await
            .unwrap();
        assert_eq!(outcome.route, PullRoute::ServeForward { port: 3000 });
        assert_eq!(*env.forwards_started.lock().unwrap(), 0);
        assert!(store_path(&repo).exists());
    }

    /// A store without a live `keel serve` holding the lock (a server killed with `kill -9`,
    /// whose orphaned `kubectl port-forward` may still serve Forgejo on the port) takes the
    /// temporary route: the stale store is replaced and removed.
    #[tokio::test]
    async fn a_stale_store_without_a_live_keel_serve_takes_the_temporary_route() {
        let tmp = TempDir::new().unwrap();
        let repo = working_copy(&tmp, "http://127.0.0.1:3000/keel/infra-gitops.git").await;
        let git = GitRunner::new();
        // The killed server's lock file stays behind, but nothing holds the lock any more.
        drop(ServeLock::acquire(&repo).unwrap());
        git.configure_remote_credential_store(&repo, "origin", "keel", "stale")
            .await
            .unwrap();
        let env = FakeEnv {
            forward_port: 3001,
            ..FakeEnv::default()
        };
        let outcome = pull_working_copy(&env, &git, &repo, Some(&forge_cfg()), never())
            .await
            .unwrap();
        assert_eq!(outcome.route, PullRoute::TemporaryForward { port: 3001 });
        let (store, _, _) = env.seen_at_pull.lock().unwrap().clone().unwrap();
        assert!(store.unwrap().contains(TOKEN));
        assert!(!store_path(&repo).exists());
        assert_eq!(helper_keys(&repo).await, "");
    }

    #[test]
    fn serve_lock_is_exclusive_and_released_on_drop() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("keel-gitops");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert!(!ServeLock::is_held(&repo), "no lock file yet");
        let lock = ServeLock::acquire(&repo).unwrap();
        assert!(ServeLock::is_held(&repo));
        let err = ServeLock::acquire(&repo).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        // Probing must not take the lock away from its holder.
        assert!(ServeLock::is_held(&repo));
        drop(lock);
        // `flock` locks belong to the open file description: a child that another test thread
        // forks while the lock is held keeps a copy until it `exec`s (O_CLOEXEC), so the release
        // can lag the drop by a moment. Poll instead of asserting instantly.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while ServeLock::is_held(&repo) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!ServeLock::is_held(&repo), "lock released after drop");
        assert!(repo.join(".git").join(SERVE_LOCK_FILE).exists());
    }

    /// Local bare repositories and external forges pull directly, without a cluster.
    #[tokio::test]
    async fn other_origins_pull_directly() {
        let tmp = TempDir::new().unwrap();
        let git = GitRunner::new();
        let bare = tmp.path().join("keel-gitops.git");
        git.init_bare(&bare).await.unwrap();
        let repo = working_copy(&tmp, bare.to_str().unwrap()).await;
        let env = FakeEnv::default();
        let outcome = pull_working_copy(&env, &git, &repo, None, never())
            .await
            .unwrap();
        assert_eq!(outcome.route, PullRoute::Direct);
        let mut external = forge_cfg();
        external.forge.api_url = Some("https://api.github.com".to_string());
        external.forge.provider = ForgeKind::GitHub;
        let outcome = pull_working_copy(&env, &git, &repo, Some(&external), never())
            .await
            .unwrap();
        assert_eq!(outcome.route, PullRoute::Direct);
        assert_eq!(*env.forwards_started.lock().unwrap(), 0);
        assert_eq!(
            render_outcome(&outcome),
            "Already up to date.\nPulled from origin"
        );
    }

    /// The real environment's pull fast-forwards the checked-out branch.
    #[tokio::test]
    async fn kube_env_pull_fast_forwards() {
        let tmp = TempDir::new().unwrap();
        let git = GitRunner::new();
        let bare = tmp.path().join("origin.git");
        git.init_bare(&bare).await.unwrap();
        let upstream = tmp.path().join("upstream");
        init_repo(&upstream, bare.to_str().unwrap());
        std::fs::write(upstream.join("a.txt"), "a\n").unwrap();
        git.add(&upstream, ["."]).await.unwrap();
        git.commit(
            &upstream,
            "chore: a",
            &CommitAuthor::new("Dev", "dev@keel.io"),
        )
        .await
        .unwrap();
        git.push(&upstream, "origin", "HEAD:refs/heads/main")
            .await
            .unwrap();
        let repo = tmp.path().join("keel-gitops");
        git.clone(bare.to_str().unwrap(), &repo).await.unwrap();
        std::fs::write(upstream.join("b.txt"), "b\n").unwrap();
        git.add(&upstream, ["."]).await.unwrap();
        git.commit(
            &upstream,
            "chore: b",
            &CommitAuthor::new("Dev", "dev@keel.io"),
        )
        .await
        .unwrap();
        git.push(&upstream, "origin", "HEAD:refs/heads/main")
            .await
            .unwrap();
        let env = KubePullEnv {
            context: None,
            kubeconfig: Some(PathBuf::from("/nonexistent")),
        };
        pull_working_copy(&env, &git, &repo, None, never())
            .await
            .unwrap();
        assert!(repo.join("b.txt").exists());
    }

    #[test]
    fn loopback_port_only_for_plain_http_loopback() {
        assert_eq!(
            loopback_http_port("http://127.0.0.1:3000/keel/infra-gitops.git"),
            Some(3000)
        );
        assert_eq!(loopback_http_port("https://127.0.0.1:3000/x.git"), None);
        assert_eq!(loopback_http_port("http://forge.example/x.git"), None);
        assert_eq!(loopback_http_port("/tmp/keel-gitops.git"), None);
    }

    #[tokio::test]
    async fn execute_rejects_a_directory_that_is_not_a_working_copy() {
        let tmp = TempDir::new().unwrap();
        let args = GitopsPullArgs {
            repo_dir: tmp.path().to_path_buf(),
            context: None,
            kubeconfig: None,
        };
        let err = execute_gitops_pull(&args).await.unwrap_err();
        assert!(err.to_string().contains("not a git working copy"), "{err}");
    }
}
