use std::{
    borrow::Cow,
    io::{Read, Write},
    process::Stdio,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use crate::{
    acl::{Acl, Role},
    ecast::ws::JBResult,
    entity::{JBAttributes, JBEntity, JBObject, JBRestrictions, JBType, JBValue},
    Client, ClientType, ConnectedSocket, JBProfile, Room, Token,
};

use axum::extract::ws::{Message, WebSocket};
use color_eyre::eyre::{self, bail, eyre, OptionExt, WrapErr};
use dashmap::DashMap;
use futures_util::{stream::SplitStream, SinkExt, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use tokio::{
    io::Interest,
    sync::{Mutex, Notify},
};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::instrument;

#[derive(Debug)]
struct WSMessage<'a> {
    _opcode: u8,
    message: Option<JBMessage<'a>>,
}

impl<'a> FromStr for WSMessage<'a> {
    type Err = eyre::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s
            .split_once("::")
            .ok_or_else(|| eyre!("Failed to split socket.io message: {}", s))?;
        let opcode: u8 =
            s.0.parse()
                .wrap_err_with(|| format!("Opcode is not a valid number: {}", s.0))?;
        let message: Option<JBMessage> = if opcode == 5 {
            let message =
                s.1.get(1..)
                    .ok_or_else(|| eyre!("Opcode 5 contains no message: {}", s.1))?;
            Some(
                serde_json::from_str(&message)
                    .wrap_err_with(|| format!("Failed to deserialize message: {}", message))?,
            )
        } else {
            None
        };

        Ok(Self {
            _opcode: opcode,
            message,
        })
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub struct JBMessage<'a> {
    pub name: Cow<'a, str>,
    pub args: JBMessageArgs<'a>,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(untagged)]
pub enum JBMessageArgs<'a> {
    Object(JBArgs<'a>),
    Array([JBArgs<'a>; 1]),
}

impl<'a> JBMessageArgs<'a> {
    pub fn get_args(&self) -> &JBArgs<'a> {
        match self {
            JBMessageArgs::Object(a) => a,
            JBMessageArgs::Array(a) => &a[0],
        }
    }
}

fn object_is_empty(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => true,
        serde_json::Value::String(s) => s.is_empty(),
        serde_json::Value::Array(a) => a.is_empty(),
        serde_json::Value::Object(o) => o.is_empty(),
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => false,
    }
}

#[derive(Deserialize, Serialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct JBArgs<'a> {
    #[serde(default)]
    #[serde(skip_serializing_if = "str::is_empty")]
    pub action: Cow<'a, str>,
    #[serde(default)]
    #[serde(skip_serializing_if = "str::is_empty")]
    pub event: Cow<'a, str>,
    #[serde(default)]
    #[serde(skip_serializing_if = "String::is_empty")]
    pub app_id: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "object_is_empty")]
    pub options: serde_json::Value,
    #[serde(rename = "type")]
    #[serde(skip_serializing_if = "str::is_empty")]
    pub arg_type: Cow<'a, str>,
    #[serde(default)]
    #[serde(skip_serializing_if = "object_is_empty")]
    pub blob: serde_json::Value,
    #[serde(default)]
    #[serde(skip_serializing_if = "object_is_empty")]
    pub message: serde_json::Value,
    #[serde(default)]
    #[serde(skip_serializing_if = "str::is_empty")]
    pub user_id: Cow<'a, str>,
    #[serde(default)]
    #[serde(skip_serializing_if = "str::is_empty")]
    pub customer_user_id: Cow<'a, str>,
    #[serde(default)]
    #[serde(skip_serializing_if = "str::is_empty")]
    pub customer_name: Cow<'a, str>,
    #[serde(default)]
    #[serde(skip_serializing_if = "str::is_empty")]
    pub room_id: Cow<'a, str>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
}

