
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, Method, StatusCode, Uri},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use reqwest::Client;
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower::ServiceBuilder;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};

// --- App State ---
struct AppState {
    http_client: Client,
    api_token: Arc<RwLock<Option<String>>>,
}

// --- Embedded static assets ---
#[derive(Embed)]
#[folder = "static/"]
struct Assets;

async fn static_handler(uri: Uri) -> impl IntoResponse {
    let mut path = uri.path().trim_start_matches('/').to_string();
    if path.is_empty() {
        path = "index.html".to_string();
    }

    match Assets::get(&path) {
        Some(file) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref())],
                file.data,
            )
                .into_response()
        }
        None => {
            if let Some(file) = Assets::get("index.html") {
                (
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    file.data,
                )
                    .into_response()
            } else {
                (StatusCode::NOT_FOUND, "Not Found").into_response()
            }
        }
    }
}

// --- Token helpers ---
fn token_file_path() -> Option<PathBuf> {
    let mut path = std::env::current_exe().ok()?;
    path.set_file_name("groupme_token");
    Some(path)
}

fn load_token_from_file() -> Option<String> {
    let path = token_file_path()?;
    std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn save_token_to_file(token: &str) -> Result<(), String> {
    let path = token_file_path().ok_or("Could not determine executable path")?;
    std::fs::write(&path, token).map_err(|e| format!("Failed to write token file: {}", e))
}

async fn get_token(state: &AppState) -> Result<String, StatusCode> {
    let guard = state.api_token.read().await;
    guard
        .clone()
        .ok_or(StatusCode::UNAUTHORIZED)
}

// --- Group Structs ---
#[derive(Deserialize, Serialize)]
struct GroupMeResponse {
    response: Vec<Group>,
}

#[derive(Deserialize, Serialize)]
struct Group {
    id: String,
    name: String,
    members: Option<Vec<Member>>,
}

#[derive(Deserialize, Serialize)]
struct Member {
    user_id: String,
    nickname: String,
}

// --- DM Chat Structs ---
#[derive(Deserialize, Serialize)]
struct ChatsResponse {
    response: Vec<Chat>,
}

#[derive(Deserialize, Serialize)]
struct Chat {
    other_user: OtherUser,
}

#[derive(Deserialize, Serialize)]
struct OtherUser {
    id: String,
    name: String,
}

// --- Message Structs ---
#[derive(Deserialize, Serialize)]
struct MessagesResponse {
    response: MessagesData,
}

#[derive(Deserialize, Serialize)]
struct MessagesData {
    messages: Vec<Message>,
}

#[derive(Deserialize, Serialize)]
struct DMDataResponse {
    response: DMData,
}

#[derive(Deserialize, Serialize)]
struct DMData {
    direct_messages: Vec<Message>,
}

#[derive(Deserialize, Serialize)]
struct Message {
    id: String,
    created_at: i64,
    #[serde(rename = "user_id")]
    sender_id: String,
    name: String,
    text: Option<String>,
    attachments: Option<Vec<Attachment>>,
}

#[derive(Deserialize, Serialize, Clone)]
struct Attachment {
    #[serde(rename = "type")]
    attachment_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

// --- Send Message Structs ---
#[derive(Deserialize)]
struct SendMessageReq {
    text: Option<String>,
    source_guid: String,
    attachments: Option<Vec<Attachment>>,
}

#[derive(Serialize)]
struct GroupMeSendPayload {
    message: GroupMeSendMessage,
}

#[derive(Serialize)]
struct GroupMeSendMessage {
    source_guid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attachments: Option<Vec<Attachment>>,
}

#[derive(Serialize)]
struct GroupMeSendDMPayload {
    direct_message: GroupMeSendDM,
}

#[derive(Serialize)]
struct GroupMeSendDM {
    source_guid: String,
    recipient_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attachments: Option<Vec<Attachment>>,
}

#[derive(Deserialize)]
struct MessageParams {
    before_id: Option<String>,
}

#[derive(Deserialize)]
struct SetTokenReq {
    token: String,
}

#[derive(Serialize)]
struct StatusResponse {
    has_token: bool,
}

// --- Validation Helper ---
fn validate_id(id: &str) -> Result<(), String> {
    if id.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') && !id.is_empty() {
        Ok(())
    } else {
        Err(format!("Invalid ID format: {}", id))
    }
}

fn api_error_response<T: std::fmt::Display>(err: T, context: &str) -> impl IntoResponse {
    tracing::error!("{}: {}", context, err);
    (StatusCode::BAD_GATEWAY, format!("{} failed", context)).into_response()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    // Resolve token: env var → file next to binary
    let initial_token = std::env::var("GROUPME_TOKEN")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(load_token_from_file);

    if initial_token.is_some() {
        tracing::info!("GroupMe token loaded");
    } else {
        tracing::warn!("No GroupMe token found — open the UI to set one");
    }

    let state = Arc::new(AppState {
        http_client: Client::new(),
        api_token: Arc::new(RwLock::new(initial_token)),
    });

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST])
        .allow_headers(Any);

    let app = Router::new()
        .route("/api/groups", get(get_groups))
        .route(
            "/api/groups/:group_id/messages", // <-- Changed {group_id} to :group_id
            get(get_group_messages).post(send_group_message),
        )
        .route("/api/chats", get(get_chats))
        .route(
            "/api/chats/:other_user_id/messages", // <-- Changed {other_user_id} to :other_user_id
            get(get_dm_messages).post(send_dm_message),
        )
        .route("/api/upload_image", post(upload_image))
        .fallback(static_handler)
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(cors)
                .layer(DefaultBodyLimit::max(15 * 1024 * 1024)),
        )
        .with_state(state);

    tracing::info!("Server running on http://localhost:8080");

    if let Err(e) = open::that("http://localhost:8080") {
        tracing::warn!("Could not open browser automatically: {}", e);
    }

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// --- Token endpoints ---
async fn get_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let has_token = state.api_token.read().await.is_some();
    Json(StatusResponse { has_token })
}

