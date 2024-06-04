use std::{borrow::Cow, ops::Deref, sync::Arc};

use axum::{
    extract::{Path, Query, WebSocketUpgrade},
    response::IntoResponse,
    Json,
};
use color_eyre::eyre::Context;
use dashmap::DashMap;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tracing::instrument;

use crate::{
    error::{PropogateRequest, WithStatusCode},
    OpMode, State, Token,
};

pub mod ws;

// The only way to derive this it seems is by starting a game in each game in proxy
// mode, and noting down this app id and tag that jackbox.tv gives you.
pub const APP_TAGS: phf::Map<&'static str, &'static str> = phf::phf_map! {
    "6b0eba86a4a303019c1e0f87f6e3dc53" => "fibbage2",
    "WqtOjtE5JDSlp2APnbER9nqM5DEHjin7" => "earwax",
    "tYOBHxxuHbhpGSIptPVPaVnDXGmKHIST" => "auction",
    "bc140c96-9c1d-4640-aaf2-a4f57c0786d1" => "quiplash",
    "CCJhi8No2mKeADbC474Zn81Vstm7WyBP" => "bombintern",
    "c01f66be-745d-4173-8dac-c60395b2437a" => "quiplash2",
    "87fd7112-e835-4794-88bc-dc6e3630d640" => "triviadeath",
    "46a70386-d9a9-11e5-b5d2-0a1d41d68578" => "pollposition",
    "322cd918bbf2ac6af0c05b6f3b8b3ce8" => "fakinit",
    "c531ca944bf9762cd63a032d87cb96e7" => "awshirt",
    "75a6de72-ea54-e1cb-28e1-aab354704d45" => "fibbage3",
    "130f9f92-6fc4-4cdb-815e-0f65fdd2904b" => "survivetheinternet",
    "2c7351f4-953c-4ca3-88f3-7ddf9a973bd1" => "monstermingle",
    "ba051223-0c2e-4420-8f90-1356d94d6284" => "bracketeering",
    "c9ae7765-ee0a-a0f6-492c-90548b1d52fb" => "overdrawn",
    "70147cd2-65a0-43af-b7e4-2ebb9997544e" => "ydkj2018",
    "5983d1cf-bf20-5def-7224-3b6b07fa0a06" => "rapbattle",
    "25bcaae6-cada-4149-9947-5ef63f2aa2a4" => "splittheroom",
    "6a899966-4120-430c-9ae2-95f90767fa48" => "patentlystupid",
    "c72415bb-762a-df24-10f1-62dd419342a2" => "slingshoot",
    "d4eab799-37ab-4649-b599-548442d081ce" => "triviadeath2",
    "fe8be043-af2e-401f-8862-37fb386dfda7" => "rolemodels",
    "3c424bfe-1208-4c05-a83d-6a1888b8477d" => "jokeboat",
    "9364f2b4-e8e2-46ab-a8ad-71e0fce9e21b" => "ridictionary",
    "ddebfee6-5bc0-4325-a2d5-c50856802f9f" => "pushthebutton"
};