#[instrument(skip(room_map))]
pub async fn connect_socket(
    socket: WebSocket,
    room_map: Arc<DashMap<String, Arc<Room>>>,
    host: String,
) -> eyre::Result<ConnectedSocket> {
    let (mut ws_write, mut ws_read) = socket.split();

    ws_write
        .send(Message::Text("1::".to_owned()))
        .await
        .wrap_err("Failed to send blobcast connection message: ::1")?;

    let create_room_msg = ws_read
        .next()
        .await
        .ok_or_eyre("Failed to receive CreateRoom message")?
        .wrap_err("CreateRoom message is a None")?;

    let Message::Text(create_room) = create_room_msg else {
        bail!(
            "Expected a Message::Text for create room, got: {:?}",
            create_room_msg
        );
    };

    let create_room = WSMessage::from_str(&create_room).wrap_err_with(|| {
        format!(
            "Failed to deserialize CreateRoom WS Message: {}",
            create_room
        )
    })?;

    let room_code = crate::room_id();

    let room = Arc::new(Room {
        entities: DashMap::new(),
        connections: DashMap::new(),
        room_serial: 1.into(),
        room_config: crate::JBRoom {
            app_tag: {
                let app_id = create_room
                    .message
                    .as_ref()
                    .ok_or_else(|| eyre!("CreateRoom contained no message: {:?}", create_room))?
                    .args
                    .get_args()
                    .app_id
                    .as_str();

                super::APP_TAGS
                    .get(app_id)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| {
                        tracing::warn!("No app tag found for blobcast app id: {}", app_id);
                        String::new()
                    })
            },
            app_id: create_room
                .message
                .as_ref()
                .ok_or_else(|| eyre!("CreateRoom contained no message: {:?}", create_room))?
                .args
                .get_args()
                .app_id
                .clone(),
            audience_enabled: false,
            code: room_code.clone(),
            audience_host: host.clone(),
            host,
            locked: false.into(),
            full: false.into(),
            moderation_enabled: false,
            password_required: false,
            twitch_locked: false,
            locale: Cow::Borrowed("en"),
            keepalive: false,
        },
        exit: Notify::new(),
        channel: tokio::sync::watch::channel(()).0,
    });

    room_map.insert(room_code.clone(), Arc::clone(&room));

    ws_write
        .send(Message::Text(format!(
            "5:::{{\
                \"name\": \"msg\",\
                \"args\": [\
                    {{\
                        \"type\": \"Result\",\
                        \"action\": \"CreateRoom\",\
                        \"success\": true,\
                        \"roomId\": \"{}\"\
                    }}\
                ]\
            }}",
            room_code
        )))
        .await
        .wrap_err("Failed to send result of CreateRoom")?;

    let serial = room
        .room_serial
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);

    let profile = JBProfile {
        id: serial,
        roles: crate::JBProfileRoles::Host {},
        user_id: create_room
            .message
            .as_ref()
            .ok_or_else(|| eyre!("CreateRoom contained no message: {:?}", create_room))?
            .args
            .get_args()
            .user_id
            .clone()
            .into_owned(),
        role: Role::Host,
        name: String::new(),
    };

    let client = Arc::new(Client {
        pc: 0.into(),
        profile,
        socket: Mutex::new(Some(ws_write)),
        client_type: ClientType::Blobcast,
        secret: Token::random(),
    });
    room.connections.insert(serial, Arc::clone(&client));
    Ok(ConnectedSocket {
        client,
        room,
        read_half: ws_read,
        reconnected: false,
    })
}

