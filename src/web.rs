use axum::{Json, Router, extract::State, response::Html, routing::get};
use std::env;
use tokio::sync::watch;

use crate::AppState;

pub async fn start_server(rx: watch::Receiver<Option<AppState>>) {
    let port_str = env::var("WEB_PORT").unwrap_or_else(|_| "8080".to_string());
    let port: u16 = port_str.parse().unwrap_or(8080);

    let app = Router::new()
        .route("/", get(index))
        .route("/api/state", get(get_state))
        .with_state(rx);

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

async fn index() -> Html<&'static str> {
    Html(include_str!("static/index.html"))
}

async fn get_state(State(rx): State<watch::Receiver<Option<AppState>>>) -> Json<Option<AppState>> {
    let state = rx.borrow().clone();
    Json(state)
}
