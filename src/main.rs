mod cache;
mod config;
mod handlers;
mod model;
mod parallel;

use std::sync::Arc;
use std::time::Duration;

use salvo::compression::{Compression, CompressionLevel};
use salvo::cors::{AllowHeaders, AllowMethods, AllowOrigin, Cors};
use salvo::prelude::*;
use tracing_subscriber::EnvFilter;

use crate::cache::SearchCache;
use crate::config::Config;
use crate::handlers::{AppState, Stats, health, require_key, search};
use crate::parallel::ParallelClient;

fn main() {
    // Bunny Magic Containers expose the host's cores (often 32+) to the
    // container; an IO-bound proxy needs far fewer workers than that.
    let workers = std::env::var("TOKIO_WORKER_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(run());
}

async fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env().unwrap_or_else(|err| {
        eprintln!("{err}");
        std::process::exit(1);
    });

    let state = AppState {
        parallel: ParallelClient::new(
            config.parallel_base_url.clone(),
            config.parallel_api_key.clone(),
            config.request_timeout,
        ),
        cache: SearchCache::new(config.cache_max_bytes, config.cache_ttl),
        stats: Arc::new(Stats::default()),
        config: config.clone(),
    };

    let cors = Cors::new()
        .allow_origin(AllowOrigin::any())
        .allow_methods(AllowMethods::list([
            salvo::http::Method::GET,
            salvo::http::Method::POST,
            salvo::http::Method::OPTIONS,
        ]))
        .allow_headers(AllowHeaders::list([
            salvo::http::header::CONTENT_TYPE,
            salvo::http::header::HeaderName::from_static("x-api-key"),
        ]))
        .into_handler();

    let compression = Compression::new()
        .enable_gzip(CompressionLevel::Fastest)
        .enable_brotli(CompressionLevel::Fastest);

    let router = Router::new()
        .hoop(Logger::new())
        .hoop(cors)
        .hoop(compression)
        .hoop(affix_state::inject(state))
        .push(Router::with_path("health").get(health))
        .push(
            Router::with_path("v1/search")
                .hoop(require_key)
                .get(search)
                .post(search),
        );

    tracing::info!(
        bind = %config.bind,
        ttl_secs = config.cache_ttl.as_secs(),
        cache_max_bytes = config.cache_max_bytes,
        cdn_cache = ?config.cdn_cache,
        "starting search cache"
    );
    let bind = config.bind.clone();
    let acceptor = TcpListener::new(bind).bind().await;
    let server = Server::new(acceptor);

    let handle = server.handle();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutdown signal received, draining connections");
        handle.stop_graceful(Duration::from_secs(10));
    });

    server.serve(router).await;
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