#[instrument(skip_all, fields(role = ?client.profile.role, id = client.profile.id))]
pub async fn handle_socket(
    mut ws_read: SplitStream<WebSocket>,
    room: Arc<Room>,
    client: Arc<Client>,
) -> eyre::Result<()> {
    'outer: loop {
        tokio::select! {
            ws_message = ws_read.next() => {
                match ws_message {
                    Some(Ok(ws_message)) => {
                        let message: WSMessage = match ws_message {
                            Message::Text(ref t) => WSMessage::from_str(t)
                                .wrap_err_with(|| format!("Failed to deserialize message: {}", t))?,
                            Message::Close(_) => break 'outer,
                            Message::Ping(d) => {
                                client.pong(d).await?;
                                continue;
                            }
                            _ => continue,
                        };
                        if let Some(ref message) = message.message {
                            process_message(&client, message, &room).await
                                .wrap_err("Failed to process message")?;
                        }
                    }
                    Some(Err(e)) => {
                        tracing::error!(?e, "Error in receiving message");
                    }
                    None => {
                        break
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(5)) => {
                client.send_ws_message(Message::Text(String::from("2:::"))).await
                    .wrap_err("Failed to send blobcast ping to client")?;
            }
            _ = room.exit.notified() => {
                break
            }
        }
        room.channel.send_replace(());
    }

    Ok(())
}

#[instrument(skip(client, room))]
async fn process_message(
    client: &Client,
    message: &JBMessage<'_>,
    room: &Room,
) -> eyre::Result<()> {
    match message.args.get_args().action.as_ref() {
        "SetRoomBlob" => {
            let entity = {
                let prev_value = room.entities.get("bc:room");
                JBEntity(
                    JBType::Object,
                    JBObject {
                        key: "bc:room".to_owned(),
                        val: JBValue::Object {
                            val: message
                                .args
                                .get_args()
                                .blob
                                .as_object()
                                .cloned()
                                .ok_or_eyre("No blob in SetRoomBlob message")?,
                        },
                        restrictions: JBRestrictions::default(),
                        version: prev_value
                            .as_ref()
                            .map(|p| p.value().1.version + 1)
                            .unwrap_or_default(),
                        from: client.profile.id.into(),
                    },
                    JBAttributes::default(),
                )
            };
            let value = JBResult::Object(&entity.1);
            for client in room.connections.iter() {
                match client.client_type {
                    ClientType::Ecast => {
                        client
                            .send_ecast(crate::ecast::ws::JBMessage {
                                pc: 0,
                                re: None,
                                result: &value,
                            })
                            .await
                            .wrap_err("Failed to send room blob to ecast client")?;
                    }
                    ClientType::Blobcast => {
                        client
                            .send_blobcast(JBMessage {
                                name: Cow::Borrowed("msg"),
                                args: JBMessageArgs::Array([JBArgs {
                                    arg_type: Cow::Borrowed("Event"),
                                    action: Cow::Borrowed("RoomBlobChanged"),
                                    room_id: Cow::Borrowed(&room.room_config.code),
                                    blob: serde_json::to_value(&entity.1.val).wrap_err_with(
                                        || {
                                            format!(
                                            "Failed to convert entity to serde_json::Value: {:?}",
                                            entity.1.val
                                        )
                                        },
                                    )?,
                                    ..Default::default()
                                }]),
                            })
                            .await
                            .wrap_err("Failed to ACK RoomBlob to blobcast host")?;
                    }
                }
            }
            room.entities.insert("bc:room".to_owned(), entity);
            client
                .send_blobcast(JBMessage {
                    name: Cow::Borrowed("msg"),
                    args: JBMessageArgs::Array([JBArgs {
                        arg_type: Cow::Borrowed("Result"),
                        action: Cow::Borrowed("SetRoomBlob"),
                        success: Some(true),
                        ..Default::default()
                    }]),
                })
                .await
                .wrap_err("Failed to send result of SetRoomBlob")?;
        }
        "SetCustomerBlob" => {
            let user_id = message.args.get_args().customer_user_id.as_ref();
            let key = format!("bc:customer:{}", user_id);
            let connection = room
                .connections
                .iter()
                .find(|c| c.profile.user_id == user_id);
            let entity = {
                let prev_value = room.entities.get(&key);
                JBEntity(
                    JBType::Object,
                    JBObject {
                        key: key.clone(),
                        val: JBValue::Object {
                            val: message
                                .args
                                .get_args()
                                .blob
                                .as_object()
                                .cloned()
                                .ok_or_eyre("No blob in SetCustomerBlob message")?,
                        },
                        restrictions: JBRestrictions::default(),
                        version: prev_value
                            .as_ref()
                            .map(|p| p.value().1.version + 1)
                            .unwrap_or_default(),
                        from: client.profile.id.into(),
                    },
                    JBAttributes {
                        locked: false.into(),
                        acl: vec![Acl {
                            interest: Interest::READABLE,
                            principle: crate::acl::Principle::Id(
                                connection.as_ref().map(|c| *c.key()).unwrap_or_default(),
                            ),
                        }],
                    },
                )
            };
            if let Some(connection) = connection {
                connection
                    .send_ecast(crate::ecast::ws::JBMessage {
                        pc: 0,
                        re: None,
                        result: &JBResult::Object(&entity.1),
                    })
                    .await
                    .wrap_err("Failed to send customer blob to ecast client")?;
            }
            room.entities.insert(key, entity);
        }
        "LockRoom" => {
            client
                .send_blobcast(JBMessage {
                    name: Cow::Borrowed("msg"),
                    args: JBMessageArgs::Array([JBArgs {
                        arg_type: Cow::Borrowed("Result"),
                        action: Cow::Borrowed("LockRoom"),
                        success: Some(true),
                        room_id: Cow::Borrowed(&room.room_config.code),
                        ..Default::default()
                    }]),
                })
                .await
                .wrap_err("Failed to send result of LockRoom to blobcast host")?;
        }
        a => bail!("Unimplemented blobcast action {:?}", a),
    }

    Ok(())
}

#[instrument(skip(socket))]
pub async fn handle_socket_proxy(
    host: String,
    socket: WebSocket,
    blobcast_req: String,
) -> eyre::Result<()> {
    let blobcast_req = blobcast_req
        .into_client_request()
        .wrap_err("blobcast_req could not be converted to a client request")?;
    let (blobcast_connection, _) = tokio_tungstenite::connect_async(blobcast_req)
        .await
        .wrap_err("Failed to connect to blobcast proxy")?;

    let (local_write, local_read) = socket.split();

    let (blobcast_write, blobcast_read) = blobcast_connection.split();

    let local_to_blobcast = local_read
        .map_err(|e: axum::Error| eyre!("local_to_blobcast stream broken: {}", e))
        .map(
            move |m| -> eyre::Result<tokio_tungstenite::tungstenite::Message> {
                let m = match m.wrap_err("local_to_blobcast message failed to be received")? {
                    axum::extract::ws::Message::Text(m) => {
                        let mut sm = m.split(":::");
                        let opcode = sm.next().unwrap();
                        let json_message = sm.next();
                        tracing::debug!(
                            role = ?Role::Host,
                            opcode = opcode,
                            message = %{
                                json_message.map(|m| -> eyre::Result<String> {
                                    if let Ok(jq) = std::process::Command::new("jq")
                                        .stdin(Stdio::piped())
                                        .stdout(Stdio::piped())
                                        .arg("-C")
                                        .spawn() {
                                            let mut jq_in = jq.stdin.ok_or_eyre("jq process has no stdin")?;
                                            let mut jq_out = jq.stdout.ok_or_eyre("jq process has no stdout")?;
                                            jq_in.write_all(m.as_bytes())
                                                .wrap_err("Failed to write to jq process")?;
                                            jq_in.write_all(b"\n")
                                                .wrap_err("Failed to write to jq process")?;
                                            drop(jq_in);
                                            let mut jm = String::new();
                                            jq_out.read_to_string(&mut jm)
                                                .wrap_err("Failed to read from jq process")?;
                                            Ok(jm)
                                        } else {
                                            Ok(m.to_owned())
                                        }
                                })
                                .unwrap_or_else(|| Ok(String::new()))
                                .wrap_err_with(|| format!("jq coloration failed for message: {:?}", json_message))?
                            },
                            "to blobcast",
                        );
                        return Ok(tokio_tungstenite::tungstenite::Message::Text(m));
                    }
                    axum::extract::ws::Message::Binary(m) => {
                        Ok(tokio_tungstenite::tungstenite::Message::Binary(m))
                    }
                    axum::extract::ws::Message::Ping(m) => {
                        Ok(tokio_tungstenite::tungstenite::Message::Ping(m))
                    }
                    axum::extract::ws::Message::Pong(m) => {
                        Ok(tokio_tungstenite::tungstenite::Message::Pong(m))
                    }
                    axum::extract::ws::Message::Close(m) => {
                        Ok(tokio_tungstenite::tungstenite::Message::Close(m.map(|f| {
                            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                                code: f.code.into(),
                                reason: f.reason,
                            }
                        })))
                    }
                };
                tracing::debug!(
                    role = ?Role::Host,
                    message = ?m,
                    "to blobcast",
                );
                m
            },
        )
        .forward(blobcast_write.sink_map_err(|e| eyre!(e)));

    let blobcast_to_local = blobcast_read
        .map_err(|e: tokio_tungstenite::tungstenite::Error| {
            eyre!("blobcast_to_local stream broken: {}", e)
        })
        .map(|m| -> eyre::Result<axum::extract::ws::Message> {
            let m = match m.wrap_err("blobcast_to_local message failed to be received")? {
                tokio_tungstenite::tungstenite::Message::Text(m) => {
                    let mut sm = m.split(":::");
                    let opcode = sm.next().unwrap();
                    let json_message = sm.next();
                    tracing::debug!(
                        role = ?Role::Host,
                        opcode = opcode,
                        message = %{
                            json_message.map(|m| -> eyre::Result<String> {
                                if let Ok(jq) = std::process::Command::new("jq")
                                    .stdin(Stdio::piped())
                                    .stdout(Stdio::piped())
                                    .arg("-C")
                                    .spawn() {
                                        let mut jq_in = jq.stdin.ok_or_eyre("jq process has no stdin")?;
                                        let mut jq_out = jq.stdout.ok_or_eyre("jq process has no stdout")?;
                                        jq_in.write_all(m.as_bytes())
                                            .wrap_err("Failed to write to jq process")?;
                                        jq_in.write_all(b"\n")
                                            .wrap_err("Failed to write to jq process")?;
                                        drop(jq_in);
                                        let mut jm = String::new();
                                        jq_out.read_to_string(&mut jm)
                                            .wrap_err("Failed to read from jq process")?;
                                        Ok(jm)
                                    } else {
                                        Ok(m.to_owned())
                                    }
                            })
                            .unwrap_or_else(|| Ok(String::new()))
                            .wrap_err_with(|| format!("jq coloration failed for message: {:?}", json_message))?
                        },
                        "blobcast to",
                    );
                    return Ok(axum::extract::ws::Message::Text(m));
                }
                tokio_tungstenite::tungstenite::Message::Binary(m) => {
                    Ok(axum::extract::ws::Message::Binary(m))
                }
                tokio_tungstenite::tungstenite::Message::Ping(m) => {
                    Ok(axum::extract::ws::Message::Ping(m))
                }
                tokio_tungstenite::tungstenite::Message::Pong(m) => {
                    Ok(axum::extract::ws::Message::Pong(m))
                }
                tokio_tungstenite::tungstenite::Message::Close(m) => {
                    Ok(axum::extract::ws::Message::Close(m.map(|f| {
                        axum::extract::ws::CloseFrame {
                            code: f.code.into(),
                            reason: f.reason,
                        }
                    })))
                }
                tokio_tungstenite::tungstenite::Message::Frame(f) => bail!("Failed to proxy unimplemented raw frame: {:?}", f),
            };
            tracing::debug!(
                role = ?Role::Host,
                message = ?m,
                "blobcast to",
            );
            m
        })
        .forward(local_write.sink_map_err(|e| eyre!(e)));

    tokio::pin!(local_to_blobcast, blobcast_to_local);

    tokio::select! {
        r = local_to_blobcast => r,
        r = blobcast_to_local => r
    }
}