async fn set_token(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<SetTokenReq>,
) -> impl IntoResponse {
    let token = payload.token.trim().to_string();
    if token.is_empty() {
        return (StatusCode::BAD_REQUEST, "Token cannot be empty").into_response();
    }

    if let Err(e) = save_token_to_file(&token) {
        tracing::error!("{}", e);
        // Still keep it in memory even if file write fails
    }

    *state.api_token.write().await = Some(token);
    tracing::info!("GroupMe token saved");
    StatusCode::OK.into_response()
}

// --- Image Proxy Handler ---
async fn upload_image(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let api_token = match get_token(&state).await {
        Ok(t) => t,
        Err(status) => return status.into_response(),
    };

    let content_type = headers
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg");

    let allowed_types = ["image/jpeg", "image/png", "image/gif", "image/webp"];
    if !allowed_types.contains(&content_type) {
        return (StatusCode::BAD_REQUEST, "Invalid content type").into_response();
    }

    let url = "https://image.groupme.com/pictures";

    match state
        .http_client
        .post(url)
        .header("X-Access-Token", &api_token)
        .header("Content-Type", content_type)
        .body(body)
        .send()
        .await
    {
        Ok(res) => {
            if let Ok(data) = res.json::<serde_json::Value>().await {
                (StatusCode::OK, Json(data)).into_response()
            } else {
                tracing::error!("Failed to parse image upload response");
                (StatusCode::INTERNAL_SERVER_ERROR, "Parse Error").into_response()
            }
        }
        Err(e) => {
            tracing::error!("Image upload failed: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Upload Error").into_response()
        }
    }
}

// --- Group Handlers ---
async fn get_groups(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let api_token = match get_token(&state).await {
        Ok(t) => t,
        Err(status) => return status.into_response(),
    };

    let url = "https://api.groupme.com/v3/groups";

    match state
        .http_client
        .get(url)
        .header("X-Access-Token", &api_token)
        .query(&[("per_page", "500")])
        .send()
        .await
    {
        Ok(res) => {
            if let Ok(data) = res.json::<GroupMeResponse>().await {
                (StatusCode::OK, Json(data.response)).into_response()
            } else {
                tracing::error!("Failed to parse groups response");
                (StatusCode::INTERNAL_SERVER_ERROR, "Parse error").into_response()
            }
        }
        Err(e) => api_error_response(e, "Groups fetch").into_response(),
    }
}

async fn get_group_messages(
    State(state): State<Arc<AppState>>,
    Path(group_id): Path<String>,
    Query(params): Query<MessageParams>,
) -> impl IntoResponse {
    let api_token = match get_token(&state).await {
        Ok(t) => t,
        Err(status) => return status.into_response(),
    };

    if let Err(e) = validate_id(&group_id) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }

    let url = format!("https://api.groupme.com/v3/groups/{}/messages", group_id);

    let mut request = state
        .http_client
        .get(&url)
        .header("X-Access-Token", &api_token)
        .query(&[("limit", "50")]);

    if let Some(ref before_id) = params.before_id {
        if validate_id(before_id).is_err() {
            return (StatusCode::BAD_REQUEST, "Invalid before_id format").into_response();
        }
        request = request.query(&[("before_id", before_id.as_str())]);
    }

    match request.send().await {
        Ok(res) => {
            if let Ok(data) = res.json::<MessagesResponse>().await {
                (StatusCode::OK, Json(data.response.messages)).into_response()
            } else {
                tracing::error!("Failed to parse messages response for group {}", group_id);
                (StatusCode::INTERNAL_SERVER_ERROR, "Parse error").into_response()
            }
        }
        Err(e) => api_error_response(e, "Messages fetch").into_response(),
    }
}

