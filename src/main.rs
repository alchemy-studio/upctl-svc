mod config;
mod handlers;

use std::net::SocketAddr;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let port = config::port();
    tracing::info!("[upctl-svc] starting on port {port}");

    let app = Router::new()
        .route("/", get(|| async { "upctl-svc" }))
        // Ticket Gitea proxy
        .route(
            "/api/v2/ts/tickets",
            get(handlers::gitea_list_tickets).post(handlers::gitea_create_ticket),
        )
        .route(
            "/api/v2/ts/tickets/labels",
            get(handlers::gitea_list_labels),
        )
        .route(
            "/api/v2/ts/tickets/{id}",
            get(handlers::gitea_get_ticket).patch(handlers::gitea_update_ticket),
        )
        .route(
            "/api/v2/ts/tickets/{id}/close",
            post(handlers::gitea_close_ticket),
        )
        .route(
            "/api/v2/ts/tickets/{id}/labels",
            post(handlers::gitea_add_label),
        )
        .route(
            "/api/v2/ts/tickets/{id}/comments",
            post(handlers::gitea_add_comment),
        )
        // Attachment upload/serve
        .route(
            "/api/v2/ts/upload_attachment",
            post(handlers::upload_attachment),
        )
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
        .route(
            "/api/v2/ts/attachment/{filename}",
            get(handlers::serve_attachment),
        );

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("[upctl-svc] listening on {addr}");
    axum::serve(listener, app.into_make_service()).await?;

    Ok(())
}
