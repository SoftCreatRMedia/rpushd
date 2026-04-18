/*
 * Copyright by SoftCreatR.dev.
 *
 * License: https://softcreatr.dev/license-terms
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
 * IN THE SOFTWARE.
 *
 * The above copyright notice and this disclaimer notice shall be included in all
 * copies or substantial portions of the Software.
 */

use std::env;
use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_stream::stream;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, EXPIRES, PRAGMA};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use socket2::{Domain, Protocol, Socket, Type};
use subtle::ConstantTimeEq;
use tokio::sync::broadcast;
use tokio::time::{self, MissedTickBehavior};
use tracing::{Level, info, warn};
use tracing_subscriber::EnvFilter;

type HmacSha256 = Hmac<Sha256>;

const CHANNEL_BUFFER_SIZE: usize = 512;
const DEFAULT_CHANNEL_IDLE_TTL_SECS: u64 = 3600;
const DEFAULT_HEARTBEAT_SECS: u64 = 15;
const DEFAULT_LISTEN_ADDRESS: &str = "127.0.0.1:45831";
const DEFAULT_LISTEN_BACKLOG: i32 = 16_384;
const PUSH_DAEMON_AUDIENCE: &str = "rpushd";
const REPOSITORY_URL: &str = "https://github.com/SoftCreatRMedia/rpushd";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Runtime configuration loaded from environment variables or created directly.
#[derive(Clone)]
pub struct Configuration {
    channel_idle_ttl: Duration,
    heartbeat_interval: Duration,
    listen_address: SocketAddr,
    publish_secret: String,
    subscription_secret: String,
}

impl Configuration {
    /// Creates a configuration instance explicitly.
    pub fn new(
        listen_address: SocketAddr,
        subscription_secret: impl Into<String>,
        publish_secret: impl Into<String>,
        heartbeat_interval: Duration,
        channel_idle_ttl: Duration,
    ) -> Self {
        Self {
            channel_idle_ttl,
            heartbeat_interval,
            listen_address,
            publish_secret: publish_secret.into(),
            subscription_secret: subscription_secret.into(),
        }
    }

    /// Builds the daemon configuration from environment variables.
    pub fn from_environment() -> Result<Self, String> {
        let listen_address = read_optional_env("RPUSHD_LISTEN", DEFAULT_LISTEN_ADDRESS)
            .parse::<SocketAddr>()
            .map_err(|error| format!("Invalid rpushd listen address: {error}"))?;

        let subscription_secret = read_required_env("RPUSHD_SECRET")
            .map(|value| value.trim().to_owned())
            .map_err(|_| "RPUSHD_SECRET is required".to_owned())?;
        if subscription_secret.is_empty() {
            return Err("RPUSHD_SECRET must not be empty".to_owned());
        }

        let publish_secret = read_required_env("RPUSHD_PUBLISH_SECRET")
            .map(|value| value.trim().to_owned())
            .map_err(|_| "RPUSHD_PUBLISH_SECRET is required".to_owned())?;
        if publish_secret.is_empty() {
            return Err("RPUSHD_PUBLISH_SECRET must not be empty".to_owned());
        }

        let heartbeat_interval = Duration::from_secs(parse_optional_env_u64(
            "RPUSHD_HEARTBEAT_SECS",
            DEFAULT_HEARTBEAT_SECS,
        )?);
        let channel_idle_ttl = Duration::from_secs(parse_optional_env_u64(
            "RPUSHD_CHANNEL_IDLE_TTL_SECS",
            DEFAULT_CHANNEL_IDLE_TTL_SECS,
        )?);

        Ok(Self::new(
            listen_address,
            subscription_secret,
            publish_secret,
            heartbeat_interval,
            channel_idle_ttl,
        ))
    }

    /// Returns the socket address the daemon should bind to.
    pub fn listen_address(&self) -> SocketAddr {
        self.listen_address
    }
}

/// Initializes structured logging for the daemon process.
pub fn initialize_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::from_default_env().add_directive(Level::INFO.into()));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

/// Builds the daemon router for a specific configuration.
pub fn build_app(configuration: Configuration) -> Router {
    let state = AppState::new(Arc::new(configuration));
    tokio::spawn(cleanup_channels_task(state.clone()));

    build_router(state)
}

/// Starts the daemon server and blocks until it exits.
pub async fn run(configuration: Configuration) -> Result<(), String> {
    let listen_address = configuration.listen_address();
    let state = AppState::new(Arc::new(configuration));
    tokio::spawn(cleanup_channels_task(state.clone()));

    let app = build_router(state);
    info!("listening on {listen_address}");

    let listener = create_listener(listen_address, DEFAULT_LISTEN_BACKLOG)
        .map_err(|error| format!("Failed to bind listener: {error}"))?;

    axum::serve(listener, app)
        .await
        .map_err(|error| format!("HTTP server terminated: {error}"))
}

