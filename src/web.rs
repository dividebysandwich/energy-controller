use axum::{Json, Router, extract::{State, Path}, response::{Html, IntoResponse}, routing::get};
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

    let api_router = Router::new()
        .route("/state", get(get_state))
        .route("/alarm/status", get(get_alarm_status))
        .route("/alarm/code/:pin", get(send_alarm_code));

    let app = Router::new()
        .route("/", get(index))
        .route("/keypad", get(keypad))
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

async fn keypad() -> Html<String> {
    let mut context_path = env::var("APP_CONTEXT_PATH").unwrap_or_else(|_| "".to_string());
    if !context_path.is_empty() && !context_path.starts_with('/') {
        context_path = format!("/{}", context_path);
    }
    if context_path.ends_with('/') {
        context_path.pop();
    }

    let html_content = include_str!("static/keypad.html");
    Html(html_content.replace("{{APP_CONTEXT_PATH}}", &context_path))
}

async fn get_state(State(rx): State<watch::Receiver<Option<AppState>>>) -> Json<Option<AppState>> {
    let state = rx.borrow().clone();
    Json(state)
}

async fn get_alarm_status() -> impl IntoResponse {
    let client = reqwest::Client::new();
    match client.get("http://192.168.178.11:8081/get_status").send().await {
        Ok(res) => {
            match res.text().await {
                Ok(text) => text.into_response(),
                Err(e) => {
                    log::error!("Error reading alarm status response: {}", e);
                    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Error reading response").into_response()
                }
            }
        }
        Err(e) => {
            log::error!("Error fetching alarm status: {}", e);
            (axum::http::StatusCode::BAD_GATEWAY, "Error connecting to alarm system").into_response()
        }
    }
}

async fn send_alarm_code(Path(pin): Path<String>) -> impl IntoResponse {
    let client = reqwest::Client::new();
    let url = format!("http://192.168.178.11:8081/code/{}", pin);
    match client.get(&url).send().await {
        Ok(res) => {
            match res.text().await {
                Ok(text) => text.into_response(),
                Err(e) => {
                    log::error!("Error reading alarm code response: {}", e);
                    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Error reading response").into_response()
                }
            }
        }
        Err(e) => {
            log::error!("Error sending alarm code: {}", e);
            (axum::http::StatusCode::BAD_GATEWAY, "Error connecting to alarm system").into_response()
        }
    }
}
