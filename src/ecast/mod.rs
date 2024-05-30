use std::{borrow::Cow, collections::HashMap, sync::Arc};

use axum::{
    extract::{Path, Query, WebSocketUpgrade},
    response::Response,
    Json,
};
use color_eyre::eyre::{eyre, Context};
use dashmap::DashMap;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Notify;
use tracing::instrument;

use crate::{
    acl::Role,
    error::{PropogateRequest, WithStatusCode},
    JBRoom, OpMode, State, Token,
};

pub mod ws;

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RoomRequest {
    pub app_id: String,
    pub app_tag: String,
    pub audience_enabled: bool,
    pub max_players: u8,
    pub platform: String,
    pub player_names: serde_json::Value,
    pub time: f32,
    pub twitch_locked: bool,
    pub user_id: uuid::Uuid,
    #[serde(default)]
    pub host: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct WSQuery {
    #[serde(rename = "user-id")]
    pub user_id: String,
    pub format: String,
    pub name: String,
    pub role: Role,
    #[serde(rename = "host-token")]
    pub host_token: Option<Token>,
    pub secret: Option<Token>,
    #[serde(default)]
    // Id will never be 0 (this works)
    id: i64,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct JBResponse<T: Serialize + std::fmt::Debug> {
    ok: bool,
    #[serde(flatten)]
    body: JBResponseBody<T>,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum JBResponseBody<T: Serialize + std::fmt::Debug> {
    Body(T),
    Error(Cow<'static, str>),
}

#[derive(Deserialize, Serialize, Debug)]
pub struct RoomResponse {
    host: String,
    code: String,
    token: String,
}

pub async fn play_handler(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<State>,
    code: Path<String>,
    url_query: Query<WSQuery>,
) -> Result<Response, (StatusCode, Json<JBResponse<()>>)> {
    let Some(config) = state.room_map.get(&code.0) else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(JBResponse {
                ok: false,
                body: JBResponseBody::Error(Cow::Borrowed("no such room")),
            }),
        ));
    };
    let mut host = match url_query.role {
        Role::Audience => "https://ecast.jackboxgames.com".to_owned(),
        _ => format!("wss://{}", config.value().room_config.host),
    };

    host = host.replace("https://", "wss://");
    host = host.replace("http://", "ws://");

    if matches!(state.config.ecast.op_mode, OpMode::Proxy) {
        Ok(ws.protocols(["ecast-v0"]).on_upgrade(move |socket| {
            let ecast_req = format!(
                "{}/api/v2/{}/{}/play?{}",
                host,
                match url_query.role {
                    Role::Audience => "audience",
                    _ => "rooms",
                },
                code.0,
                serde_urlencoded::to_string(&url_query.0).unwrap()
            );
            async move {
                if let Err(e) = ws::handle_socket_proxy(host, socket, ecast_req).await {
                    tracing::error!(error = %e, "Failed to proxy ecast client");
                }
            }
        }))
    } else {
        let room = Arc::clone(config.value());
        let config = Arc::clone(&state.config);
        Ok(ws.protocols(["ecast-v0"])
            .on_upgrade(move |socket| async move {
                match ws::connect_socket(socket, url_query, room).await.wrap_err("Failed to connect ecast client") {
                    Err(e) => {
                        tracing::error!(%e);
                        return;
                    }
                    Ok(connected) => {
                        if let Err(e) = ws::handle_socket(Arc::clone(&connected.client), Arc::clone(&connected.room), connected.reconnected, connected.read_half, &config.doodles).await {
                            tracing::error!(id = connected.client.profile.id, role = ?connected.client.profile.role, code = connected.room.room_config.code, error = %e, "Error in WebSocket");
                            connected.client.disconnect().await;
                        } else {
                            connected.client.disconnect().await;
                            tracing::debug!(id = connected.client.profile.id, role = ?connected.client.profile.role, code = connected.room.room_config.code, "Leaving room");
                        }
                    }
                }
            })
        )
    }
}

#[instrument(skip(state))]
pub async fn rooms_handler(
    axum::extract::State(state): axum::extract::State<State>,
    Json(room_req): Json<RoomRequest>,
) -> crate::error::Result<Json<JBResponse<RoomResponse>>> {
    let code;
    let token;
    let host;
    match state.config.ecast.op_mode {
        OpMode::Proxy => {
            let url = format!(
                "{}/api/v2/rooms",
                state
                    .config
                    .ecast
                    .server_url
                    .as_ref()
                    .map(|s| s.as_str())
                    .unwrap_or("https://ecast.jackboxgames.com")
            );

            let response: JBResponse<RoomResponse> = state
                .http_cache
                .client
                .post(&url)
                .json(&room_req)
                .send()
                .await
                .wrap_err("Request for room failed")
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?
                .propogate_request_if_err()?
                .json()
                .await
                .wrap_err("Failed to convert to result of room request from JSON")
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

            tracing::debug!(
                url = url,
                response = ?response,
                "ecast request"
            );

            match response.body {
                JBResponseBody::Body(body) => {
                    code = body.code;
                    token = body.token;
                    host = body.host;
                }
                JBResponseBody::Error(_) => return Ok(Json(response)),
            }
        }
        OpMode::Native => {
            code = crate::room_id();
            token = format!("{:02x}", Token::random());
            host = state.config.accessible_host.to_owned();
        }
    }

    tracing::debug!(code, token, host, "Creating room");

    state.room_map.insert(
        code.clone(),
        Arc::new(crate::Room {
            entities: DashMap::new(),
            connections: DashMap::new(),
            room_serial: 1.into(),
            room_config: JBRoom {
                app_id: room_req.app_id,
                app_tag: room_req.app_tag.clone(),
                audience_enabled: room_req.audience_enabled,
                code: code.clone(),
                host,
                audience_host: state.config.accessible_host.clone(),
                locked: false,
                full: false,
                moderation_enabled: false,
                password_required: false,
                twitch_locked: false, // unimplemented
                locale: Cow::Borrowed("en"),
                keepalive: false,
            },
            exit: Notify::new(),
        }),
    );

    Ok(Json(JBResponse {
        ok: true,
        body: JBResponseBody::Body(RoomResponse {
            host: state.config.accessible_host.clone(),
            code,
            token,
        }),
    }))
}

#[instrument(skip(state))]
pub async fn rooms_get_handler(
    axum::extract::State(state): axum::extract::State<State>,
    Path(code): Path<String>,
) -> crate::error::Result<Json<JBResponse<JBRoom>>> {
    match state.config.ecast.op_mode {
        OpMode::Native => {
            let room = state.room_map.get(&code);

            if let Some(room) = room {
                return Ok(Json(JBResponse {
                    ok: true,
                    body: JBResponseBody::Body(room.value().room_config.clone()),
                }));
            } else {
                return Err(eyre!("no such room")).with_status_code(StatusCode::NOT_FOUND);
            }
        }
        OpMode::Proxy => {
            let url = format!(
                "{}/api/v2/rooms/{}",
                state
                    .config
                    .ecast
                    .server_url
                    .as_ref()
                    .map(|s| s.as_str())
                    .unwrap_or("https://ecast.jackboxgames.com"),
                code
            );
            let response = state
                .http_cache
                .client
                .get(&url)
                .send()
                .await
                .wrap_err_with(|| format!("Failed to request room from {}", url))
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?
                .propogate_request_if_err()?;
            let status_code = response.status();
            let mut response: JBResponse<JBRoom> = response
                .json()
                .await
                .wrap_err("Failed to deserialize room to JSON")
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

            tracing::debug!(url, ?status_code, ?response, "ecast request");

            match &mut response.body {
                JBResponseBody::Body(body) => {
                    body.host = state.config.accessible_host.clone();
                    body.audience_host = state.config.accessible_host.clone();
                }
                _ => {}
            }

            Ok(Json(response))
        }
    }
}

#[instrument(skip(state))]
pub async fn app_config_handler(
    Path(code): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    axum::extract::State(state): axum::extract::State<State>,
) -> crate::error::Result<Json<JBResponse<serde_json::Value>>> {
    match state.config.ecast.op_mode {
        OpMode::Native => {
            return Ok(Json(JBResponse {
                ok: true,
                body: JBResponseBody::Body(json!({
                    "settings": {
                        "serverUrl": state.config.accessible_host.clone()
                    }
                })),
            }));
        }
        OpMode::Proxy => {
            let url = format!(
                "{}/api/v2/app-configs/{}?{}",
                state
                    .config
                    .ecast
                    .server_url
                    .as_ref()
                    .map(|s| s.as_str())
                    .unwrap_or("https://ecast.jackboxgames.com"),
                code,
                serde_urlencoded::to_string(query).unwrap()
            );
            let response: JBResponse<serde_json::Value> = state
                .http_cache
                .client
                .get(&url)
                .send()
                .await
                .wrap_err("Failed to get app config")
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?
                .propogate_request_if_err()?
                .json()
                .await
                .wrap_err("Failed to deserialize app config to JSON")
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

            tracing::debug!(
                url = url,
                response = ?response,
                "ecast request"
            );

            Ok(Json(response))
        }
    }
}