fn create_listener(
    listen_address: SocketAddr,
    backlog: i32,
) -> Result<tokio::net::TcpListener, std::io::Error> {
    let domain = match listen_address {
        SocketAddr::V4(_) => Domain::IPV4,
        SocketAddr::V6(_) => Domain::IPV6,
    };

    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_nonblocking(true)?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.bind(&listen_address.into())?;
    socket.listen(backlog)?;

    let listener: std::net::TcpListener = socket.into();
    tokio::net::TcpListener::from_std(listener)
}

#[derive(Clone)]
struct AppState {
    channels: Arc<DashMap<String, Arc<ChannelBus>>>,
    configuration: Arc<Configuration>,
    metrics: Arc<AppMetrics>,
}

impl AppState {
    fn new(configuration: Arc<Configuration>) -> Self {
        Self {
            channels: Arc::new(DashMap::new()),
            configuration,
            metrics: Arc::new(AppMetrics::new(now_unix_seconds())),
        }
    }

    /// Returns the broadcast bus for a channel, creating it on demand.
    fn get_channel(&self, channel: &str) -> Arc<ChannelBus> {
        let now = now_unix_seconds();

        if let Some(existing) = self.channels.get(channel) {
            let existing = existing.clone();
            existing.touch(now);

            return existing;
        }

        let channel_bus = Arc::new(ChannelBus::new(now));
        let entry = self
            .channels
            .entry(channel.to_owned())
            .or_insert_with(|| channel_bus.clone());
        let channel_bus = entry.clone();
        channel_bus.touch(now);

        channel_bus
    }
}

struct ChannelBus {
    last_used: AtomicU64,
    sender: broadcast::Sender<Bytes>,
    subscribers: AtomicUsize,
}

impl ChannelBus {
    fn new(now: u64) -> Self {
        let (sender, _) = broadcast::channel(CHANNEL_BUFFER_SIZE);

        Self {
            last_used: AtomicU64::new(now),
            sender,
            subscribers: AtomicUsize::new(0),
        }
    }

    fn touch(&self, now: u64) {
        self.last_used.store(now, Ordering::Relaxed);
    }

    fn subscriber_count(&self) -> usize {
        self.subscribers.load(Ordering::Relaxed)
    }
}

struct SubscriptionGuard {
    channel: Arc<ChannelBus>,
}

impl SubscriptionGuard {
    fn new(channel: Arc<ChannelBus>) -> Self {
        channel.subscribers.fetch_add(1, Ordering::Relaxed);

        Self { channel }
    }
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        self.channel.subscribers.fetch_sub(1, Ordering::Relaxed);
        self.channel.touch(now_unix_seconds());
    }
}

struct StreamConnectionGuard {
    metrics: Arc<AppMetrics>,
}

impl StreamConnectionGuard {
    fn new(metrics: Arc<AppMetrics>) -> Self {
        metrics
            .stream_connections_total
            .fetch_add(1, Ordering::Relaxed);
        metrics
            .active_stream_connections
            .fetch_add(1, Ordering::Relaxed);

        Self { metrics }
    }
}

impl Drop for StreamConnectionGuard {
    fn drop(&mut self) {
        self.metrics
            .active_stream_connections
            .fetch_sub(1, Ordering::Relaxed);
    }
}

struct AppMetrics {
    active_stream_connections: AtomicUsize,
    auth_failures_total: AtomicU64,
    publish_requests_total: AtomicU64,
    published_bytes_total: AtomicU64,
    started_at: u64,
    stream_connections_total: AtomicU64,
}

impl AppMetrics {
    fn new(started_at: u64) -> Self {
        Self {
            active_stream_connections: AtomicUsize::new(0),
            auth_failures_total: AtomicU64::new(0),
            publish_requests_total: AtomicU64::new(0),
            published_bytes_total: AtomicU64::new(0),
            started_at,
            stream_connections_total: AtomicU64::new(0),
        }
    }
}

#[derive(Deserialize)]
struct PublishRequest {
    channel: String,
    message: Value,
}

#[derive(Deserialize)]
struct StreamRequest {
    token: String,
}

