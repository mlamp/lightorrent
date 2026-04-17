use std::future::IntoFuture;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use lightorrent::{api, config, engine};
use tracing::{debug, error, info};

#[derive(Parser)]
#[command(
    name = "lightorrent",
    about = "Lightweight torrent daemon",
    version = lightorrent::version_string(),
)]
struct Cli {
    /// Path to TOML config file (required when running the daemon)
    #[arg(short, long)]
    config: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Hash a plaintext password with argon2id and print the PHC string,
    /// suitable for `api_password = "$argon2id$..."` in config.toml.
    HashPassword {
        /// Password to hash. If omitted, reads one line from stdin.
        password: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Some(Command::HashPassword { password }) = cli.command {
        let plaintext = match password {
            Some(p) => p,
            None => {
                use std::io::BufRead;
                let stdin = std::io::stdin();
                let mut line = String::new();
                stdin.lock().read_line(&mut line)?;
                line.trim_end_matches(['\n', '\r']).to_string()
            }
        };
        if plaintext.is_empty() {
            anyhow::bail!("password must not be empty");
        }
        let phc = api::hash_password(&plaintext)?;
        println!("{phc}");
        return Ok(());
    }

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let json = std::env::var("LIGHTORRENT_LOG_JSON").ok().as_deref() == Some("1");
    let log_dir = std::env::var("LIGHTORRENT_LOG_DIR").ok();

    // Hold the WorkerGuard for the lifetime of main() so the non-blocking
    // appender flushes on exit. None means we're logging to stdout.
    let _guard: Option<tracing_appender::non_blocking::WorkerGuard> = if let Some(dir) = log_dir {
        std::fs::create_dir_all(&dir)?;
        let file_appender = tracing_appender::rolling::daily(&dir, "lightorrent.log");
        let (nb, guard) = tracing_appender::non_blocking(file_appender);
        let builder = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(nb);
        if json {
            builder.json().init();
        } else {
            builder.init();
        }
        Some(guard)
    } else {
        let builder = tracing_subscriber::fmt().with_env_filter(env_filter);
        if json {
            builder.json().init();
        } else {
            builder.init();
        }
        None
    };

    let config_path = cli
        .config
        .ok_or_else(|| anyhow::anyhow!("--config is required when running the daemon"))?;
    debug!(path = %config_path, "loading config");
    let config = config::Config::load(&config_path)?;
    info!(
        version = lightorrent::version_string(),
        git = env!("BUILD_GIT_HASH"),
        built = env!("BUILD_TIMESTAMP"),
        download_dir = %config.download_dir,
        listen_port = %config.listen_port,
        "lightorrent starting",
    );

    let engine = Arc::new(engine::Engine::new(&config).await?);

    let router = api::router(engine.clone(), &config);
    let listener =
        tokio::net::TcpListener::bind(format!("{}:{}", config.api_bind_address, config.api_port))
            .await?;
    info!(port = config.api_port, bind = %config.api_bind_address, "API server listening on {}:{}", config.api_bind_address, config.api_port);

    let shutdown_token = engine.cancel_token();
    let server_token = shutdown_token.clone();
    let server_handle = tokio::spawn(
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move { server_token.cancelled().await })
        .into_future(),
    );

    if let Some(torrents) = &config.torrents {
        for source in torrents {
            let result = match std::fs::read(source) {
                Ok(bytes) => {
                    engine
                        .add_torrent_bytes(bytes, &config.download_dir, "", false)
                        .await
                }
                Err(e) => {
                    error!(source, error = %e, "failed to read torrent file");
                    continue;
                }
            };
            if let Err(e) = result {
                error!(source, error = %e, "failed to add torrent");
            }
        }
    }

    info!("daemon running, waiting for signal");

    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("received SIGINT");
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM");
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        info!("received SIGINT");
    }

    engine.shutdown().await;
    match server_handle.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => error!(error = %e, "API server exited with error"),
        Err(e) if e.is_cancelled() => {}
        Err(e) => error!(error = %e, "API server task join error"),
    }
    info!("lightorrent stopped");

    Ok(())
}
