use axum::{Json, Router, extract::State, response::Html, routing::get};
use std::env;
use tokio::sync::watch;

use crate::AppState;

pub async fn start_server(rx: watch::Receiver<Option<AppState>>) {
    let port_str = env::var("WEB_PORT").unwrap_or_else(|_| "8080".to_string());
    let port: u16 = port_str.parse().unwrap_or(8080);

    let mut context_path = env::var("APP_CONTEXT_PATH").unwrap_or_else(|_| "".to_string());
    if !context_path.is_empty() && !context_path.starts_with('/') {
        context_path = format!("/{}", context_path);
    }
    // Remove trailing slash if present for consistency
    if context_path.ends_with('/') {
        context_path.pop();
    }

    let api_router = Router::new().route("/state", get(get_state));

    let app = Router::new()
        .route("/", get(index))
        .nest("/api", api_router)
        .with_state(rx);

    // If context_path is empty, nest at "/", else nest at context_path
    let app = if context_path.is_empty() {
        app
    } else {
        Router::new().nest(&context_path, app)
    };

    let addr = format!("0.0.0.0:{}", port);
    log::info!("Starting web server on {}", addr);

    match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => {
            if let Err(e) = axum::serve(listener, app).await {
                log::error!("Web server error: {}", e);
            }
        }
        Err(e) => {
            log::error!("Failed to bind web server to {}: {}", addr, e);
        }
    }
}

async fn index() -> Html<String> {
    let mut context_path = env::var("APP_CONTEXT_PATH").unwrap_or_else(|_| "".to_string());
    if !context_path.is_empty() && !context_path.starts_with('/') {
        context_path = format!("/{}", context_path);
    }
    if context_path.ends_with('/') {
        context_path.pop();
    }

    let html_content = include_str!("static/index.html");
    Html(html_content.replace("{{APP_CONTEXT_PATH}}", &context_path))
}

async fn get_state(State(rx): State<watch::Receiver<Option<AppState>>>) -> Json<Option<AppState>> {
    let state = rx.borrow().clone();
    Json(state)
}