#[derive(Deserialize)]
struct SubscribeClaims {
    aud: String,
    channel: String,
    exp: u64,
    scope: String,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct StatsResponse {
    active_channels: usize,
    active_stream_connections: usize,
    active_subscribers: usize,
    auth_failures_total: u64,
    channels: Vec<ChannelStatistics>,
    memory_rss_bytes: Option<u64>,
    publish_requests_total: u64,
    published_bytes_total: u64,
    retained_channels: usize,
    repository_url: &'static str,
    started_at: u64,
    stream_connections_total: u64,
    uptime_seconds: u64,
    version: &'static str,
}

#[derive(Serialize)]
struct ChannelStatistics {
    idle_seconds: u64,
    name: String,
    subscribers: usize,
}

#[derive(Deserialize)]
struct StatsQuery {
    mode: Option<String>,
    verbose: Option<String>,
}

fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/publish", post(publish))
        .route("/api/stats", get(stats))
        .route("/api/stream/{channel}", post(stream_channel))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    Json(HealthResponse { status: "ok" })
}

async fn stats(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<StatsQuery>,
) -> Result<Response<Body>, StatusCode> {
    authorize_privileged_request(&state, &headers)?;

    let now = now_unix_seconds();
    let mut channels = state
        .channels
        .iter()
        .map(|entry| {
            let bus = entry.value();

            ChannelStatistics {
                idle_seconds: now.saturating_sub(bus.last_used.load(Ordering::Relaxed)),
                name: entry.key().clone(),
                subscribers: bus.subscriber_count(),
            }
        })
        .collect::<Vec<_>>();
    channels.sort_by(|left, right| {
        right
            .subscribers
            .cmp(&left.subscribers)
            .then_with(|| left.name.cmp(&right.name))
    });

    let retained_channels = channels.len();
    let active_channels = channels
        .iter()
        .filter(|channel| channel.subscribers > 0)
        .count();
    let active_subscribers = channels.iter().map(|channel| channel.subscribers).sum();

    let response = StatsResponse {
        active_channels,
        active_stream_connections: state
            .metrics
            .active_stream_connections
            .load(Ordering::Relaxed),
        active_subscribers,
        auth_failures_total: state.metrics.auth_failures_total.load(Ordering::Relaxed),
        channels: if verbose_enabled(query.verbose.as_deref()) {
            channels
        } else {
            Vec::new()
        },
        memory_rss_bytes: read_memory_rss_bytes(),
        publish_requests_total: state.metrics.publish_requests_total.load(Ordering::Relaxed),
        published_bytes_total: state.metrics.published_bytes_total.load(Ordering::Relaxed),
        retained_channels,
        repository_url: REPOSITORY_URL,
        started_at: state.metrics.started_at,
        stream_connections_total: state
            .metrics
            .stream_connections_total
            .load(Ordering::Relaxed),
        uptime_seconds: now.saturating_sub(state.metrics.started_at),
        version: VERSION,
    };

    render_stats_response(response, query.mode.as_deref())
}

async fn publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<PublishRequest>,
) -> impl IntoResponse {
    if !is_valid_channel(&request.channel) {
        return StatusCode::BAD_REQUEST;
    }
    if let Err(status) = authorize_privileged_request(&state, &headers) {
        return status;
    }

    let payload = match serde_json::to_vec(&request.message) {
        Ok(payload) => payload,
        Err(error) => {
            warn!("Failed to encode publish payload: {error}");
            return StatusCode::BAD_REQUEST;
        }
    };

    if payload.len() > u16::MAX as usize {
        return StatusCode::PAYLOAD_TOO_LARGE;
    }

    let channel = state.get_channel(&request.channel);
    let _ = channel.sender.send(frame_message(&payload));
    channel.touch(now_unix_seconds());
    state
        .metrics
        .publish_requests_total
        .fetch_add(1, Ordering::Relaxed);
    state
        .metrics
        .published_bytes_total
        .fetch_add(payload.len() as u64, Ordering::Relaxed);

    StatusCode::NO_CONTENT
}

