mod config;
mod handlers;
mod agent;

use std::net::SocketAddr;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, patch, post};
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
            "/api/v2/upctl/api/tickets",
            get(handlers::gitea_list_tickets).post(handlers::gitea_create_ticket),
        )
        .route(
            "/api/v2/upctl/api/tickets/labels",
            get(handlers::gitea_list_labels),
        )
        .route(
            "/api/v2/upctl/api/tickets/{id}",
            get(handlers::gitea_get_ticket).patch(handlers::gitea_update_ticket),
        )
        .route(
            "/api/v2/upctl/api/tickets/{id}/close",
            post(handlers::gitea_close_ticket),
        )
        .route(
            "/api/v2/upctl/api/tickets/{id}/labels",
            post(handlers::gitea_add_label),
        )
        .route(
            "/api/v2/upctl/api/tickets/{id}/labels/{label_id}",
            get(handlers::gitea_remove_label).delete(handlers::gitea_remove_label),
        )
        .route(
            "/api/v2/upctl/api/tickets/{id}/comments",
            post(handlers::gitea_add_comment),
        )
        // Attachment upload/serve
        .route(
            "/api/v2/upctl/api/upload_attachment",
            post(handlers::upload_attachment),
        )
        .route(
            "/api/v2/upctl/api/attachment/{filename}",
            get(handlers::serve_attachment),
        )
        // Project management
        .route(
            "/api/v2/upctl/api/projects",
            get(handlers::list_projects).post(handlers::create_project),
        )
        .route(
            "/api/v2/upctl/api/projects/{id}",
            patch(handlers::update_project).delete(handlers::delete_project),
        )
        // Agent / tmux endpoints
        .route(
            "/api/v2/upctl/api/tmux/{session}",
            get(handlers::agent_capture),
        )
        .route(
            "/api/v2/upctl/api/tmux/{session}/send",
            post(handlers::agent_send_keys),
        )
        .route(
            "/api/v2/upctl/api/agent/prompt",
            post(handlers::agent_prompt),
        )
        // Config: custom prompt prefix + memory dir
        .route(
            "/api/v2/upctl/api/config/prompt-prefix",
            get(handlers::get_prompt_prefix).put(handlers::set_prompt_prefix),
        )
        .route(
            "/api/v2/upctl/api/config/memory-dir",
            get(handlers::get_memory_dir).put(handlers::set_memory_dir),
        )
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024));

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("[upctl-svc] listening on {addr}");
    axum::serve(listener, app.into_make_service()).await?;

    Ok(())
}
