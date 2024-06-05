use std::{
    borrow::Cow,
    fmt::{Debug, Display, LowerHex},
    net::SocketAddr,
    ops::DerefMut,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64},
        Arc,
    },
    time::Duration,
};

use axum::{
    extract::{
        ws::{Message, WebSocket},
        Host, OriginalUri,
    },
    handler::HandlerWithoutStateExt,
    http::{uri::PathAndQuery, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    BoxError, Json, Router,
};
use axum_server::tls_rustls::RustlsConfig;
use color_eyre::eyre::{self, Context};
use dashmap::DashMap;
use error::WithStatusCode;
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt,
};
use helix_stdx::rope::RegexBuilder;
use rand::{rngs::OsRng, RngCore, SeedableRng};
use rand_xoshiro::Xoshiro128PlusPlus;
use reqwest::header::USER_AGENT;
use serde::{de::Error, Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{watch::Sender, Mutex, Notify, RwLock};
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::instrument;
use tracing_error::ErrorLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use tree_sitter::{Parser, QueryCursor};

mod acl;
mod blobcast;
mod ecast;
mod entity;
mod error;
mod http_cache;
mod tts;
mod tui;

#[derive(Debug, Clone, Copy)]
pub struct Token([u8; 12]);

impl From<[u8; 12]> for Token {
    fn from(value: [u8; 12]) -> Self {
        Self(value)
    }
}

impl Serialize for Token {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Serialize::serialize(&format!("{:02x}", self), serializer)
    }
}

impl<'de> Deserialize<'de> for Token {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let token: Cow<'de, str> = Deserialize::deserialize(deserializer)?;
        let mut bytes = [0u8; 12];
        let len =
            token
                .as_bytes()
                .chunks(2)
                .zip(bytes.iter_mut())
                .try_fold(0, |count, (bi, bo)| {
                    let hex_str = unsafe { std::str::from_utf8_unchecked(bi) };
                    *bo = u8::from_str_radix(hex_str, 16).map_err(|_| {
                        D::Error::invalid_value(
                            serde::de::Unexpected::Str(hex_str),
                            &"A hexadecimal number",
                        )
                    })?;

                    Ok(count + 1)
                })?;
        assert_eq!(len, 12);
        Ok(Token(bytes))
    }
}

impl LowerHex for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for t in self.0 {
            write!(f, "{:02x}", t)?;
        }
        Ok(())
    }
}

impl Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        <Token as LowerHex>::fmt(self, f)
    }
}

impl Token {
    fn random() -> Token {
        let mut t = Token([0u8; 12]);
        OsRng.fill_bytes(&mut t.0);
        t
    }

    fn from_seed(s: u64) -> Token {
        let mut t = Token([0u8; 12]);
        Xoshiro128PlusPlus::seed_from_u64(s).fill_bytes(&mut t.0);
        t
    }
}

