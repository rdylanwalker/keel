//! Termination signals shared by long-running commands (`keel serve`, `keel gitops pull`), so
//! they return through their cleanup (dropping `kubectl port-forward` guards, removing the
//! working copy's temporary git credential) instead of dying on the default signal action.

/// Resolves on the first `SIGINT` (`Ctrl+C`), `SIGTERM` or `SIGHUP` (only `Ctrl+C` off Unix).
/// The Unix handlers are registered when this is called (not when the future is first polled),
/// so the default "terminate" disposition is replaced from then on and a signal that arrives
/// before the future is polled is still observed.
pub(crate) fn shutdown_signal() -> impl Future<Output = ()> + Send + 'static {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let streams = (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        );
        async move {
            let (Ok(mut int), Ok(mut term), Ok(mut hup)) = streams else {
                let _ = tokio::signal::ctrl_c().await;
                return;
            };
            tokio::select! {
                _ = int.recv() => tracing::info!("Received SIGINT; shutting down"),
                _ = term.recv() => tracing::info!("Received SIGTERM; shutting down"),
                _ = hup.recv() => tracing::info!("Received SIGHUP; shutting down"),
            }
        }
    }
    #[cfg(not(unix))]
    {
        async move {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}
