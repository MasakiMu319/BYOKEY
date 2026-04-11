use anyhow::Result;
use arc_swap::ArcSwap;
use byokey_auth::AuthManager;
use byokey_config::{Config, ConfigWatcher};
use byokey_proxy::AppState;
use std::sync::Arc;

use crate::ServerArgs;

/// Rotate the log file if it exceeds `max_size` bytes.
/// Renames `path` to `path.old` (overwriting any previous `.old`),
/// so at most 1 old copy is kept.
fn rotate_if_needed(path: &std::path::Path, max_size: u64) {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return,
    };
    if meta.len() <= max_size {
        return;
    }
    let mut old = path.to_path_buf();
    let mut name = old
        .file_name()
        .unwrap_or_default()
        .to_os_string();
    name.push(".old");
    old.set_file_name(name);
    // Remove previous .old, rename current, ignore errors.
    let _ = std::fs::remove_file(&old);
    let _ = std::fs::rename(path, &old);
}

pub async fn cmd_serve(args: ServerArgs) -> Result<()> {
    // Install a rustls CryptoProvider so that crates using rustls internally
    // (e.g. gcp_auth for Vertex AI) can create TLS connections.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let ServerArgs {
        config: config_path,
        port,
        host,
        db,
        log_file,
    } = args;
    let effective_path = config_path.or_else(|| {
        let default = byokey_daemon::paths::config_path().ok()?;
        if default.exists() {
            Some(default)
        } else {
            None
        }
    });

    // Load config first so we can use log settings.
    let config_arc: Arc<ArcSwap<Config>> = if let Some(ref path) = effective_path {
        let watcher = Arc::new(
            ConfigWatcher::new(path.clone()).map_err(|e| anyhow::anyhow!("config error: {e}"))?,
        );
        let arc = watcher.arc();
        watcher.watch();
        arc
    } else {
        Arc::new(ArcSwap::from_pointee(Config::default()))
    };

    let snapshot = config_arc.load();

    // ── Log rotation ────────────────────────────────────────────────────
    // Rotate the default daemon log file on startup if it exceeds max_size.
    // In daemon mode stdout/stderr are redirected to this file, so the
    // tracing appender never manages it.
    let max_log_size = snapshot.log.max_size;
    if let Ok(default_log) = byokey_daemon::paths::log_path() {
        rotate_if_needed(&default_log, max_log_size);
    }

    // Initialize structured logging based on config.
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&snapshot.log.level));

    // _log_guard must be held until server exits to flush buffered writes.
    let _log_guard: Option<tracing_appender::non_blocking::WorkerGuard>;

    // CLI --log-file overrides config.
    let effective_log_file = log_file
        .map(|p| p.to_string_lossy().into_owned())
        .or_else(|| snapshot.log.file.clone());

    if let Some(ref log_path) = effective_log_file {
        let path = std::path::Path::new(log_path);
        let dir = path.parent().unwrap_or(std::path::Path::new("."));
        let filename = path
            .file_name()
            .unwrap_or(std::ffi::OsStr::new("byokey.log"));
        let file_appender = tracing_appender::rolling::daily(dir, filename);
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
        _log_guard = Some(guard);

        if snapshot.log.format == "json" {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(env_filter)
                .with_target(true)
                .with_writer(non_blocking)
                .init();
        } else {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_target(true)
                .with_writer(non_blocking)
                .init();
        }
    } else {
        _log_guard = None;
        if snapshot.log.format == "json" {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(env_filter)
                .with_target(true)
                .init();
        } else {
            tracing_subscriber::fmt()
                .with_ansi(true)
                .with_env_filter(env_filter)
                .with_target(true)
                .init();
        }
    }

    // CLI overrides for listen address.
    let addr = format!(
        "{}:{}",
        host.as_deref().unwrap_or(&snapshot.host),
        port.unwrap_or(snapshot.port),
    );

    let store = Arc::new(crate::open_store(db).await?);
    let auth = Arc::new(AuthManager::new(store.clone(), rquest::Client::new()));
    let usage_store: Arc<dyn byokey_types::UsageStore> = store;
    let state = AppState::new(config_arc, auth, Some(usage_store.clone()));

    // Pre-load cumulative usage from persisted records so the in-memory snapshot
    // reflects historical totals even after a restart.
    if let Ok(totals) = usage_store.totals(None, None).await {
        for bucket in &totals {
            state.usage.preload(
                &bucket.model,
                bucket.request_count,
                bucket.input_tokens,
                bucket.output_tokens,
            );
        }
    }
    let app = byokey_proxy::make_router(state);

    // ── Periodic log rotation ───────────────────────────────────────────
    // Check every 10 minutes and rotate if the daemon log exceeds max_size.
    if let Ok(default_log) = byokey_daemon::paths::log_path() {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(600));
            interval.tick().await; // skip immediate first tick
            loop {
                interval.tick().await;
                rotate_if_needed(&default_log, max_log_size);
            }
        });
    }

    // Check for TLS configuration.
    let tls_config = snapshot.tls.as_ref().filter(|t| t.enable);

    if let Some(tls) = tls_config {
        let rustls_config =
            axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert, &tls.key)
                .await
                .map_err(|e| anyhow::anyhow!("TLS config error: {e}"))?;
        let addr: std::net::SocketAddr = addr
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid address: {e}"))?;
        tracing::info!(%addr, "byokey listening (TLS)");
        axum_server::bind_rustls(addr, rustls_config)
            .serve(app.into_make_service())
            .await?;
    } else {
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        tracing::info!(addr = %addr, "byokey listening");
        axum::serve(listener, app).await?;
    }
    Ok(())
}
