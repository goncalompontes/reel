use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::http::HeaderValue;
use axum::routing::{get, post};
use reel_core::Engine;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::handlers;

/// Shared state for every route.
#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    /// Used to turn relative stream paths into absolute URLs in responses.
    pub base_url: String,
}

/// Build the router without any cross-origin access.
///
/// A local API that can add torrents and delete files should not be reachable
/// from arbitrary web pages, so CORS is off unless explicitly enabled — see
/// [`build_router_with_allowed_origins`].
pub fn build_router(engine: Arc<Engine>, base_url: impl Into<String>) -> Router {
    build(engine, base_url.into(), None)
}

/// Build the router allowing browser access only from the given origins
/// (for example `http://localhost:5173` for a Vite dev server).
pub fn build_router_with_allowed_origins(
    engine: Arc<Engine>,
    base_url: impl Into<String>,
    origins: impl IntoIterator<Item = String>,
) -> Router {
    let origins: Vec<HeaderValue> = origins
        .into_iter()
        .filter_map(|o| HeaderValue::from_str(&o).ok())
        .collect();
    if origins.is_empty() {
        return build_router(engine, base_url);
    }
    build(engine, base_url.into(), Some(origins))
}

fn build(engine: Arc<Engine>, base_url: String, origins: Option<Vec<HeaderValue>>) -> Router {
    let state = AppState { engine, base_url };

    let router = Router::new()
        .route("/", get(handlers::index))
        .route("/api/health", get(handlers::health))
        .route("/api/config", get(handlers::get_config))
        .route(
            "/api/torrents",
            get(handlers::list_torrents).post(handlers::add_torrent),
        )
        .route(
            "/api/torrents/{id}",
            get(handlers::get_torrent).delete(handlers::delete_torrent),
        )
        .route(
            "/api/torrents/{id}/files",
            get(handlers::get_files).put(handlers::set_files),
        )
        .route("/api/torrents/{id}/stats", get(handlers::get_stats))
        .route("/api/torrents/{id}/pause", post(handlers::pause_torrent))
        .route("/api/torrents/{id}/resume", post(handlers::resume_torrent))
        .route("/api/events", get(handlers::events))
        .route("/stream/{id}/{file_id}", get(handlers::stream_file))
        .route(
            "/stream/{id}/{file_id}/{name}",
            get(handlers::stream_file_named),
        );

    let router = match origins {
        Some(origins) => router.layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(origins))
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        ),
        None => router,
    };

    router.layer(TraceLayer::new_for_http()).with_state(state)
}

/// Bind the API server.
pub async fn bind(addr: SocketAddr) -> anyhow::Result<tokio::net::TcpListener> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    Ok(listener)
}

/// Serve until Ctrl-C (or the future returned by `shutdown` resolves).
pub async fn serve(
    listener: tokio::net::TcpListener,
    engine: Arc<Engine>,
    base_url: impl Into<String>,
) -> anyhow::Result<()> {
    serve_with_shutdown(listener, engine, base_url, shutdown_signal()).await
}

pub async fn serve_with_shutdown(
    listener: tokio::net::TcpListener,
    engine: Arc<Engine>,
    base_url: impl Into<String>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let router = build_router(engine, base_url);
    serve_router_with_shutdown(listener, router, shutdown).await
}

/// Serve an already-built router until `shutdown` resolves.
///
/// Use this when you need to customise the router (CORS origins, extra routes,
/// middleware) — the desktop app does exactly that.
pub async fn serve_router_with_shutdown(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "HTTP API listening");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await?;

    Ok(())
}

/// Resolve on Ctrl-C.
pub async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!(error = %e, "failed to listen for Ctrl-C");
    }
}
