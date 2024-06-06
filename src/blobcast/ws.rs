use std::{
    borrow::Cow,
    io::{Read, Write},
    process::Stdio,
    str::FromStr,
    sync::{atomic::AtomicI64, Arc},
    time::Duration,
};

use crate::{
    acl::{Acl, Role},
    ecast::ws::JBResult,
    entity::{JBAttributes, JBCountGroup, JBEntity, JBObject, JBRestrictions, JBType, JBValue},
    Client, ClientType, ConnectedSocket, JBProfile, Room, Token,
};

use axum::extract::ws::{Message, WebSocket};
use color_eyre::eyre::{self, bail, eyre, OptionExt, WrapErr};
use dashmap::DashMap;
use futures_util::{stream::SplitStream, SinkExt, StreamExt, TryStreamExt};
use indexmap::IndexMap;
use serde::{de::IgnoredAny, Deserialize, Serialize};
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

#[derive(Deserialize, Debug)]
#[serde(tag = "name", content = "args")]
#[serde(rename_all = "camelCase")]
pub enum JBMessage<'a> {
    Msg(JBMessageArgs<'a>),
}

#[derive(Serialize, Debug)]
#[serde(tag = "name", content = "args")]
#[serde(rename_all = "camelCase")]
pub enum JBResponse<'a> {
    Msg([JBResponseArgs<'a>; 1]),
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
#[serde(tag = "type")]
pub enum JBMessageArgs<'a> {
    #[serde(rename_all = "camelCase")]
    Action {
        app_id: Cow<'a, str>,
        user_id: Cow<'a, str>,
        #[serde(flatten)]
        action: JBAction<'a>,
    },
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
#[serde(tag = "type")]
pub enum JBResponseArgs<'a> {
    #[serde(rename_all = "camelCase")]
    Event {
        room_id: Cow<'a, str>,
        #[serde(flatten)]
        event: JBEvent<'a>,
    },
    Result {
        #[serde(flatten)]
        result: JBResultAction<'a>,
        success: bool,
    },
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
#[serde(tag = "action")]
pub enum JBAction<'a> {
    #[serde(rename_all = "camelCase")]
    CreateRoom { options: serde_json::Value },
    #[serde(rename_all = "camelCase")]
    SetRoomBlob {
        blob: serde_json::Map<String, serde_json::Value>,
        room_id: Cow<'a, str>,
    },
    #[serde(rename_all = "camelCase")]
    SetCustomerBlob {
        blob: serde_json::Map<String, serde_json::Value>,
        customer_user_id: Cow<'a, str>,
        room_id: Cow<'a, str>,
    },
    #[serde(rename_all = "camelCase")]
    StartSession {
        #[serde(flatten)]
        module: JBSessionModuleWithArgs,
        name: Cow<'a, str>,
        room_id: Cow<'a, str>,
    },
    #[serde(rename_all = "camelCase")]
    StopSession {
        module: JBSessionModule,
        name: Cow<'a, str>,
        room_id: Cow<'a, str>,
    },
    #[serde(rename_all = "camelCase")]
    GetSessionStatus {
        module: JBSessionModule,
        name: Cow<'a, str>,
        room_id: Cow<'a, str>,
    },
    #[serde(rename_all = "camelCase")]
    LockRoom { room_id: Cow<'a, str> },
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
#[serde(tag = "action")]
pub enum JBResultAction<'a> {
    #[serde(rename_all = "camelCase")]
    CreateRoom { room_id: Cow<'a, str> },
    #[serde(rename_all = "camelCase")]
    SetRoomBlob {},
    #[serde(rename_all = "camelCase")]
    LockRoom { room_id: Cow<'a, str> },
    #[serde(rename_all = "camelCase")]
    GetSessionStatus {
        #[serde(flatten)]
        module: JBSessionModuleWithResponse<'a>,
        name: Cow<'a, str>,
    },
    #[serde(rename_all = "camelCase")]
    StartSession {
        #[serde(flatten)]
        module: JBSessionModuleWithResponse<'a>,
        name: Cow<'a, str>,
    },
    #[serde(rename_all = "camelCase")]
    StopSession {
        #[serde(flatten)]
        module: JBSessionModuleWithResponse<'a>,
        name: Cow<'a, str>,
    },
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "module", content = "options")]
pub enum JBSessionModuleWithArgs {
    Audience(IgnoredAny),
    Vote(JBCountGroup),
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "module", content = "response")]
pub enum JBSessionModuleWithResponse<'a> {
    Audience(&'a JBValue),
    Vote(&'a IndexMap<String, AtomicI64>),
}

impl From<JBSessionModuleWithArgs> for JBSessionModule {
    fn from(value: JBSessionModuleWithArgs) -> Self {
        match value {
            JBSessionModuleWithArgs::Audience(_) => Self::Audience,
            JBSessionModuleWithArgs::Vote(_) => Self::Vote,
        }
    }
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub enum JBSessionModule {
    Audience,
    Vote,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
#[serde(tag = "event")]
pub enum JBEvent<'a> {
    #[serde(rename_all = "camelCase")]
    RoomBlobChanged { blob: serde_json::Value },
    #[serde(rename_all = "camelCase")]
    CustomerJoinedRoom {
        customer_user_id: Cow<'a, str>,
        customer_name: Cow<'a, str>,
        options: JBCustomerOptions<'a>,
    },
    #[serde(rename_all = "camelCase")]
    CustomerMessage {
        user_id: Cow<'a, str>,
        message: serde_json::Value,
    },
}

#[derive(Deserialize, Serialize, Debug)]
pub struct JBCustomerOptions<'a> {
    pub roomcode: Cow<'a, str>,
    pub name: Cow<'a, str>,
    pub email: Cow<'a, str>,
    pub phone: Cow<'a, str>,
}

#[instrument(skip(room_map, socket))]
pub async fn connect_socket(
    socket: WebSocket,
    room_map: Arc<DashMap<String, Arc<Room>>>,
    host: String,
) -> eyre::Result<ConnectedSocket> {
    let (mut ws_write, mut ws_read) = socket.split();

    ws_write
        .send(Message::Text("1::".to_owned()))
        .await
        .wrap_err("Failed to send blobcast connection message: 1::")?;

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

    let create_room = WSMessage::from_str(&create_room)
        .wrap_err("Failed to deserialize CreateRoom WS Message")?;

    let JBMessage::Msg(JBMessageArgs::Action {
        app_id,
        user_id,
        action: JBAction::CreateRoom { .. },
        ..
    }) = create_room
        .message
        .as_ref()
        .ok_or_else(|| eyre!("CreateRoom contained no message: {:?}", create_room))?
    else {
        bail!("Expected CreateRoom messages, got: {:?}", create_room);
    };

    let room_code = crate::room_id();

    let room = Arc::new(Room {
        entities: DashMap::new(),
        connections: DashMap::new(),
        room_serial: 1.into(),
        room_config: crate::JBRoom {
            app_tag: {
                super::APP_TAGS
                    .get(app_id)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| {
                        tracing::warn!("No app tag found for blobcast app id: {}", app_id);
                        String::new()
                    })
            },
            app_id: app_id.clone().into_owned(),
            audience_enabled: true,
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

    let serial = room
        .room_serial
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);

    let profile = JBProfile {
        id: serial,
        roles: crate::JBProfileRoles::Host {},
        user_id: user_id.clone().into_owned(),
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

    client
        .send_blobcast(JBResponse::Msg([JBResponseArgs::Result {
            result: JBResultAction::CreateRoom {
                room_id: Cow::Owned(room_code),
            },
            success: true,
        }]))
        .await
        .wrap_err("Failed to send host the room code")?;

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
                        if let Some(message) = message.message {
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
async fn process_message(client: &Client, message: JBMessage<'_>, room: &Room) -> eyre::Result<()> {
    match message {
        JBMessage::Msg(JBMessageArgs::Action {
            app_id,
            user_id,
            action,
        }) => match action {
            JBAction::SetRoomBlob { blob, room_id } => {
                let entity = {
                    let prev_value = room.entities.get("bc:room");
                    JBEntity(
                        JBType::Object,
                        JBObject {
                            key: "bc:room".to_owned(),
                            val: JBValue::Object { val: blob },
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
                                .send_blobcast(JBResponse::Msg(
                                     [JBResponseArgs::Event {
                                        room_id: Cow::Borrowed(&room.room_config.code),
                                        event: JBEvent::RoomBlobChanged {
                                            blob: serde_json::to_value(&entity.1.val)
                                                .wrap_err_with(|| {
                                                    format!(
                                                        "Failed to convert entity to serde_json::Value: {:?}",
                                                        entity.1.val
                                                    )
                                                })?,
                                        },
                                    }],
                                ))
                                .await
                                .wrap_err("Failed to ACK RoomBlob to blobcast host")?;
                        }
                    }
                }
                room.entities.insert("bc:room".to_owned(), entity);
                client
                    .send_blobcast(JBResponse::Msg([JBResponseArgs::Result {
                        result: JBResultAction::SetRoomBlob {},
                        success: true,
                    }]))
                    .await
                    .wrap_err("Failed to send result of SetRoomBlob")?;
            }
            JBAction::SetCustomerBlob {
                blob,
                customer_user_id,
                room_id,
            } => {
                let key = format!("bc:customer:{}", customer_user_id);
                let connection = room
                    .connections
                    .iter()
                    .find(|c| c.profile.user_id == customer_user_id);
                let entity = {
                    let prev_value = room.entities.get(&key);
                    JBEntity(
                        JBType::Object,
                        JBObject {
                            key: key.clone(),
                            val: JBValue::Object { val: blob },
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
            JBAction::LockRoom { room_id } => {
                room.room_config
                    .locked
                    .store(true, std::sync::atomic::Ordering::Release);
                client
                    .send_blobcast(JBResponse::Msg([JBResponseArgs::Result {
                        result: JBResultAction::LockRoom {
                            room_id: Cow::Borrowed(&room.room_config.code),
                        },
                        success: true,
                    }]))
                    .await
                    .wrap_err("Failed to send result of LockRoom to blobcast host")?;
            }
            JBAction::CreateRoom { .. } => {
                bail!("Got CreateRoom message, but room is already created")
            }
            JBAction::StartSession {
                module,
                name,
                room_id,
            } => match module {
                JBSessionModuleWithArgs::Audience(_) => {
                    let audience = room.entities.get("audience");
                    client
                        .send_blobcast(JBResponse::Msg([JBResponseArgs::Result {
                            result: JBResultAction::StartSession {
                                module: JBSessionModuleWithResponse::Audience(
                                    audience.as_ref().map(|e| &e.1.val).unwrap_or(
                                        &JBValue::AudiencePnCounter {
                                            count: AtomicI64::new(0),
                                        },
                                    ),
                                ),
                                name,
                            },
                            success: true,
                        }]))
                        .await
                        .wrap_err("Failed to notify blobcast host of room audience connections")?;
                }
                JBSessionModuleWithArgs::Vote(count_group) => {
                    let entity = {
                        JBEntity(
                            JBType::AudienceCountGroup,
                            JBObject {
                                key: name.clone().into_owned(),
                                val: JBValue::AudienceCountGroup(count_group),
                                restrictions: JBRestrictions::default(),
                                version: 0,
                                from: client.profile.id.into(),
                            },
                            JBAttributes::default(),
                        )
                    };
                    let value = JBResult::AudienceCountGroup(&entity.1);
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
                                    .wrap_err("Failed to send count group to ecast client")?;
                            }
                            ClientType::Blobcast => {}
                        }
                    }
                    let JBValue::AudienceCountGroup(ref cg) = entity.1.val else {
                        unreachable!()
                    };
                    client
                        .send_blobcast(JBResponse::Msg([JBResponseArgs::Result {
                            result: JBResultAction::StartSession {
                                module: JBSessionModuleWithResponse::Vote(&cg.choices),
                                name: name.clone(),
                            },
                            success: true,
                        }]))
                        .await
                        .wrap_err("Failed to send result of StartSession")?;
                    room.entities.insert(name.into_owned(), entity);
                }
            },
            JBAction::StopSession {
                module,
                name,
                room_id,
            } => match module {
                JBSessionModule::Audience => {
                    let audience = room.entities.remove("audience");
                    client
                        .send_blobcast(JBResponse::Msg([JBResponseArgs::Result {
                            result: JBResultAction::StopSession {
                                module: JBSessionModuleWithResponse::Audience(
                                    audience.as_ref().map(|(_, e)| &e.1.val).unwrap_or(
                                        &JBValue::AudiencePnCounter {
                                            count: AtomicI64::new(0),
                                        },
                                    ),
                                ),
                                name,
                            },
                            success: true,
                        }]))
                        .await
                        .wrap_err("Failed to notify blobcast host of room audience connections")?;
                }
                JBSessionModule::Vote => {
                    let entity = room.entities.remove(name.as_ref());
                    if let Some((_, entity)) = entity {
                        let JBValue::AudienceCountGroup(ref cg) = entity.1.val else {
                            unreachable!()
                        };
                        client
                            .send_blobcast(JBResponse::Msg([JBResponseArgs::Result {
                                result: JBResultAction::StopSession {
                                    module: JBSessionModuleWithResponse::Vote(&cg.choices),
                                    name: name.clone(),
                                },
                                success: true,
                            }]))
                            .await
                            .wrap_err("Failed to send result of StopSession")?;
                    }
                }
            },
            JBAction::GetSessionStatus {
                module,
                name,
                room_id,
            } => match module {
                JBSessionModule::Audience => {
                    let audience = room.entities.get("audience");
                    client
                        .send_blobcast(JBResponse::Msg([JBResponseArgs::Result {
                            result: JBResultAction::GetSessionStatus {
                                module: JBSessionModuleWithResponse::Audience(
                                    audience.as_ref().map(|e| &e.1.val).unwrap_or(
                                        &JBValue::AudiencePnCounter {
                                            count: AtomicI64::new(0),
                                        },
                                    ),
                                ),
                                name,
                            },
                            success: true,
                        }]))
                        .await
                        .wrap_err("Failed to notify blobcast host of room audience connections")?;
                }
                JBSessionModule::Vote => {
                    let count_group = room.entities.get(name.as_ref());

                    if let Some(count_group) = count_group {
                        if let JBValue::AudienceCountGroup(JBCountGroup { ref choices, .. }) =
                            count_group.value().1.val
                        {
                            client
                                .send_blobcast(JBResponse::Msg([JBResponseArgs::Result {
                                    result: JBResultAction::GetSessionStatus {
                                        module: JBSessionModuleWithResponse::Vote(choices),
                                        name,
                                    },
                                    success: true,
                                }]))
                                .await
                                .wrap_err(
                                    "Failed to notify blobcast host of room audience connections",
                                )?;
                        }
                    }
                }
            },
        },
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