#[derive(Deserialize, Serialize, Debug)]
pub struct BlobcastWSQuery {
    t: u64,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct BlobcastRoomResponse {
    create: bool,
    server: String,
}

pub async fn play_handler(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<State>,
    id: Path<String>,
) -> impl IntoResponse {
    if matches!(state.config.blobcast.op_mode, OpMode::Proxy) {
        let host = format!(
            "wss://{}:38203/socket.io/1/websocket/{}",
            state.blobcast_host.read().await.deref(),
            id.0
        );

        ws.on_upgrade(move |socket| {
            let ecast_req = format!("{}/socket.io/websocket/{}", host, id.0);
            async move {
                if let Err(e) = ws::handle_socket_proxy(host, socket, ecast_req).await {
                    tracing::error!(error = %e, "Failed to proxy blobcast host");
                }
            }
        })
        .into_response()
    } else {
        let room_map = Arc::clone(&state.room_map);
        let sender = state.tui_sender.clone();
        ws
            .on_upgrade(move |socket| async move {
                match ws::connect_socket(socket, Arc::clone(&room_map), state.config.accessible_host.clone()).await.wrap_err("Failed to connect blobcast socket") {
                    Err(e) => {
                        tracing::error!(%e);
                        return;
                    }
                    Ok(socket) => {
                        sender.send(()).unwrap();
                        if let Err(e) = ws::handle_socket(socket.read_half, Arc::clone(&socket.room), Arc::clone(&socket.client)).await {
                            tracing::error!(id = socket.client.profile.id, role = ?socket.client.profile.role, error = %e, "Error in WebSocket");
                            socket.client.disconnect().await;
                        } else {
                            tracing::debug!(id = socket.client.profile.id, role = ?socket.client.profile.role, "Leaving room");
                            socket.client.disconnect().await;
                            socket.room.exit.notify_waiters();
                            tracing::debug!(socket.room.room_config.code, "Removing room");
                            room_map.remove(&socket.room.room_config.code);
                        }
                    }
                }
            })
            .into_response()
    }
}

#[instrument(skip(state))]
pub async fn load_handler(
    axum::extract::State(state): axum::extract::State<State>,
    url_query: Query<BlobcastWSQuery>,
) -> crate::error::Result<String> {
    match state.config.blobcast.op_mode {
        OpMode::Proxy => {
            let url = format!(
                "https://{}:38203/socket.io/1?{}",
                state.blobcast_host.read().await.deref(),
                serde_urlencoded::to_string(&url_query.0).unwrap()
            );

            let response: String = state
                .http_cache
                .client
                .get(&url)
                .send()
                .await
                .wrap_err("Failed to contact blobcast server to load room")
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?
                .propogate_request_if_err()?
                .text()
                .await
                .wrap_err("Failed to convert response of blobcast load to String")
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

            tracing::debug!(
                url = url,
                response = ?response,
                "blobcast request"
            );

            Ok(response)
        }
        OpMode::Native => Ok(format!(
            "{:x}:60:60:websocket",
            Token::from_seed(url_query.t)
        )),
    }
}

#[instrument(skip_all)]
pub async fn rooms_handler(
    axum::extract::State(state): axum::extract::State<State>,
) -> crate::error::Result<Json<BlobcastRoomResponse>> {
    let f_url;
    let response = match state.config.blobcast.op_mode {
        OpMode::Proxy => {
            let url = format!(
                "{}/room",
                state
                    .config
                    .blobcast
                    .server_url
                    .as_ref()
                    .map(|s| s.as_str())
                    .unwrap_or("http://blobcast.jackboxgames.com")
            );

            let mut response: BlobcastRoomResponse = state
                .http_cache
                .client
                .get(&url)
                .send()
                .await
                .wrap_err("Failed to create blobcast room")
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?
                .propogate_request_if_err()?
                .json()
                .await
                .wrap_err("Failed to convert to blobcast room response from JSON")
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

            *state.blobcast_host.write().await = response.server.clone();

            response.server = state.config.accessible_host.to_owned();

            tracing::debug!(
                url = url,
                response = ?response,
                "blobcast request"
            );

            f_url = Cow::Owned(url);

            response
        }
        OpMode::Native => {
            f_url = Cow::Borrowed("/room");
            BlobcastRoomResponse {
                create: true,
                server: state.config.accessible_host.to_owned(),
            }
        }
    };

    tracing::debug!(
        url = f_url.as_ref(),
        response = ?response,
        "blobcast request"
    );

    Ok(Json(response))
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AccessTokenRequest {
    app_id: uuid::Uuid,
    room_id: String,
    user_id: String,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AccessTokenResponse {
    access_token: Token,
    success: bool,
}

#[instrument(skip(state))]
pub async fn access_token_handler(
    axum::extract::State(state): axum::extract::State<State>,
    Json(token_req): Json<AccessTokenRequest>,
) -> crate::error::Result<Json<AccessTokenResponse>> {
    let response = match state.config.blobcast.op_mode {
        OpMode::Proxy => {
            let url = format!(
                "{}/accessToken",
                state
                    .config
                    .blobcast
                    .server_url
                    .as_ref()
                    .map(|s| s.as_str())
                    .unwrap_or("http://blobcast.jackboxgames.com")
            );

            let response: AccessTokenResponse = state
                .http_cache
                .client
                .post(&url)
                .json(&token_req)
                .send()
                .await
                .wrap_err("Failed to acquire access token")
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?
                .propogate_request_if_err()?
                .json()
                .await
                .wrap_err("Failed to decode access token response from JSON")
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

            tracing::debug!(
                url = url,
                response = ?response,
                "blobcast request"
            );

            state.room_map.insert(
                token_req.room_id.clone(),
                Arc::new(crate::Room {
                    entities: DashMap::new(),
                    connections: DashMap::new(),
                    room_serial: 1.into(),
                    room_config: crate::JBRoom {
                        app_id: token_req.app_id.to_string(),
                        app_tag: String::new(),
                        audience_enabled: false,
                        code: token_req.room_id.clone(),
                        host: state.blobcast_host.read().await.clone(),
                        audience_host: state.config.ecast.server_url.clone().unwrap_or_default(),
                        locked: false,
                        full: false,
                        moderation_enabled: false,
                        password_required: false,
                        twitch_locked: false,
                        locale: Cow::Borrowed("en"),
                        keepalive: false,
                    },
                    exit: Notify::new(),
                    channel: tokio::sync::watch::channel(()),
                }),
            );

            response
        }
        OpMode::Native => AccessTokenResponse {
            access_token: Token::random(),
            success: true,
        },
    };

    return Ok(Json(response));
}