#[derive(Clone)]
struct State {
    blobcast_host: Arc<RwLock<String>>,
    room_map: Arc<DashMap<String, Arc<Room>>>,
    http_cache: http_cache::HttpCache,
    config: Arc<Config>,
    tui_sender: Sender<()>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum OpMode {
    Native,
    Proxy,
}

#[derive(Deserialize)]
struct Config {
    doodles: DoodleConfig,
    ecast: Ecast,
    blobcast: Ecast,
    tls: Tls,
    tts: TTSConfig,
    tui: bool,
    ports: Ports,
    cache: CacheConfig,
    accessible_host: String,
}

#[derive(Deserialize)]
struct DoodleConfig {
    render: bool,
    path: PathBuf,
}

#[derive(Deserialize)]
struct Ecast {
    op_mode: OpMode,
    server_url: Option<String>,
}

#[derive(Deserialize, Debug)]
struct Tls {
    cert: PathBuf,
    key: PathBuf,
}

#[derive(Deserialize)]
struct TTSConfig {
    piper_bin: PathBuf,
    ffmpeg_bin: PathBuf,
    voices_path: PathBuf,
    tts_dir: PathBuf,
    op_mode: OpMode,
}

#[derive(Deserialize, Clone, Copy)]
struct Ports {
    https: u16,
    blobcast: u16,
    http: Option<u16>,
}

#[derive(Deserialize)]
struct CacheConfig {
    cache_path: PathBuf,
    cache_mode: CacheMode,
}

#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
pub enum CacheMode {
    Online,
    Oneshot,
    Offline,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JBRoom {
    pub app_id: String,
    pub app_tag: String,
    pub audience_enabled: bool,
    pub code: String,
    pub host: String,
    pub audience_host: String,
    pub locked: AtomicBool,
    pub full: AtomicBool,
    pub moderation_enabled: bool,
    pub password_required: bool,
    pub twitch_locked: bool,
    pub locale: Cow<'static, str>,
    pub keepalive: bool,
}

impl Clone for JBRoom {
    fn clone(&self) -> Self {
        Self {
            app_id: self.app_id.clone(),
            app_tag: self.app_tag.clone(),
            audience_enabled: self.audience_enabled,
            code: self.code.clone(),
            host: self.host.clone(),
            audience_host: self.audience_host.clone(),
            locked: self
                .locked
                .load(std::sync::atomic::Ordering::Acquire)
                .into(),
            full: self.full.load(std::sync::atomic::Ordering::Acquire).into(),
            moderation_enabled: self.moderation_enabled,
            password_required: self.password_required,
            twitch_locked: self.twitch_locked,
            locale: self.locale.clone(),
            keepalive: self.keepalive,
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
pub enum ClientType {
    Blobcast,
    Ecast,
}

type Connections = DashMap<i64, Arc<Client>>;

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JBProfile {
    pub id: i64,
    pub user_id: String,
    pub role: acl::Role,
    pub name: String,
    pub roles: JBProfileRoles,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub enum JBProfileRoles {
    Host {},
    Player { name: String },
    None,
}

#[derive(Debug)]
pub struct Client {
    pub profile: JBProfile,
    socket: Mutex<Option<SplitSink<WebSocket, Message>>>,
    pc: AtomicU64,
    client_type: ClientType,
    secret: Token,
}

impl Client {
    pub async fn send_ecast(&self, mut message: ecast::ws::JBMessage<'_>) -> eyre::Result<()> {
        message.pc = self.pc.fetch_add(1, std::sync::atomic::Ordering::AcqRel);

        tracing::debug!(?message, "Sending WS Message");

        let json_message = serde_json::to_string(&message)
            .wrap_err_with(|| format!("Failed to serialize ecast message: {:?}", &message))?;
        if let Err(e) = self.send_ws_message(Message::Text(json_message)).await {
            self.disconnect().await;
            return Err(e)
                .wrap_err_with(|| format!("Ecast message failed to send: {:?}", &message));
        }

        Ok(())
    }

    pub async fn send_blobcast(&self, message: blobcast::ws::JBMessage<'_>) -> eyre::Result<()> {
        tracing::debug!(?message, "Sending WS Message");

        if let Err(e) = self
            .send_ws_message(Message::Text(format!(
                "5:::{}",
                serde_json::to_string(&message).wrap_err_with(|| {
                    format!("Failed to serialize blobcast message: {:?}", &message)
                })?
            )))
            .await
        {
            self.disconnect().await;
            return Err(e)
                .wrap_err_with(|| format!("Blobcast message failed to send: {:?}", &message));
        }

        Ok(())
    }

    pub async fn ping(&self, d: Vec<u8>) -> eyre::Result<()> {
        if let Err(e) = self.send_ws_message(Message::Ping(d)).await {
            self.disconnect().await;
            return Err(e).wrap_err("Failed to ping socket");
        }

        Ok(())
    }

    pub async fn pong(&self, d: Vec<u8>) -> eyre::Result<()> {
        if let Err(e) = self.send_ws_message(Message::Pong(d)).await {
            self.disconnect().await;
            return Err(e).wrap_err("Failed to pong socket");
        }

        Ok(())
    }

    pub async fn close(&self) -> eyre::Result<()> {
        tracing::debug!("Closing connection");

        self.send_ws_message(Message::Close(Some(axum::extract::ws::CloseFrame {
            code: 1000,
            reason: Cow::Borrowed("normal close"),
        })))
        .await
        .wrap_err("Failed to close connection")?;

        Ok(())
    }

    pub async fn disconnect(&self) {
        *self.socket.lock().await = None;
    }

    pub async fn send_ws_message(&self, message: Message) -> eyre::Result<()> {
        if let Some(socket) = self.socket.lock().await.deref_mut() {
            tokio::select! {
                r = socket.send(message) => r.wrap_err("WS Message failed to send")?,
                _ = tokio::time::sleep(Duration::from_secs(3)) => {
                    tracing::error!("Connection timed out");
                    self.disconnect().await;
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct Room {
    pub entities: DashMap<String, entity::JBEntity>,
    pub connections: Connections,
    pub room_serial: AtomicI64,
    pub room_config: JBRoom,
    pub exit: Notify,
    pub channel: Sender<()>,
}

impl Room {
    async fn close(&self) -> Result<(), axum::Error> {
        futures_util::future::try_join_all(self.connections.iter().map(|connection| async move {
            if let Some(socket) = connection.socket.lock().await.as_mut() {
                socket.close().await
            } else {
                Ok(())
            }
        }))
        .await?;
        Ok(())
    }
}

pub struct ConnectedSocket {
    pub client: Arc<Client>,
    pub room: Arc<Room>,
    pub read_half: SplitStream<WebSocket>,
    pub reconnected: bool,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    color_eyre::install()?;

    let config_file =
        std::fs::read_to_string("config.toml").wrap_err("Could not read config.toml")?;
    let config: Config =
        toml::from_str(&config_file).wrap_err("Failed to deserialize config.toml")?;

    let tls_config = RustlsConfig::from_pem_file(&config.tls.cert, &config.tls.key)
        .await
        .wrap_err_with(|| format!("TLS Config failed: {:?}", config.tls))?;

    let fragment_regex = RegexBuilder::new()
        .build("blobcast.jackboxgames.com|ecast.jackboxgames.com|bundles.jackbox.tv|jackbox.tv|cdn.jackboxgames.com|s3.amazonaws.com")
        .wrap_err("fragment_regex failed to build")?;
    let content_to_compress = RegexBuilder::new().build("text/html|text/css|text/xml|text/javascript|application/javascript|application/x-javascript|application/json").wrap_err("content_to_compress regex failed to build")?;

    let js_lang = tree_sitter_javascript::language();
    let fragment_query = tree_sitter::Query::new(&js_lang, "(string_fragment) @frag")
        .wrap_err("fragment_query failed to build")?;
    let css_lang = tree_sitter_css::language();
    let css_query = tree_sitter::Query::new(&css_lang, "(plain_value) @frag")
        .wrap_err("css_query failed to build")?;
    let (tx, rx) = tokio::sync::watch::channel(());
    let state = State {
        tui_sender: tx.clone(),
        blobcast_host: Arc::new(RwLock::new(String::new())),
        room_map: Arc::new(DashMap::new()),
        http_cache: http_cache::HttpCache {
            client: reqwest::Client::new(),
            ts_parser: Arc::new(Mutex::new((Parser::new(), QueryCursor::new()))),
            js_lang: Arc::new((js_lang, fragment_query)),
            css_lang: Arc::new((css_lang, css_query)),
            regexes: Arc::new(http_cache::Regexes {
                content_to_compress,
                jackbox_urls: fragment_regex,
            }),
        },
        config: Arc::new(config),
    };

    let handle = axum_server::Handle::new();
    let tracing_registry = tracing_subscriber::registry()
        .with(ErrorLayer::default())
        .with(
            EnvFilter::try_from_default_env()
                .or_else(|_| EnvFilter::try_new("info"))
                .unwrap(),
        );
    let tui_state = state.clone();
    let tui_future = if state.config.tui {
        let log_writer = tui::tracing_writer::TuiWriter::new(tx);
        let fmt_layer = tracing_subscriber::fmt::layer()
            .event_format(log_writer.clone())
            .with_writer(std::io::sink);
        tracing_registry.with(fmt_layer).init();

        tui::tui(
            handle.clone(),
            tui_state,
            Some(log_writer),
            rx,
            state.config.tui,
        )
    } else {
        tracing_registry
            .with(tracing_subscriber::fmt::layer())
            .init();
        tui::tui(handle.clone(), tui_state, None, rx, state.config.tui)
    };

    tokio::fs::create_dir_all(&state.config.doodles.path)
        .await
        .wrap_err_with(|| {
            format!(
                "Failed to create doodles dir: {}",
                state.config.doodles.path.display()
            )
        })?;

    let ports = state.config.ports;

    let app = Router::new()
        .route("/api/v2/rooms/:code/play", get(ecast::play_handler))
        .route("/api/v2/audience/:code/play", get(ecast::play_handler))
        .route("/api/v2/rooms", post(ecast::rooms_handler))
        .route(
            "/@ecast.jackboxgames.com/api/v2/rooms/:code",
            get(ecast::rooms_get_handler),
        )
        .route(
            "/api/v2/app-configs/:app_tag",
            get(ecast::app_config_handler),
        )
        .route("/tts/generate", post(tts::generate_handler))
        .route_service(
            "/tts/*path",
            ServeDir::new(state.config.tts.tts_dir.clone()),
        )
        .route("/room", get(blobcast::rooms_handler))
        .route("/accessToken", post(blobcast::access_token_handler))
        .route("/socket.io/1", get(blobcast::load_handler))
        .route("/socket.io/1/websocket/:id", get(blobcast::play_handler))
        .fallback(serve_jb_tv)
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], ports.https));
    tracing::info!("Ecast listening on {}", addr);
    let blobcast_addr = SocketAddr::from(([0, 0, 0, 0], ports.blobcast));
    tracing::info!("Blobcast listening on {}", blobcast_addr);
    tokio::try_join!(
        axum_server::bind_rustls(addr, tls_config.clone())
            .handle(handle.clone())
            .serve(app.clone().into_make_service()),
        axum_server::bind_rustls(blobcast_addr, tls_config)
            .handle(handle.clone())
            .serve(app.into_make_service()),
        redirect_http_to_https(ports, handle.clone()),
        tui_future,
    )?;

    Ok(())
}

#[instrument(skip(state, headers))]
async fn serve_jb_tv(
    axum::extract::State(state): axum::extract::State<State>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> crate::error::Result<Response> {
    if uri.query().is_some_and(|q| q.contains("&s=")) {
        let mut new_query: String = format!("{}?", uri.path());
        new_query.extend(
            uri.query()
                .unwrap()
                .split('&')
                .filter(|q| !q.starts_with("s="))
                .flat_map(|q| [q, "&"]),
        );
        new_query.pop();
        let mut parts = uri.into_parts();
        parts.path_and_query = Some(
            PathAndQuery::try_from(&new_query)
                .wrap_err_with(|| format!("Generated query `{}` was not valid", new_query))
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?,
        );

        return Ok(Redirect::to(&Uri::from_parts(parts).unwrap().to_string()).into_response());
    }
    if headers
        .get(USER_AGENT)
        .map(|a| {
            Ok(a.to_str()
                .wrap_err("UserAgent was not valid UTF-8")
                .with_status_code(StatusCode::UNPROCESSABLE_ENTITY)?
                .starts_with("JackboxGames"))
        })
        .unwrap_or(Ok(false))?
    {
        tracing::error!("unknown endpoint");
        Ok(Json(json! ({ "ok": true, "body": {} })).into_response())
    } else {
        return state
            .http_cache
            .get_cached(
                uri,
                headers,
                state.config.cache.cache_mode,
                &state.config.accessible_host,
                &state.config.cache.cache_path,
            )
            .await;
        // Ok(().into_response())
    }
}

pub fn room_id() -> String {
    fn random(size: usize) -> Vec<u8> {
        let mut bytes: Vec<u8> = vec![0; size];

        OsRng.fill_bytes(&mut bytes);

        bytes
    }
    const ALPHA_CAPITAL: [char; 26] = [
        'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R',
        'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z',
    ];
    let mut code = nanoid::nanoid!(4, &ALPHA_CAPITAL, random);
    code.make_ascii_uppercase();
    code
}

async fn redirect_http_to_https(
    ports: Ports,
    handle: axum_server::Handle,
) -> Result<(), std::io::Error> {
    if let Some(port) = ports.http {
        fn make_https(
            host: String,
            uri: Uri,
            http: String,
            https: String,
        ) -> Result<Uri, BoxError> {
            tracing::debug!(host, ?uri, http, https, "Received HTTP request");
            let mut parts = uri.into_parts();

            parts.scheme = Some(axum::http::uri::Scheme::HTTPS);

            if parts.path_and_query.is_none() {
                parts.path_and_query = Some("/".parse().unwrap());
            }

            let https_host = host.replace(&http, &https);
            parts.authority = Some(https_host.parse()?);

            Ok(Uri::from_parts(parts)?)
        }

        let http = format!("{}", port);
        let https = format!("{}", ports.https);
        let redirect = move |Host(host): Host, uri: Uri| async move {
            match make_https(host, uri, http, https) {
                Ok(uri) => Ok(Redirect::permanent(&uri.to_string())),
                Err(error) => {
                    tracing::warn!(%error, "failed to convert URI to HTTPS");
                    Err(StatusCode::BAD_REQUEST)
                }
            }
        };

        let addr = SocketAddr::from(([0, 0, 0, 0], 80));
        tracing::info!("listening on {}", addr);
        axum_server::bind(addr)
            .handle(handle)
            .serve(redirect.into_make_service())
            .await
    } else {
        Ok(())
    }
}
