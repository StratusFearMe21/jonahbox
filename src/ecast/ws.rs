use std::{
    borrow::Cow,
    io::{Read, Write},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use axum::extract::{
    ws::{Message, WebSocket},
    Query,
};
use color_eyre::eyre::{self, bail, eyre, Context, OptionExt};
use dashmap::DashMap;
use futures_util::{stream::SplitStream, SinkExt, StreamExt, TryStreamExt};
use serde::{de::IgnoredAny, ser::SerializeMap, Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::instrument;

use crate::{
    acl::{Acl, Role},
    blobcast::ws::{JBCustomerOptions, JBEvent, JBResponseArgs},
    entity::{
        JBAttributes, JBCountGroup, JBDoodle, JBEntity, JBLine, JBObject, JBRestrictions, JBType,
        JBValue,
    },
    Client, ClientType, ConnectedSocket, Connections, DoodleConfig, JBProfile, JBProfileRoles,
    Room, Token,
};

use super::WSQuery;

#[derive(Serialize, Debug)]
pub struct JBMessage<'a> {
    pub pc: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub re: Option<u64>,
    #[serde(flatten)]
    pub result: &'a JBResult<'a>,
}

#[derive(Serialize, Debug)]
#[serde(tag = "opcode", content = "result")]
pub enum JBResult<'a> {
    #[serde(rename = "client/welcome")]
    ClientWelcome(ClientWelcome<'a>),
    #[serde(rename = "client/connected")]
    ClientConnected(ClientConnected<'a>),
    #[serde(rename = "text")]
    Text(&'a JBObject),
    #[serde(rename = "number")]
    Number(&'a JBObject),
    #[serde(rename = "object")]
    Object(&'a JBObject),
    #[serde(rename = "doodle")]
    Doodle(&'a JBObject),
    #[serde(rename = "audience/pn-counter")]
    AudiencePnCounter(&'a JBObject),
    #[serde(rename = "audience/g-counter")]
    AudienceGCounter(&'a JBObject),
    #[serde(rename = "audience/count-group")]
    AudienceCountGroup(&'a JBObject),
    #[serde(rename = "doodle/line")]
    DoodleLine {
        key: Cow<'a, str>,
        from: i64,
        val: &'a JBLine,
    },
    #[serde(rename = "room/get-audience")]
    RoomGetAudience { connections: i64 },
    #[serde(rename = "client/send")]
    ClientSend(JBClientSendParams),
    #[serde(rename = "lock")]
    Lock { key: Cow<'a, str>, from: i64 },
    #[serde(rename = "room/lock")]
    RoomLock {},
    #[serde(rename = "error")]
    Error(&'static str),
    #[serde(rename = "ok")]
    Ok {},
}

#[derive(Deserialize, Debug)]
struct WSMessage {
    #[serde(flatten)]
    params: JBParams,
    seq: u64,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "opcode", content = "params")]
enum JBParams {
    #[serde(rename = "text/create")]
    TextCreate(JBCreateParams),
    #[serde(rename = "text/set")]
    TextSet(JBCreateParams),
    #[serde(rename = "text/update")]
    TextUpdate(JBCreateParams),
    #[serde(rename = "text/get")]
    TextGet(JBKeyParam),
    #[serde(rename = "number/create")]
    NumberCreate(JBCreateParams),
    #[serde(rename = "number/set")]
    NumberSet(JBCreateParams),
    #[serde(rename = "number/update")]
    NumberUpdate(JBCreateParams),
    #[serde(rename = "number/get")]
    NumberGet(JBKeyParam),
    #[serde(rename = "number/increment")]
    NumberIncrement(JBKeyParam),
    #[serde(rename = "number/decrement")]
    NumberDecrement(JBKeyParam),
    #[serde(rename = "object/create")]
    ObjectCreate(JBCreateParams),
    #[serde(rename = "object/set")]
    ObjectSet(JBCreateParams),
    #[serde(rename = "object/update")]
    ObjectUpdate(JBCreateParams),
    #[serde(rename = "object/get")]
    ObjectGet(JBKeyParam),
    #[serde(rename = "audience/g-counter/create")]
    AudienceGCounterCreate(JBCreateParams),
    #[serde(rename = "audience/g-counter/get")]
    AudienceGCounterGet(JBKeyParam),
    #[serde(rename = "audience/g-counter/increment")]
    AudienceGCounterIncrement(JBCounterIncrementParams),
    #[serde(rename = "audience/g-counter/decrement")]
    AudienceGCounterDecrement(JBCounterIncrementParams),
    #[serde(rename = "audience/pn-counter/create")]
    AudiencePnCounterCreate(JBCreateParams),
    #[serde(rename = "audience/pn-counter/get")]
    AudiencePnCounterGet(JBKeyParam),
    #[serde(rename = "audience/pn-counter/increment")]
    AudiencePnCounterIncrement(JBCounterIncrementParams),
    #[serde(rename = "audience/pn-counter/decrement")]
    AudiencePnCounterDecrement(JBCounterIncrementParams),
    #[serde(rename = "audience/count-group/create")]
    AudienceCountGroupCreate(JBCreateParams),
    #[serde(rename = "audience/count-group/get")]
    AudienceCountGroupGet(JBKeyParam),
    #[serde(rename = "audience/count-group/increment")]
    AudienceCountGroupIncrement(JBCountGroupIncrementParams),
    #[serde(rename = "doodle/create")]
    DoodleCreate(JBCreateParams),
    #[serde(rename = "doodle/set")]
    DoodleSet(JBCreateParams),
    #[serde(rename = "doodle/update")]
    DoodleUpdate(JBCreateParams),
    #[serde(rename = "doodle/get")]
    DoodleGet(JBKeyParam),
    #[serde(rename = "doodle/stroke")]
    DoodleStroke(JBKeyWithLine),
    #[serde(rename = "client/send")]
    ClientSend(JBClientSendParams),
    #[serde(rename = "room/exit")]
    RoomExit(IgnoredAny),
    #[serde(rename = "room/get-audience")]
    RoomGetAudience(IgnoredAny),
    #[serde(rename = "room/lock")]
    RoomLock(IgnoredAny),
    #[serde(rename = "lock")]
    Lock(JBKeyParam),
    #[serde(rename = "drop")]
    Drop(JBKeyParam),
    #[serde(untagged)]
    Other(serde_json::Value),
}

impl JBParams {
    fn scope(&self) -> Option<JBType> {
        match self {
            Self::TextCreate(_) => Some(JBType::Text),
            Self::TextSet(_) => Some(JBType::Text),
            Self::TextUpdate(_) => Some(JBType::Text),
            Self::TextGet(_) => Some(JBType::Text),
            Self::NumberCreate(_) => Some(JBType::Number),
            Self::NumberSet(_) => Some(JBType::Number),
            Self::NumberUpdate(_) => Some(JBType::Number),
            Self::NumberGet(_) => Some(JBType::Number),
            Self::NumberIncrement(_) => Some(JBType::Number),
            Self::NumberDecrement(_) => Some(JBType::Number),
            Self::ObjectCreate(_) => Some(JBType::Object),
            Self::ObjectSet(_) => Some(JBType::Object),
            Self::ObjectUpdate(_) => Some(JBType::Object),
            Self::ObjectGet(_) => Some(JBType::Object),
            Self::DoodleCreate(_) => Some(JBType::Doodle),
            Self::DoodleSet(_) => Some(JBType::Doodle),
            Self::DoodleUpdate(_) => Some(JBType::Doodle),
            Self::DoodleGet(_) => Some(JBType::Doodle),
            Self::DoodleStroke(_) => Some(JBType::Doodle),
            Self::AudienceGCounterCreate(_) => Some(JBType::AudienceGCounter),
            Self::AudienceGCounterGet(_) => Some(JBType::AudienceGCounter),
            Self::AudienceGCounterIncrement(_) => Some(JBType::AudienceGCounter),
            Self::AudienceGCounterDecrement(_) => Some(JBType::AudienceGCounter),
            Self::AudiencePnCounterCreate(_) => Some(JBType::AudiencePnCounter),
            Self::AudiencePnCounterIncrement(_) => Some(JBType::AudiencePnCounter),
            Self::AudiencePnCounterDecrement(_) => Some(JBType::AudiencePnCounter),
            Self::AudiencePnCounterGet(_) => Some(JBType::AudiencePnCounter),
            Self::AudienceCountGroupCreate(_) => Some(JBType::AudienceCountGroup),
            Self::AudienceCountGroupGet(_) => Some(JBType::AudienceCountGroup),
            Self::AudienceCountGroupIncrement(_) => Some(JBType::AudienceCountGroup),
            Self::ClientSend(_) => None,
            Self::RoomExit(_) => None,
            Self::RoomGetAudience(_) => None,
            Self::RoomLock(_) => None,
            Self::Lock(_) => None,
            Self::Drop(_) => None,
            Self::Other(_) => None,
        }
    }
}

#[derive(Deserialize, Debug)]
struct JBCreateParams {
    key: String,
    #[serde(default = "Acl::default_vec")]
    acl: Vec<Acl>,
    #[serde(flatten)]
    val: Option<CreateValue>,
    #[serde(flatten)]
    restrictions: JBRestrictions,
    #[serde(flatten)]
    doodle: JBDoodle,
    #[serde(flatten)]
    count_group: JBCountGroup,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
enum CreateValue {
    Val(serde_json::Value),
    Count(i64),
}

impl Default for CreateValue {
    fn default() -> Self {
        CreateValue::Val(serde_json::Value::Null)
    }
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct JBKeyWithLine {
    key: String,
    #[serde(flatten)]
    line: JBLine,
}

#[derive(Deserialize, Debug)]
struct JBKeyParam {
    key: String,
}

#[derive(Deserialize, Debug)]
struct JBCounterIncrementParams {
    key: String,
    times: i64,
}

#[derive(Deserialize, Debug)]
struct JBCountGroupIncrementParams {
    name: String,
    vote: String,
    times: i64,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct JBClientSendParams {
    #[serde(rename = "from")]
    _from: i64,
    to: i64,
    body: serde_json::Value,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ClientWelcome<'a> {
    id: i64,
    secret: Token,
    reconnect: bool,
    device_id: Cow<'static, str>,
    entities: GetEntities<'a>,
    here: GetHere<'a>,
    profile: &'a JBProfile,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ClientConnected<'a> {
    id: i64,
    user_id: &'a str,
    name: &'a str,
    role: Role,
    reconnect: bool,
    profile: &'a JBProfile,
}

#[instrument]
pub async fn connect_socket(
    socket: WebSocket,
    Query(url_query): Query<WSQuery>,
    room: Arc<Room>,
) -> eyre::Result<ConnectedSocket> {
    let (ws_write, ws_read) = socket.split();

    let (reconnected, client): (bool, Arc<Client>) = {
        if let Some(profile) = room.connections.get(&url_query.id) {
            *profile.value().socket.lock().await = Some(ws_write);
            (true, Arc::clone(profile.value()))
        } else {
            let serial;
            let profile = match url_query.role {
                Role::Host => {
                    serial = room
                        .room_serial
                        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    JBProfile {
                        id: serial,
                        roles: JBProfileRoles::Host {},
                        user_id: url_query.user_id.clone(),
                        role: url_query.role,
                        name: url_query.name,
                    }
                }
                Role::Player => {
                    serial = room
                        .room_serial
                        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    JBProfile {
                        id: serial,
                        roles: JBProfileRoles::Player {
                            name: url_query.name.clone(),
                        },
                        user_id: url_query.user_id.clone(),
                        role: url_query.role,
                        name: url_query.name,
                    }
                }
                Role::Audience => {
                    serial = match room
                        .entities
                        .entry("audience".to_owned())
                        .or_insert_with(|| {
                            JBEntity(
                                JBType::AudiencePnCounter,
                                JBObject {
                                    key: "audience".to_owned(),
                                    val: JBValue::AudiencePnCounter { count: 0.into() },
                                    restrictions: JBRestrictions::default(),
                                    version: 0,
                                    from: 0.into(),
                                },
                                JBAttributes::default(),
                            )
                        })
                        .value()
                        .1
                        .val
                    {
                        JBValue::AudiencePnCounter { ref count } => {
                            (count.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1) * 100
                        }
                        _ => 100,
                    };
                    JBProfile {
                        id: serial,
                        roles: JBProfileRoles::None,
                        user_id: url_query.user_id,
                        role: Role::Audience,
                        name: url_query.name,
                    }
                }
                Role::Moderator => bail!("Unimplemented role: {:?}", url_query.role),
            };

            let profile = Arc::new(Client {
                pc: 0.into(),
                profile,
                socket: Mutex::new(Some(ws_write)),
                client_type: ClientType::Ecast,
                secret: url_query.secret.unwrap_or_else(|| Token::random()),
            });
            room.connections.insert(serial, Arc::clone(&profile));
            (false, profile)
        }
    };

    Ok(ConnectedSocket {
        client,
        room,
        read_half: ws_read,
        reconnected,
    })
}

#[instrument(skip_all, fields(role = ?client.profile.role, id = client.profile.id))]
pub async fn handle_socket(
    client: Arc<Client>,
    room: Arc<Room>,
    reconnect: bool,
    mut ws_read: SplitStream<WebSocket>,
    doodle_config: &DoodleConfig,
) -> eyre::Result<()> {
    client
        .send_ecast(JBMessage {
            pc: 0,
            re: None,
            result: &JBResult::ClientWelcome(ClientWelcome {
                id: client.profile.id,
                secret: client.secret,
                reconnect,
                device_id: Cow::Borrowed("0000000000.0000000000000000000000"),
                entities: GetEntities {
                    entities: &room.entities,
                    role: client.profile.role,
                    id: client.profile.id,
                },
                here: GetHere(&room.connections, client.profile.id),
                profile: &client.profile,
            }),
        })
        .await
        .wrap_err("Failed to send ecast client/welcome to client")?;

    if client.profile.role == Role::Player {
        if let Some(host) = room.connections.get(&1) {
            match host.value().client_type {
                ClientType::Blobcast if !reconnect => {
                    host.value()
                        .send_blobcast(crate::blobcast::ws::JBResponse::Msg([
                            JBResponseArgs::Event {
                                room_id: Cow::Borrowed(&room.room_config.code),
                                event: JBEvent::CustomerJoinedRoom {
                                    customer_user_id: Cow::Borrowed(&client.profile.user_id),
                                    customer_name: Cow::Borrowed(&client.profile.name),
                                    options: JBCustomerOptions {
                                        roomcode: Cow::Borrowed(""),
                                        name: Cow::Borrowed(&client.profile.name),
                                        email: Cow::Borrowed(""),
                                        phone: Cow::Borrowed(""),
                                    },
                                },
                            },
                        ]))
                        .await
                        .wrap_err("Failed to send blobcast CustomerJoinedRoom to host")?;
                }
                ClientType::Ecast => {
                    let client_connected = JBResult::ClientConnected(ClientConnected {
                        id: client.profile.id,
                        user_id: &client.profile.user_id,
                        name: &client.profile.name,
                        role: client.profile.role,
                        reconnect,
                        profile: &client.profile,
                    });

                    host.value()
                        .send_ecast(JBMessage {
                            pc: 0,
                            re: None,
                            result: &client_connected,
                        })
                        .await
                        .wrap_err("Failed to send ecast client/connected to host")?;
                }
                _ => {}
            }
        }
    }

    'outer: loop {
        tokio::select! {
            ws_message = ws_read.next() => {
                match ws_message {
                    Some(Ok(ws_message)) => {
                        let message: WSMessage = match ws_message {
                            Message::Text(ref t) => serde_json::from_str(t).wrap_err_with(|| format!("Failed to deserialize WSMessage: {}", t))?,
                            Message::Close(_) => break 'outer,
                            Message::Ping(d) => {
                                client.pong(d).await?;
                                continue;
                            }
                            _ => continue,
                        };
                        process_message(&client, message, &room, doodle_config).await
                            .wrap_err("Failed to process ecast message")?;
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
                client.ping(b"jackbox".to_vec()).await?;
            }
            _ = room.exit.notified() => {
                break
            }
        }
        room.channel.send_replace(());
    }

    Ok(())
}

#[instrument(skip(client, room, doodle_config))]
async fn process_message(
    client: &Client,
    message: WSMessage,
    room: &Room,
    doodle_config: &DoodleConfig,
) -> eyre::Result<()> {
    let jb_type = message.params.scope();
    match message.params {
        JBParams::TextCreate(params)
        | JBParams::TextSet(params)
        | JBParams::TextUpdate(params)
        | JBParams::NumberCreate(params)
        | JBParams::NumberSet(params)
        | JBParams::NumberUpdate(params)
        | JBParams::ObjectCreate(params)
        | JBParams::ObjectSet(params)
        | JBParams::ObjectUpdate(params)
        | JBParams::DoodleCreate(params)
        | JBParams::DoodleSet(params)
        | JBParams::DoodleUpdate(params)
        | JBParams::AudienceGCounterCreate(params)
        | JBParams::AudiencePnCounterCreate(params)
        | JBParams::AudienceCountGroupCreate(params) => {
            let jb_type = jb_type.unwrap();
            let entity = {
                let prev_value = room.entities.get(&params.key);
                let has_been_created = prev_value.is_some();
                let is_unlocked = prev_value.as_ref().is_some_and(|pv| {
                    !pv.value()
                        .2
                        .locked
                        .load(std::sync::atomic::Ordering::Acquire)
                });
                let has_perms = prev_value.as_ref().is_some_and(|p| {
                    p.value()
                        .2
                        .perms(client.profile.role, client.profile.id)
                        .is_some_and(|i| i.is_writable())
                });
                if !(has_been_created || is_unlocked || has_perms)
                    && client.profile.role != Role::Host
                {
                    tracing::error!(acl = ?prev_value.as_ref().map(|pv| pv.value().2.acl.as_slice()), has_been_created, is_unlocked, has_perms, "Returned to sender");
                    client
                        .send_ecast(JBMessage {
                            pc: 0,
                            re: None,
                            result: &JBResult::Error("Permission denied"),
                        })
                        .await
                        .wrap_err("Failed to send ecast error to client")?;

                    return Ok(());
                }
                JBEntity(
                    jb_type,
                    JBObject {
                        key: params.key.clone(),
                        val: match jb_type {
                            JBType::Text => match params
                                .val
                                .unwrap_or_else(|| CreateValue::default())
                            {
                                CreateValue::Val(serde_json::Value::String(s)) => {
                                    JBValue::Text { val: s }
                                }
                                CreateValue::Val(serde_json::Value::Null) => {
                                    JBValue::None { val: None }
                                }
                                val => {
                                    bail!("create/set/get message had invalid text type: {:?}", val)
                                }
                            },
                            JBType::Number => match params
                                .val
                                .unwrap_or_else(|| CreateValue::default())
                            {
                                CreateValue::Val(serde_json::Value::Number(n)) => JBValue::Number {
                                    val: n.as_f64().unwrap(),
                                },
                                CreateValue::Val(serde_json::Value::Null) => {
                                    JBValue::None { val: None }
                                }
                                val => {
                                    bail!(
                                        "create/set/get message had invalid number type: {:?}",
                                        val
                                    )
                                }
                            },
                            JBType::Object => {
                                match params.val.unwrap_or_else(|| CreateValue::default()) {
                                    CreateValue::Val(serde_json::Value::Object(o)) => {
                                        JBValue::Object { val: o }
                                    }
                                    CreateValue::Val(serde_json::Value::Null) => {
                                        JBValue::None { val: None }
                                    }
                                    val => {
                                        bail!(
                                            "create/set/get message had invalid object type: {:?}",
                                            val
                                        )
                                    }
                                }
                            }
                            JBType::AudienceGCounter => {
                                match params.val.unwrap_or_else(|| CreateValue::default()) {
                                    CreateValue::Count(c) => {
                                        JBValue::AudienceGCounter { count: c.into() }
                                    }
                                    val => {
                                        bail!(
                                            "create/set/get message had invalid object type: {:?}",
                                            val
                                        )
                                    }
                                }
                            }
                            JBType::AudiencePnCounter => {
                                match params.val.unwrap_or_else(|| CreateValue::default()) {
                                    CreateValue::Count(c) => {
                                        JBValue::AudiencePnCounter { count: c.into() }
                                    }
                                    val => {
                                        bail!(
                                            "create/set/get message had invalid object type: {:?}",
                                            val
                                        )
                                    }
                                }
                            }
                            JBType::Doodle => JBValue::Doodle { val: params.doodle },
                            JBType::AudienceCountGroup => {
                                JBValue::AudienceCountGroup(params.count_group)
                            }
                        },
                        restrictions: params.restrictions,
                        version: prev_value
                            .as_ref()
                            .map(|p| p.value().1.version + 1)
                            .unwrap_or_default(),
                        from: client.profile.id.into(),
                    },
                    JBAttributes {
                        locked: false.into(),
                        acl: prev_value
                            .map(|pv| pv.value().2.acl.clone())
                            .unwrap_or(params.acl),
                    },
                )
            };
            let value = match jb_type {
                JBType::Text => JBResult::Text(&entity.1),
                JBType::Number => JBResult::Number(&entity.1),
                JBType::Object => JBResult::Object(&entity.1),
                JBType::Doodle => JBResult::Doodle(&entity.1),
                JBType::AudiencePnCounter => JBResult::AudiencePnCounter(&entity.1),
                JBType::AudienceGCounter => JBResult::AudienceGCounter(&entity.1),
                JBType::AudienceCountGroup => JBResult::AudienceCountGroup(&entity.1),
            };
            for client in room
                .connections
                .iter()
                .filter(|c| c.profile.id != client.profile.id)
                .filter(|c| {
                    entity
                        .2
                        .perms(c.profile.role, c.profile.id)
                        .is_some_and(|pv| pv.is_readable())
                })
            {
                let message = JBMessage {
                    pc: 0,
                    re: None,
                    result: &value,
                };
                client
                    .send_ecast(message)
                    .await
                    .wrap_err("Failed to send ecast client an entity")?;
            }
            room.entities.insert(params.key, entity);
            client
                .send_ecast(JBMessage {
                    pc: 0,
                    re: Some(message.seq),
                    result: &JBResult::Ok {},
                })
                .await
                .wrap_err(
                    "Failed to send the result of create/set/update opcode to ecast client",
                )?;
        }
        JBParams::DoodleStroke(params) => {
            if let Some(mut entity) = room.entities.get_mut(&params.key) {
                {
                    let line_value = JBResult::DoodleLine {
                        key: Cow::Borrowed(&params.key),
                        from: client.profile.id,
                        val: &params.line,
                    };

                    for client in room
                        .connections
                        .iter()
                        .filter(|c| c.profile.id != client.profile.id)
                        .filter(|c| {
                            entity
                                .2
                                .perms(c.profile.role, c.profile.id)
                                .is_some_and(|pv| pv.is_readable())
                        })
                    {
                        client
                            .send_ecast(JBMessage {
                                pc: 0,
                                re: None,
                                result: &line_value,
                            })
                            .await
                            .wrap_err("Failed to send doodle/line to ecast client")?;
                    }
                }
                if let JBValue::Doodle {
                    val: ref mut doodle,
                } = entity.value_mut().1.val
                {
                    doodle.lines.push(params.line);
                    doodle.lines.sort_unstable_by_key(|l| l.index);
                }
                client
                    .send_ecast(JBMessage {
                        pc: 0,
                        re: Some(message.seq),
                        result: &JBResult::Ok {},
                    })
                    .await
                    .wrap_err("Failed to send ecast client the result of doodle/stroke opcode")?;
            }
        }
        JBParams::TextGet(params)
        | JBParams::NumberGet(params)
        | JBParams::ObjectGet(params)
        | JBParams::DoodleGet(params)
        | JBParams::AudienceGCounterGet(params)
        | JBParams::AudiencePnCounterGet(params)
        | JBParams::AudienceCountGroupGet(params) => {
            if let Some(entity) = room.entities.get(&params.key) {
                let message = JBMessage {
                    pc: 0,
                    re: Some(message.seq),
                    result: &match jb_type.unwrap() {
                        JBType::Text => JBResult::Text(&entity.1),
                        JBType::Number => JBResult::Number(&entity.1),
                        JBType::Object => JBResult::Object(&entity.1),
                        JBType::Doodle => JBResult::Doodle(&entity.1),
                        JBType::AudiencePnCounter => JBResult::AudiencePnCounter(&entity.1),
                        JBType::AudienceGCounter => JBResult::AudienceGCounter(&entity.1),
                        JBType::AudienceCountGroup => JBResult::AudienceCountGroup(&entity.1),
                    },
                };
                client
                    .send_ecast(message)
                    .await
                    .wrap_err("Failed to send ecast client the requested entity")?;
            }
        }
        JBParams::NumberIncrement(params) => {
            if room.entities.get(&params.key).is_some_and(|e| {
                e.2.perms(client.profile.role, client.profile.id)
                    .is_some_and(|i| i.is_writable())
            }) {
                if let Some(mut entity) = room.entities.get_mut(&params.key) {
                    entity.value_mut().1.version += 1;
                    entity
                        .value()
                        .1
                        .from
                        .store(client.profile.id, std::sync::atomic::Ordering::Release);
                    let increment = entity.1.restrictions.increment.unwrap_or(1.0);

                    match entity.value_mut().1.val {
                        JBValue::Number { ref mut val } => {
                            *val += increment;
                            for client in room
                                .connections
                                .iter()
                                .filter(|c| c.profile.id != client.profile.id)
                                .filter(|c| {
                                    entity
                                        .2
                                        .perms(c.profile.role, c.profile.id)
                                        .is_some_and(|pv| pv.is_readable())
                                })
                            {
                                let message = JBMessage {
                                    pc: 0,
                                    re: None,
                                    result: &JBResult::Number(&entity.1),
                                };
                                client
                                    .send_ecast(message)
                                    .await
                                    .wrap_err("Failed to send ecast client an entity")?;
                            }
                        }
                        _ => {}
                    }
                }
                client
                    .send_ecast(JBMessage {
                        pc: 0,
                        re: Some(message.seq),
                        result: &JBResult::Ok {},
                    })
                    .await
                    .wrap_err(
                        "Failed to send ecast client the result of number/increment opcode",
                    )?;
            }
        }
        JBParams::NumberDecrement(params) => {
            if room.entities.get(&params.key).is_some_and(|e| {
                e.2.perms(client.profile.role, client.profile.id)
                    .is_some_and(|i| i.is_writable())
            }) {
                if let Some(mut entity) = room.entities.get_mut(&params.key) {
                    entity.value_mut().1.version += 1;
                    entity
                        .value()
                        .1
                        .from
                        .store(client.profile.id, std::sync::atomic::Ordering::Release);
                    let increment = entity.1.restrictions.increment.unwrap_or(1.0);

                    match entity.value_mut().1.val {
                        JBValue::Number { ref mut val } => {
                            *val -= increment;
                            for client in room
                                .connections
                                .iter()
                                .filter(|c| c.profile.id != client.profile.id)
                                .filter(|c| {
                                    entity
                                        .2
                                        .perms(c.profile.role, c.profile.id)
                                        .is_some_and(|pv| pv.is_readable())
                                })
                            {
                                let message = JBMessage {
                                    pc: 0,
                                    re: None,
                                    result: &JBResult::Number(&entity.1),
                                };
                                client
                                    .send_ecast(message)
                                    .await
                                    .wrap_err("Failed to send ecast client an entity")?;
                            }
                        }
                        _ => {}
                    }
                }
                client
                    .send_ecast(JBMessage {
                        pc: 0,
                        re: Some(message.seq),
                        result: &JBResult::Ok {},
                    })
                    .await
                    .wrap_err(
                        "Failed to send ecast client the result of number/decrement opcode",
                    )?;
            }
        }
        JBParams::ClientSend(params) => {
            // Only used for blobcast compatibility?
            assert_eq!(params.to, 1);
            if let Some(con) = room.connections.get(&params.to) {
                assert_eq!(con.client_type, ClientType::Blobcast);
                match con.client_type {
                    ClientType::Blobcast => {
                        con.send_blobcast(crate::blobcast::ws::JBResponse::Msg([
                            JBResponseArgs::Event {
                                event: JBEvent::CustomerMessage {
                                    user_id: Cow::Borrowed(&client.profile.user_id),
                                    message: params.body,
                                },
                                room_id: Cow::Borrowed(&room.room_config.code),
                            },
                        ]))
                        .await
                        .wrap_err("Failed to send blobcast host a CustomerMessage")?;
                    }
                    ClientType::Ecast => {
                        con.send_ecast(JBMessage {
                            pc: 0,
                            re: None,
                            result: &JBResult::ClientSend(params),
                        })
                        .await
                        .wrap_err("Failed to send ecast host a client/send")?;
                    }
                }
            }
            client
                .send_ecast(JBMessage {
                    pc: 0,
                    re: Some(message.seq),
                    result: &JBResult::Ok {},
                })
                .await
                .wrap_err("Failed to send ecast client the result of client/send opcode")?;
        }
        JBParams::RoomGetAudience(_) => {
            client
                .send_ecast(JBMessage {
                    pc: 0,
                    re: Some(message.seq),
                    result: &JBResult::RoomGetAudience {
                        connections: room
                            .entities
                            .get("audience")
                            .and_then(|e| {
                                let JBValue::AudiencePnCounter { ref count } = e.value().1.val
                                else {
                                    return None;
                                };
                                Some(count.load(std::sync::atomic::Ordering::Acquire))
                            })
                            .unwrap_or_default(),
                    },
                })
                .await
                .wrap_err("Failed to notify ecast host of room audience connections")?;
        }
        JBParams::RoomLock(_) => {
            if client.profile.role == Role::Host {
                room.room_config
                    .locked
                    .store(true, std::sync::atomic::Ordering::Release);

                for client in room
                    .connections
                    .iter()
                    .filter(|c| c.profile.id != client.profile.id)
                {
                    client
                        .send_ecast(JBMessage {
                            pc: 0,
                            re: None,
                            result: &JBResult::RoomLock {},
                        })
                        .await
                        .wrap_err("Failed to notify ecast client that room is locked")?;
                }
            }
            client
                .send_ecast(JBMessage {
                    pc: 0,
                    re: Some(message.seq),
                    result: &JBResult::Ok {},
                })
                .await
                .wrap_err(
                    "Failed to send ecast client the result of audience/g-counter/increment opcode",
                )?;
        }
        JBParams::AudienceGCounterIncrement(params)
        | JBParams::AudiencePnCounterIncrement(params) => {
            if let Some(entity) = room.entities.get(&params.key) {
                match entity.value().1.val {
                    JBValue::AudienceGCounter { ref count }
                    | JBValue::AudiencePnCounter { ref count } => {
                        count.fetch_add(params.times, std::sync::atomic::Ordering::AcqRel);
                    }
                    _ => {}
                }
            }
            client
                .send_ecast(JBMessage {
                    pc: 0,
                    re: Some(message.seq),
                    result: &JBResult::Ok {},
                })
                .await
                .wrap_err(
                    "Failed to send ecast client the result of audience/*-counter/increment opcode",
                )?;
        }
        JBParams::AudienceGCounterDecrement(params)
        | JBParams::AudiencePnCounterDecrement(params) => {
            if let Some(entity) = room.entities.get(&params.key) {
                match entity.value().1.val {
                    JBValue::AudienceGCounter { ref count }
                    | JBValue::AudiencePnCounter { ref count } => {
                        count.fetch_sub(params.times, std::sync::atomic::Ordering::AcqRel);
                    }
                    _ => {}
                }
            }
            client
                .send_ecast(JBMessage {
                    pc: 0,
                    re: Some(message.seq),
                    result: &JBResult::Ok {},
                })
                .await
                .wrap_err(
                    "Failed to send ecast client the result of audience/*-counter/decrement opcode",
                )?;
        }
        JBParams::AudienceCountGroupIncrement(params) => {
            if let Some(entity) = room.entities.get(&params.name) {
                if let JBValue::AudienceCountGroup(JBCountGroup { ref choices, .. }) =
                    entity.value().1.val
                {
                    choices
                        .get(&params.vote)
                        .map(|c| c.fetch_add(params.times, std::sync::atomic::Ordering::Relaxed));
                }
            }
            client
                .send_ecast(JBMessage {
                    pc: 0,
                    re: Some(message.seq),
                    result: &JBResult::Ok {},
                })
                .await
                .wrap_err(
                    "Failed to send ecast client the result of audience/g-counter/increment opcode",
                )?;
        }
        JBParams::RoomExit(_) => {
            client
                .send_ecast(JBMessage {
                    pc: 0,
                    re: Some(message.seq),
                    result: &JBResult::Ok {},
                })
                .await
                .wrap_err("Failed to notify ecast host of room closing")?;
            for client in room.connections.iter() {
                client
                    .close()
                    .await
                    .wrap_err("Closing socket failed during ecast room/exit")?;
            }
            room.exit.notify_waiters();
        }
        JBParams::Lock(params) => {
            if let Some(entity) = room.entities.get(&params.key) {
                entity
                    .value()
                    .2
                    .locked
                    .store(true, std::sync::atomic::Ordering::Release);
                entity
                    .value()
                    .1
                    .from
                    .store(client.profile.id, std::sync::atomic::Ordering::Release);
                let value = JBResult::Lock {
                    key: Cow::Borrowed(&params.key),
                    from: client.profile.id,
                };
                for client in room
                    .connections
                    .iter()
                    .filter(|c| c.profile.id != client.profile.id)
                    .filter(|c| {
                        entity
                            .2
                            .perms(c.profile.role, c.profile.id)
                            .is_some_and(|pv| pv.is_readable())
                    })
                {
                    client
                        .send_ecast(JBMessage {
                            pc: 0,
                            re: None,
                            result: &value,
                        })
                        .await
                        .wrap_err("Failed to notify ecast client of locked entity")?;
                }

                if doodle_config.render {
                    if let JBValue::Doodle { val: ref d } = entity.value().1.val {
                        let png_path = doodle_config.path.join(format!("{}.png", entity.key()));
                        d.render()
                            .wrap_err_with(|| {
                                format!("Failed to render doodle to {}", png_path.display())
                            })?
                            .save_png(&png_path)
                            .wrap_err_with(|| {
                                format!("Failed to save doodle to {}", png_path.display())
                            })?;
                    }
                }
            }
            client
                .send_ecast(JBMessage {
                    pc: 0,
                    re: Some(message.seq),
                    result: &JBResult::Ok {},
                })
                .await
                .wrap_err("Failed to send ecast client the result of lock opcode")?;
        }
        JBParams::Drop(params) => {
            if room.entities.get(&params.key).is_some_and(|e| {
                e.2.perms(client.profile.role, client.profile.id)
                    .is_some_and(|i| i.is_writable())
            }) {
                room.entities.remove(&params.key);
            }
            client
                .send_ecast(JBMessage {
                    pc: 0,
                    re: Some(message.seq),
                    result: &JBResult::Ok {},
                })
                .await
                .wrap_err("Failed to send ecast client the result of drop opcode")?;
        }
        JBParams::Other(value) => {
            let error = eyre!(
                "Unimplemented opcode with data: {}",
                serde_json::to_string(&value).unwrap()
            );
            tracing::error!(%error);
            client
                .send_ecast(JBMessage {
                    pc: 0,
                    re: Some(message.seq),
                    result: &JBResult::Ok {},
                })
                .await
                .wrap_err("Failed to send generic ok to ecast client")?;
        }
    }

    Ok(())
}

#[derive(Debug)]
struct GetEntities<'a> {
    entities: &'a DashMap<String, JBEntity>,
    role: Role,
    id: i64,
}

impl<'a> Serialize for GetEntities<'a> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = serializer.serialize_map(None)?;

        for e in self.entities.iter() {
            if e.value()
                .2
                .perms(self.role, self.id)
                .is_some_and(|i| i.is_readable())
            {
                map.serialize_entry(e.key(), e.value())?;
            }
        }

        map.end()
    }
}

#[derive(Debug)]
struct GetHere<'a>(&'a Connections, i64);

impl<'a> Serialize for GetHere<'a> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for profile in self.0.iter() {
            if *profile.key() != self.1
                && matches!(profile.value().profile.role, Role::Player | Role::Host)
            {
                map.serialize_entry(profile.key(), &profile.value().profile)?;
            }
        }
        map.end()
    }
}

#[instrument(skip(socket), fields(role = ?url_query.role))]
pub async fn handle_socket_proxy(
    host: String,
    socket: WebSocket,
    ecast_req: String,
    url_query: WSQuery,
) -> eyre::Result<()> {
    let mut ecast_req = ecast_req.into_client_request().unwrap();
    ecast_req
        .headers_mut()
        .append("Sec-WebSocket-Protocol", "ecast-v0".parse().unwrap());
    let (ecast_connection, _) = tokio_tungstenite::connect_async(ecast_req).await.unwrap();

    let (local_write, local_read) = socket.split();

    let (ecast_write, ecast_read) = ecast_connection.split();

    let local_to_ecast = local_read
        .map_err(|e: axum::Error| eyre!("local_to_ecast stream broken: {}", e))
        .map(
            move |m| -> eyre::Result<tokio_tungstenite::tungstenite::Message> {
                let m = match m.wrap_err("local_to_ecast message failed to be received")? {
                    axum::extract::ws::Message::Text(m) => {
                        let json_message = Some(&m);
                        tracing::debug!(
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
                            "to ecast",
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
                    message = ?m,
                    "to ecast",
                );
                m
            },
        )
        .forward(ecast_write.sink_map_err(|e| eyre!(e)));

    let ecast_to_local = ecast_read
        .map_err(|e: tokio_tungstenite::tungstenite::Error| {
            eyre!("ecast_to_local stream broken: {}", e)
        })
        .map(|m| -> eyre::Result<axum::extract::ws::Message> {
            let m = match m.wrap_err("ecast_to_local message failed to be received")? {
                tokio_tungstenite::tungstenite::Message::Text(m) => {
                    let json_message = Some(&m);
                    tracing::debug!(
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
                        "ecast to",
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
                message = ?m,
                "ecast to",
            );
            m
        })
        .forward(local_write.sink_map_err(|e| eyre!(e)));

    tokio::pin!(local_to_ecast, ecast_to_local);

    tokio::select! {
        r = local_to_ecast => r,
        r = ecast_to_local => r
    }
}