async fn send_group_message(
    State(state): State<Arc<AppState>>,
    Path(group_id): Path<String>,
    Json(payload): Json<SendMessageReq>,
) -> impl IntoResponse {
    let api_token = match get_token(&state).await {
        Ok(t) => t,
        Err(status) => return status.into_response(),
    };

    if let Err(e) = validate_id(&group_id) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }

    let url = format!("https://api.groupme.com/v3/groups/{}/messages", group_id);

    let gm_payload = GroupMeSendPayload {
        message: GroupMeSendMessage {
            source_guid: payload.source_guid,
            text: payload.text,
            attachments: payload.attachments,
        },
    };

    match state
        .http_client
        .post(&url)
        .header("X-Access-Token", &api_token)
        .json(&gm_payload)
        .send()
        .await
    {
        Ok(res) if res.status().is_success() => StatusCode::OK.into_response(),
        Ok(res) => {
            tracing::error!("GroupMe returned error status: {}", res.status());
            (StatusCode::BAD_REQUEST, "Failed to send message").into_response()
        }
        Err(e) => api_error_response(e, "Message send").into_response(),
    }
}

// --- DM Handlers ---
async fn get_chats(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let api_token = match get_token(&state).await {
        Ok(t) => t,
        Err(status) => return status.into_response(),
    };

    let url = "https://api.groupme.com/v3/chats";

    match state
        .http_client
        .get(url)
        .header("X-Access-Token", &api_token)
        .query(&[("per_page", "100")])
        .send()
        .await
    {
        Ok(res) => {
            if let Ok(data) = res.json::<ChatsResponse>().await {
                (StatusCode::OK, Json(data.response)).into_response()
            } else {
                tracing::error!("Failed to parse chats response");
                (StatusCode::INTERNAL_SERVER_ERROR, "Parse error").into_response()
            }
        }
        Err(e) => api_error_response(e, "Chats fetch").into_response(),
    }
}

async fn get_dm_messages(
    State(state): State<Arc<AppState>>,
    Path(other_user_id): Path<String>,
    Query(params): Query<MessageParams>,
) -> impl IntoResponse {
    let api_token = match get_token(&state).await {
        Ok(t) => t,
        Err(status) => return status.into_response(),
    };

    if let Err(e) = validate_id(&other_user_id) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }

    let url = "https://api.groupme.com/v3/direct_messages";

    let mut request = state
        .http_client
        .get(url)
        .header("X-Access-Token", &api_token)
        .query(&[("other_user_id", other_user_id.as_str())])
        .query(&[("limit", "50")]);

    if let Some(ref before_id) = params.before_id {
        if validate_id(before_id).is_err() {
            return (StatusCode::BAD_REQUEST, "Invalid before_id format").into_response();
        }
        request = request.query(&[("before_id", before_id.as_str())]);
    }

    match request.send().await {
        Ok(res) => {
            if let Ok(data) = res.json::<DMDataResponse>().await {
                (StatusCode::OK, Json(data.response.direct_messages)).into_response()
            } else {
                tracing::error!(
                    "Failed to parse DM messages response for user {}",
                    other_user_id
                );
                (StatusCode::INTERNAL_SERVER_ERROR, "Parse error").into_response()
            }
        }
        Err(e) => api_error_response(e, "DM messages fetch").into_response(),
    }
}

async fn send_dm_message(
    State(state): State<Arc<AppState>>,
    Path(other_user_id): Path<String>,
    Json(payload): Json<SendMessageReq>,
) -> impl IntoResponse {
    let api_token = match get_token(&state).await {
        Ok(t) => t,
        Err(status) => return status.into_response(),
    };

    if let Err(e) = validate_id(&other_user_id) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }

    let url = "https://api.groupme.com/v3/direct_messages";

    let gm_payload = GroupMeSendDMPayload {
        direct_message: GroupMeSendDM {
            source_guid: payload.source_guid,
            recipient_id: other_user_id,
            text: payload.text,
            attachments: payload.attachments,
        },
    };

    match state
        .http_client
        .post(url)
        .header("X-Access-Token", &api_token)
        .json(&gm_payload)
        .send()
        .await
    {
        Ok(res) if res.status().is_success() => StatusCode::OK.into_response(),
        Ok(res) => {
            tracing::error!("GroupMe returned error status for DM: {}", res.status());
            (StatusCode::BAD_REQUEST, "Failed to send DM").into_response()
        }
        Err(e) => api_error_response(e, "DM send").into_response(),
    }
}
