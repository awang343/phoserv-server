mod auth;
mod config;
mod db;
mod error;
mod media;
mod models;
mod routes;
mod storage;
mod tags;

use std::sync::Arc;

use axum::middleware;
use axum::Router;
use sqlx::SqlitePool;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<config::Config>,
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
    tracing::info!("using library at {}", config.library_path.display());

    let pool = db::connect(&config.library_path).await?;
    let state = AppState { pool, config: Arc::new(config) };

    let api = routes::router()
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_token));

    let app = Router::new()
        .merge(api)
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .with_state(state.clone());

    let addr = format!("{}:{}", state.config.host, state.config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("phoserv server listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