async fn stream_channel(
    Path(channel): Path<String>,
    State(state): State<AppState>,
    Json(request): Json<StreamRequest>,
) -> Result<Response<Body>, StatusCode> {
    if !is_valid_channel(&channel) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let claims = verify_token(
        &state.configuration.subscription_secret,
        &channel,
        &request.token,
    )?;
    let channel_bus = state.get_channel(&claims.channel);
    let mut receiver = channel_bus.sender.subscribe();
    let heartbeat_interval = state.configuration.heartbeat_interval;
    let metrics = state.metrics.clone();
    let body_stream = stream! {
        let _subscription_guard = SubscriptionGuard::new(channel_bus.clone());
        let _stream_connection_guard = StreamConnectionGuard::new(metrics);

        yield Result::<Bytes, std::io::Error>::Ok(heartbeat_frame());

        let mut heartbeat = time::interval(heartbeat_interval);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                received = receiver.recv() => {
                    match received {
                        Ok(frame) => {
                            channel_bus.touch(now_unix_seconds());
                            yield Ok(frame);
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = heartbeat.tick() => {
                    channel_bus.touch(now_unix_seconds());
                    yield Ok(heartbeat_frame());
                }
            }
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/octet-stream")
        .header(
            CACHE_CONTROL,
            "no-cache, no-store, must-revalidate, no-transform",
        )
        .header(PRAGMA, "no-cache")
        .header(EXPIRES, "0")
        .header("x-content-type-options", "nosniff")
        .body(Body::from_stream(body_stream))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn cleanup_channels_task(state: AppState) {
    let mut interval = time::interval(Duration::from_secs(300));
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        interval.tick().await;

        let now = now_unix_seconds();
        let idle_ttl = state.configuration.channel_idle_ttl.as_secs();

        state.channels.retain(|channel, bus| {
            let keep = bus.subscriber_count() > 0
                || now.saturating_sub(bus.last_used.load(Ordering::Relaxed)) < idle_ttl;
            if !keep {
                info!("dropping idle channel {channel}");
            }

            keep
        });
    }
}

fn authorize_privileged_request(state: &AppState, headers: &HeaderMap) -> Result<(), StatusCode> {
    let authorization = match headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        Some(value) => value,
        None => {
            state
                .metrics
                .auth_failures_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(StatusCode::UNAUTHORIZED);
        }
    };

    let bearer_token = match authorization.strip_prefix("Bearer ") {
        Some(value) if !value.is_empty() => value,
        _ => {
            state
                .metrics
                .auth_failures_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(StatusCode::UNAUTHORIZED);
        }
    };

    if !secrets_equal(bearer_token, &state.configuration.publish_secret) {
        state
            .metrics
            .auth_failures_total
            .fetch_add(1, Ordering::Relaxed);
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(())
}

fn verify_token(
    secret: &str,
    expected_channel: &str,
    token: &str,
) -> Result<SubscribeClaims, StatusCode> {
    let mut parts = token.split('.');
    let header = parts.next().ok_or(StatusCode::UNAUTHORIZED)?;
    let payload = parts.next().ok_or(StatusCode::UNAUTHORIZED)?;
    let signature = parts.next().ok_or(StatusCode::UNAUTHORIZED)?;

    if parts.next().is_some() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let signing_input = format!("{header}.{payload}");
    let expected_signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;

    let mut mac = <HmacSha256 as KeyInit>::new_from_slice(secret.as_bytes())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    mac.update(signing_input.as_bytes());
    mac.verify_slice(&expected_signature)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;

    let payload = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    let claims = serde_json::from_slice::<SubscribeClaims>(&payload)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;

    if claims.aud != PUSH_DAEMON_AUDIENCE
        || claims.scope != "subscribe"
        || claims.channel != expected_channel
        || claims.exp < now_unix_seconds()
    {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(claims)
}

fn frame_message(message: &[u8]) -> Bytes {
    let length = message.len() as u16;
    let mut buffer = BytesMut::with_capacity(message.len() + 2);
    buffer.extend_from_slice(&length.to_be_bytes());
    buffer.extend_from_slice(message);

    buffer.freeze()
}

fn heartbeat_frame() -> Bytes {
    Bytes::from_static(&[0, 0])
}

fn is_valid_channel(channel: &str) -> bool {
    !channel.is_empty()
        && channel.len() <= 191
        && channel.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, ':' | '-' | '_' | '.')
        })
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn read_required_env(name: &str) -> Result<String, env::VarError> {
    env::var(name)
}

fn read_optional_env(name: &str, default: &str) -> String {
    read_required_env(name).unwrap_or_else(|_| default.to_owned())
}

fn parse_optional_env_u64(name: &str, default: u64) -> Result<u64, String> {
    match read_required_env(name) {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|error| format!("Invalid {name} value: {error}")),
        Err(_) => Ok(default),
    }
}

fn secrets_equal(left: &str, right: &str) -> bool {
    left.as_bytes().ct_eq(right.as_bytes()).into()
}

fn read_memory_rss_bytes() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let value = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()?;

    Some(value.saturating_mul(1024))
}

