mod auth;
mod config;
mod index;
#[cfg(test)]
mod integration_tests;
mod server;
mod terminal;

use anyhow::{Context, Result, bail};
use clap::Parser;
use config::{Cli, Config};
use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rwebman=info".into()),
        )
        .init();
    let dir = config::data_dir(&cli)?;
    let _lock = config::acquire_lock(&dir)?;
    let mut existing = config::load(&dir)?;
    if existing.is_none() || cli.set_password {
        println!(
            "Set the password used to unlock file search and the local shell (at least 12 characters)."
        );
        let password = match std::env::var("RWEBMAN_PASSWORD") {
            Ok(password) => password,
            Err(_) => {
                let password = rpassword::prompt_password("New password: ")?;
                if password != rpassword::prompt_password("Confirm password: ")? {
                    bail!("Passwords do not match");
                }
                password
            }
        };
        let hash = auth::hash_password(&password)?;
        let mut cfg = existing.unwrap_or_else(|| Config {
            password_hash: String::new(),
            roots: config::default_roots(),
            excludes: config::default_excludes(),
            index_interval_minutes: 60,
        });
        cfg.password_hash = hash;
        config::save(&dir, &cfg)?;
        existing = Some(cfg);
        if cli.set_password {
            println!("Password updated.");
            return Ok(());
        }
    }
    // Single-threaded initialization: do not leak the setup password into shells.
    unsafe {
        std::env::remove_var("RWEBMAN_PASSWORD");
    }
    let mut cfg = existing.context("Missing configuration")?;
    if !cli.roots.is_empty() {
        cfg.roots = config::validate_roots(&cli.roots)?;
    }
    if let Some(minutes) = cli.index_interval {
        if minutes > 43_200 {
            bail!("Index interval cannot exceed 43200 minutes");
        }
        cfg.index_interval_minutes = minutes;
    }
    if cfg.roots.is_empty() {
        bail!("No usable default index directories; specify --root");
    }
    argon2::PasswordHash::new(&cfg.password_hash)
        .map_err(|e| anyhow::anyhow!("Invalid password hash in config.json: {e}"))?;
    let security = server::Security::new(&cli)?;
    config::save(&dir, &cfg)?;
    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(serve(cli, dir, cfg, security));
    runtime.shutdown_timeout(Duration::from_secs(5));
    result
}

async fn serve(
    cli: Cli,
    dir: std::path::PathBuf,
    cfg: Config,
    security: server::Security,
) -> Result<()> {
    let index = index::Index::open(dir.join("index.sqlite3"))?;
    let app = Arc::new(server::App {
        config: Mutex::new(cfg),
        data_dir: dir,
        index,
        auth: auth::Auth::default(),
        security,
        shell: config::shell(&cli),
        auto_index: !cli.no_auto_index,
        terminals: Arc::new(Semaphore::new(4)),
        searches: Arc::new(Semaphore::new(4)),
        logins: Arc::new(Semaphore::new(2)),
    });
    let handle = axum_server::Handle::new();
    let shutdown_app = app.clone();
    let shutdown_handle = handle.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("Shutting down");
        shutdown_app.auth.cancel_all();
        shutdown_app.index.cancelled.store(true, Ordering::Relaxed);
        shutdown_handle.graceful_shutdown(Some(Duration::from_secs(3)));
    });
    let listener = std::net::TcpListener::bind(cli.bind).context("Cannot bind TCP listener")?;
    listener.set_nonblocking(true)?;
    let router =
        server::router(app.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    let tls = match (cli.tls_cert, cli.tls_key) {
        (Some(cert), Some(key)) => {
            let _ = rustls::crypto::ring::default_provider().install_default();
            Some(
                axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
                    .await
                    .context("Cannot load TLS certificate/key")?,
            )
        }
        _ => None,
    };
    if app.auto_index {
        let schedule = app.clone();
        tokio::spawn(async move {
            let mut last_start = Instant::now();
            {
                let cfg = schedule.config.lock().unwrap();
                schedule.index.start(cfg.clone(), schedule.data_dir.clone());
            }
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                let cfg = schedule.config.lock().unwrap();
                let interval = cfg.index_interval_minutes;
                if interval > 0
                    && last_start.elapsed() >= Duration::from_secs(interval * 60)
                    && schedule.index.start(cfg.clone(), schedule.data_dir.clone())
                {
                    last_start = Instant::now();
                }
            }
        });
    }
    let address = cli.public_url.unwrap_or_else(|| {
        format!(
            "{}://{}",
            if tls.is_some() { "https" } else { "http" },
            cli.bind
        )
    });
    println!("\nrwebman {}\nOpen: {address}", env!("CARGO_PKG_VERSION"));
    println!(
        "Data: {}\nShell: {}\nPress Ctrl+C to stop.\n",
        config::display_path(&app.data_dir),
        config::display_path(&app.shell)
    );
    if let Some(tls) = tls {
        axum_server::from_tcp_rustls(listener, tls)
            .handle(handle)
            .serve(router)
            .await?;
    } else {
        axum_server::from_tcp(listener)
            .handle(handle)
            .serve(router)
            .await?;
    }
    app.auth.cancel_all();
    app.index.cancelled.store(true, Ordering::Relaxed);
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
