mod auth;
mod config;
mod db;
mod error;
mod gallery;
mod gallery_tags;
mod media;
mod models;
mod routes;
mod search;
mod storage;
mod tags;

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::middleware;
use axum::Router;

const MAX_UPLOAD_BYTES: usize = 1024 * 1024 * 1024; // 1 GiB
use sqlx::SqlitePool;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<config::ServerConfig>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = config::Config::load()?;
    let servers = config.into_servers();

    let mut set = tokio::task::JoinSet::new();
    for server_config in servers {
        set.spawn(run_server(server_config));
    }

    while let Some(result) = set.join_next().await {
        result??;
    }
    Ok(())
}

async fn run_server(config: config::ServerConfig) -> anyhow::Result<()> {
    tracing::info!("using library at {}", config.library_path.display());

    let pool = db::connect(&config.library_path).await?;
    let state = AppState { pool, config: Arc::new(config) };

    let api = routes::router()
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_token));

    let app = Router::new()
        .merge(api)
        .layer(TraceLayer::new_for_http())
        .layer(
            // A wildcard `Access-Control-Allow-Headers: *` doesn't cover
            // `Authorization` per the fetch spec, so every authenticated
            // request would fail CORS — it must be listed explicitly.
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers([AUTHORIZATION, CONTENT_TYPE]),
        )
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state.clone());

    let addr = format!("{}:{}", state.config.host, state.config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("phoserv server listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
