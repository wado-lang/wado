//! HTTP routing benchmark server — Axum on native Tokio.
//!
//! Mirrors benchmark/http_routing/app.wado and app.js: the same route
//! set and the same `{ route, params }` JSON response shape. Axum is the
//! native-Rust reference point for the `wado serve` vs Hono comparison.
//!
//! The route set is Hono's official router benchmark
//! (honojs/hono, benchmarks/routers/src/tool.mts).
//!
//!   PORT=3001 cargo run --release --manifest-path benchmark/http_routing/Cargo.toml

use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router, extract::Path};
use serde_json::{Value, json};

fn body(route: &str, params: Vec<String>) -> Json<Value> {
    Json(json!({ "route": route, "params": params }))
}

async fn user() -> Json<Value> {
    body("user", vec![])
}
async fn user_comments() -> Json<Value> {
    body("user.comments", vec![])
}
async fn user_avatar() -> Json<Value> {
    body("user.avatar", vec![])
}
async fn user_lookup_username(Path(username): Path<String>) -> Json<Value> {
    body("user.lookup.username", vec![username])
}
async fn user_lookup_email(Path(address): Path<String>) -> Json<Value> {
    body("user.lookup.email", vec![address])
}
async fn event_show(Path(id): Path<String>) -> Json<Value> {
    body("event.show", vec![id])
}
async fn event_comments(Path(id): Path<String>) -> Json<Value> {
    body("event.comments", vec![id])
}
async fn event_comment_create(Path(id): Path<String>) -> Json<Value> {
    body("event.comment.create", vec![id])
}
async fn map_events(Path(location): Path<String>) -> Json<Value> {
    body("map.events", vec![location])
}
async fn status() -> Json<Value> {
    body("status", vec![])
}
async fn deeply_nested() -> Json<Value> {
    body("deeply.nested", vec![])
}
async fn static_(Path(path): Path<String>) -> Json<Value> {
    body("static", vec![path])
}
async fn not_found() -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, body("not-found", vec![]))
}

#[tokio::main]
async fn main() {
    // Route set: honojs/hono, benchmarks/routers/src/tool.mts.
    let app = Router::new()
        .route("/user", get(user))
        .route("/user/comments", get(user_comments))
        .route("/user/avatar", get(user_avatar))
        .route("/user/lookup/username/{username}", get(user_lookup_username))
        .route("/user/lookup/email/{address}", get(user_lookup_email))
        .route("/event/{id}", get(event_show))
        .route("/event/{id}/comments", get(event_comments))
        .route("/event/{id}/comment", post(event_comment_create))
        .route("/map/{location}/events", get(map_events))
        .route("/status", get(status))
        .route("/very/deeply/nested/route/hello/there", get(deeply_nested))
        .route("/static/{*path}", get(static_))
        .fallback(not_found);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3001);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    eprintln!("axum listening on http://{addr}/");
    axum::serve(listener, app).await.unwrap();
}