fn render_stats_response(
    response: StatsResponse,
    mode: Option<&str>,
) -> Result<Response<Body>, StatusCode> {
    match mode.unwrap_or("text") {
        "text" => build_text_response(&render_stats_text(&response), "text/plain; charset=utf-8"),
        "json" => {
            let body =
                serde_json::to_vec(&response).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            build_binary_response(body, "application/json; charset=utf-8")
        }
        "xml" => build_text_response(
            &render_stats_xml(&response),
            "application/xml; charset=utf-8",
        ),
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

fn build_text_response(body: &str, content_type: &str) -> Result<Response<Body>, StatusCode> {
    build_binary_response(body.as_bytes().to_vec(), content_type)
}

fn build_binary_response(body: Vec<u8>, content_type: &str) -> Result<Response<Body>, StatusCode> {
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, content_type)
        .header(
            CACHE_CONTROL,
            "no-cache, no-store, must-revalidate, no-transform",
        )
        .header(PRAGMA, "no-cache")
        .header(EXPIRES, "0")
        .body(Body::from(body))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

fn render_stats_text(response: &StatsResponse) -> String {
    let mut lines = vec![
        "rpushd stats".to_owned(),
        format!("version: {}", response.version),
        format!("repository: {}", response.repository_url),
        format!("started_at: {}", response.started_at),
        format!("uptime_seconds: {}", response.uptime_seconds),
        format!("active_channels: {}", response.active_channels),
        format!("retained_channels: {}", response.retained_channels),
        format!(
            "active_stream_connections: {}",
            response.active_stream_connections
        ),
        format!("active_subscribers: {}", response.active_subscribers),
        format!(
            "stream_connections_total: {}",
            response.stream_connections_total
        ),
        format!(
            "publish_requests_total: {}",
            response.publish_requests_total
        ),
        format!("published_bytes_total: {}", response.published_bytes_total),
        format!("auth_failures_total: {}", response.auth_failures_total),
        format!(
            "memory_rss_bytes: {}",
            response
                .memory_rss_bytes
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_owned())
        ),
    ];

    if response.channels.is_empty() {
        lines.push("channels: hidden (use ?verbose=1 to include channel details)".to_owned());
    } else {
        lines.push("channels:".to_owned());
        for channel in &response.channels {
            lines.push(format!(
                "- {} | subscribers={} | idle_seconds={}",
                channel.name, channel.subscribers, channel.idle_seconds
            ));
        }
    }

    lines.join("\n")
}

fn render_stats_xml(response: &StatsResponse) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<stats>");
    append_xml_element(&mut xml, "version", response.version);
    append_xml_element(&mut xml, "repositoryUrl", response.repository_url);
    append_xml_element(&mut xml, "startedAt", &response.started_at.to_string());
    append_xml_element(
        &mut xml,
        "uptimeSeconds",
        &response.uptime_seconds.to_string(),
    );
    append_xml_element(
        &mut xml,
        "activeChannels",
        &response.active_channels.to_string(),
    );
    append_xml_element(
        &mut xml,
        "retainedChannels",
        &response.retained_channels.to_string(),
    );
    append_xml_element(
        &mut xml,
        "activeStreamConnections",
        &response.active_stream_connections.to_string(),
    );
    append_xml_element(
        &mut xml,
        "activeSubscribers",
        &response.active_subscribers.to_string(),
    );
    append_xml_element(
        &mut xml,
        "streamConnectionsTotal",
        &response.stream_connections_total.to_string(),
    );
    append_xml_element(
        &mut xml,
        "publishRequestsTotal",
        &response.publish_requests_total.to_string(),
    );
    append_xml_element(
        &mut xml,
        "publishedBytesTotal",
        &response.published_bytes_total.to_string(),
    );
    append_xml_element(
        &mut xml,
        "authFailuresTotal",
        &response.auth_failures_total.to_string(),
    );
    append_xml_element(
        &mut xml,
        "memoryRssBytes",
        &response
            .memory_rss_bytes
            .map(|value| value.to_string())
            .unwrap_or_default(),
    );
    xml.push_str("<channels>");
    for channel in &response.channels {
        xml.push_str("<channel>");
        append_xml_element(&mut xml, "name", &channel.name);
        append_xml_element(&mut xml, "subscribers", &channel.subscribers.to_string());
        append_xml_element(&mut xml, "idleSeconds", &channel.idle_seconds.to_string());
        xml.push_str("</channel>");
    }
    xml.push_str("</channels></stats>");

    xml
}

fn append_xml_element(output: &mut String, name: &str, value: &str) {
    output.push('<');
    output.push_str(name);
    output.push('>');
    output.push_str(&escape_xml(value));
    output.push_str("</");
    output.push_str(name);
    output.push('>');
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn verbose_enabled(value: Option<&str>) -> bool {
    matches!(value, Some("1" | "true" | "yes" | "on"))
}
