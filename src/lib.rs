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
use bytes::Bytes;
use dashmap::{DashMap, mapref::entry::Entry};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use socket2::{Domain, Protocol, Socket, Type};
use subtle::ConstantTimeEq;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast};
use tokio::time::{self, MissedTickBehavior};
use tracing::{Level, info, warn};
use tracing_subscriber::EnvFilter;

type HmacSha256 = Hmac<Sha256>;

const CHANNEL_BUFFER_SIZE: usize = 512;
const DEFAULT_HEARTBEAT_SECS: u64 = 15;
const MAX_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3600);
const DEFAULT_MAX_STREAMS: usize = 4096;
const DEFAULT_MAX_CHANNELS: usize = 1024;
const DEFAULT_MAX_BUFFERED_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_LISTEN_ADDRESS: &str = "127.0.0.1:45831";
const DEFAULT_LISTEN_BACKLOG: i32 = 16_384;
const PUSH_DAEMON_AUDIENCE: &str = "rpushd";
const REPOSITORY_URL: &str = "https://github.com/SoftCreatRMedia/rpushd";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Runtime configuration loaded from environment variables or created directly.
#[derive(Clone)]
pub struct Configuration {
    heartbeat_interval: Duration,
    listen_address: SocketAddr,
    max_streams: usize,
    max_channels: usize,
    max_buffered_bytes: usize,
    publish_secret: String,
    subscription_secret: String,
}

impl Configuration {
    /// Creates a configuration instance explicitly.
    ///
    /// Invalid heartbeat intervals use the 15-second default. The former idle TTL
    /// argument is retained for callers; channels now close with their last stream.
    pub fn new(
        listen_address: SocketAddr,
        subscription_secret: impl Into<String>,
        publish_secret: impl Into<String>,
        heartbeat_interval: Duration,
        _channel_idle_ttl: Duration,
    ) -> Self {
        Self {
            heartbeat_interval: if valid_heartbeat_interval(heartbeat_interval) {
                heartbeat_interval
            } else {
                Duration::from_secs(DEFAULT_HEARTBEAT_SECS)
            },
            listen_address,
            max_streams: DEFAULT_MAX_STREAMS,
            max_channels: DEFAULT_MAX_CHANNELS,
            max_buffered_bytes: DEFAULT_MAX_BUFFERED_BYTES,
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
        if !valid_heartbeat_interval(heartbeat_interval) {
            return Err("RPUSHD_HEARTBEAT_SECS must be between 1 and 3600".to_owned());
        }

        let configuration = Self::new(
            listen_address,
            subscription_secret,
            publish_secret,
            heartbeat_interval,
            Duration::ZERO,
        )
        .with_limits(
            parse_limit("RPUSHD_MAX_STREAMS", DEFAULT_MAX_STREAMS)?,
            parse_limit("RPUSHD_MAX_CHANNELS", DEFAULT_MAX_CHANNELS)?,
            parse_limit("RPUSHD_MAX_BUFFERED_BYTES", DEFAULT_MAX_BUFFERED_BYTES)?,
        )?;
        configuration.validate_secrets()?;

        Ok(configuration)
    }

    /// Sets the stream, active channel, and shared buffered frame byte limits.
    pub fn with_limits(
        mut self,
        max_streams: usize,
        max_channels: usize,
        max_buffered_bytes: usize,
    ) -> Result<Self, String> {
        for (name, limit) in [
            ("RPUSHD_MAX_STREAMS", max_streams),
            ("RPUSHD_MAX_CHANNELS", max_channels),
            ("RPUSHD_MAX_BUFFERED_BYTES", max_buffered_bytes),
        ] {
            if !(1..=Semaphore::MAX_PERMITS).contains(&limit) {
                return Err(format!(
                    "{name} must be between 1 and {}",
                    Semaphore::MAX_PERMITS
                ));
            }
        }

        self.max_streams = max_streams;
        self.max_channels = max_channels;
        self.max_buffered_bytes = max_buffered_bytes;
        Ok(self)
    }

    fn validate_secrets(&self) -> Result<(), String> {
        for (name, secret) in [
            ("RPUSHD_SECRET", self.subscription_secret.as_str()),
            ("RPUSHD_PUBLISH_SECRET", self.publish_secret.as_str()),
        ] {
            if secret.trim().is_empty() || secret.starts_with("replace-with-") {
                return Err(format!("{name} must contain a generated secret"));
            }
        }
        if secrets_equal(&self.subscription_secret, &self.publish_secret) {
            return Err("RPUSHD_SECRET and RPUSHD_PUBLISH_SECRET must differ".to_owned());
        }
        Ok(())
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
    build_router(AppState::new(configuration))
}

/// Starts the daemon server and blocks until it exits.
pub async fn run(configuration: Configuration) -> Result<(), String> {
    configuration.validate_secrets()?;
    let listener = create_listener(configuration.listen_address(), DEFAULT_LISTEN_BACKLOG)
        .map_err(|error| format!("Failed to bind listener: {error}"))?;
    let listen_address = listener
        .local_addr()
        .map_err(|error| format!("Failed to read listener address: {error}"))?;
    info!("listening on {listen_address}");

    axum::serve(listener, build_app(configuration))
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
    socket.bind(&listen_address.into())?;
    socket.listen(backlog)?;

    let listener: std::net::TcpListener = socket.into();
    tokio::net::TcpListener::from_std(listener)
}

struct AppState {
    channels: DashMap<String, Channel>,
    configuration: Configuration,
    metrics: AppMetrics,
    stream_slots: Arc<Semaphore>,
    channel_slots: Arc<Semaphore>,
    buffered_bytes: Arc<Semaphore>,
}

impl AppState {
    fn new(configuration: Configuration) -> Arc<Self> {
        Arc::new(Self {
            channels: DashMap::new(),
            metrics: AppMetrics::new(now_unix_seconds()),
            stream_slots: Arc::new(Semaphore::new(configuration.max_streams)),
            channel_slots: Arc::new(Semaphore::new(configuration.max_channels)),
            buffered_bytes: Arc::new(Semaphore::new(configuration.max_buffered_bytes)),
            configuration,
        })
    }

    fn subscribe(self: &Arc<Self>, channel: String) -> Result<Subscription, StatusCode> {
        let stream_slot = self
            .stream_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| self.reject_stream())?;

        // Hold the map entry until the receiver exists, so removal cannot orphan it.
        let receiver = match self.channels.entry(channel.clone()) {
            Entry::Occupied(entry) => {
                let bus = entry.get();
                bus.touch();
                bus.sender.subscribe()
            }
            Entry::Vacant(entry) => {
                let channel_slot = self
                    .channel_slots
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| self.reject_stream())?;
                let (sender, receiver) = broadcast::channel(CHANNEL_BUFFER_SIZE);
                entry.insert(Channel {
                    last_used: AtomicU64::new(now_unix_seconds()),
                    sender,
                    _slot: channel_slot,
                });
                receiver
            }
        };

        self.metrics
            .stream_connections_total
            .fetch_add(1, Ordering::Relaxed);
        self.metrics
            .active_stream_connections
            .fetch_add(1, Ordering::Relaxed);
        Ok(Subscription {
            state: self.clone(),
            channel,
            receiver: Some(receiver),
            _slot: stream_slot,
        })
    }

    fn reject_stream(&self) -> StatusCode {
        self.metrics
            .stream_admission_rejections_total
            .fetch_add(1, Ordering::Relaxed);
        StatusCode::SERVICE_UNAVAILABLE
    }

    fn broadcast(&self, channel: &str, frame: Vec<u8>) -> Result<(), StatusCode> {
        let Some(bus) = self.channels.get(channel) else {
            return Ok(());
        };
        if bus.sender.receiver_count() == 0 {
            return Ok(());
        }

        let budget = self
            .buffered_bytes
            .clone()
            .try_acquire_many_owned(frame.len() as u32)
            .map_err(|_| {
                self.metrics
                    .publish_overload_total
                    .fetch_add(1, Ordering::Relaxed);
                StatusCode::SERVICE_UNAVAILABLE
            })?;

        let frame = Bytes::from_owner(BufferedFrame {
            bytes: frame.into_boxed_slice(),
            _budget: budget,
        });
        if bus.sender.send(frame).is_ok() {
            bus.touch();
        }

        Ok(())
    }
}

struct Channel {
    last_used: AtomicU64,
    sender: broadcast::Sender<Bytes>,
    _slot: OwnedSemaphorePermit,
}

impl Channel {
    fn touch(&self) {
        self.last_used.store(now_unix_seconds(), Ordering::Relaxed);
    }
}

struct BufferedFrame {
    bytes: Box<[u8]>,
    _budget: OwnedSemaphorePermit,
}

impl AsRef<[u8]> for BufferedFrame {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

struct Subscription {
    state: Arc<AppState>,
    channel: String,
    receiver: Option<broadcast::Receiver<Bytes>>,
    _slot: OwnedSemaphorePermit,
}

impl Subscription {
    async fn recv(&mut self) -> Result<Bytes, broadcast::error::RecvError> {
        self.receiver
            .as_mut()
            .expect("live subscription")
            .recv()
            .await
    }

    fn record_lag(&self, missed: u64) {
        self.state
            .metrics
            .stream_lagged_total
            .fetch_add(1, Ordering::Relaxed);
        self.state
            .metrics
            .lagged_events_total
            .fetch_add(missed, Ordering::Relaxed);
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        drop(self.receiver.take());
        if self
            .state
            .channels
            .remove_if(&self.channel, |_, bus| bus.sender.receiver_count() == 0)
            .is_some()
        {
            self.state
                .metrics
                .channel_removals_total
                .fetch_add(1, Ordering::Relaxed);
        }
        self.state
            .metrics
            .active_stream_connections
            .fetch_sub(1, Ordering::Relaxed);
    }
}

struct AppMetrics {
    subscription_auth_failures_total: AtomicU64,
    stream_lagged_total: AtomicU64,
    stream_admission_rejections_total: AtomicU64,
    publish_overload_total: AtomicU64,
    lagged_events_total: AtomicU64,
    channel_removals_total: AtomicU64,
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
            subscription_auth_failures_total: AtomicU64::new(0),
            stream_lagged_total: AtomicU64::new(0),
            stream_admission_rejections_total: AtomicU64::new(0),
            publish_overload_total: AtomicU64::new(0),
            lagged_events_total: AtomicU64::new(0),
            channel_removals_total: AtomicU64::new(0),
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
    subscription_auth_failures_total: u64,
    stream_lagged_total: u64,
    stream_admission_rejections_total: u64,
    publish_overload_total: u64,
    lagged_events_total: u64,
    channel_removals_total: u64,
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

fn build_router(state: Arc<AppState>) -> Router {
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
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<StatsQuery>,
) -> Result<Response<Body>, StatusCode> {
    authorize_privileged_request(&state, &headers)?;

    let now = now_unix_seconds();
    let verbose = verbose_enabled(query.verbose.as_deref());
    let mut channels = Vec::new();
    let mut active_channels = 0;
    let mut active_subscribers = 0;
    for entry in state.channels.iter() {
        let bus = entry.value();
        let subscribers = bus.sender.receiver_count();
        if subscribers == 0 {
            continue;
        }

        active_channels += 1;
        active_subscribers += subscribers;
        if verbose {
            channels.push(ChannelStatistics {
                idle_seconds: now.saturating_sub(bus.last_used.load(Ordering::Relaxed)),
                name: entry.key().clone(),
                subscribers,
            });
        }
    }
    if verbose {
        channels.sort_by(|left, right| {
            right
                .subscribers
                .cmp(&left.subscribers)
                .then_with(|| left.name.cmp(&right.name))
        });
    }

    let response = StatsResponse {
        subscription_auth_failures_total: state
            .metrics
            .subscription_auth_failures_total
            .load(Ordering::Relaxed),
        stream_lagged_total: state.metrics.stream_lagged_total.load(Ordering::Relaxed),
        stream_admission_rejections_total: state
            .metrics
            .stream_admission_rejections_total
            .load(Ordering::Relaxed),
        publish_overload_total: state.metrics.publish_overload_total.load(Ordering::Relaxed),
        lagged_events_total: state.metrics.lagged_events_total.load(Ordering::Relaxed),
        channel_removals_total: state.metrics.channel_removals_total.load(Ordering::Relaxed),
        active_channels,
        active_stream_connections: state
            .metrics
            .active_stream_connections
            .load(Ordering::Relaxed),
        active_subscribers,
        auth_failures_total: state.metrics.auth_failures_total.load(Ordering::Relaxed),
        channels,
        memory_rss_bytes: read_memory_rss_bytes(),
        publish_requests_total: state.metrics.publish_requests_total.load(Ordering::Relaxed),
        published_bytes_total: state.metrics.published_bytes_total.load(Ordering::Relaxed),
        retained_channels: active_channels,
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
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<PublishRequest>,
) -> impl IntoResponse {
    if !is_valid_channel(&request.channel) {
        return StatusCode::BAD_REQUEST;
    }
    if let Err(status) = authorize_privileged_request(&state, &headers) {
        return status;
    }

    let mut frame = vec![0, 0];
    if let Err(error) = serde_json::to_writer(&mut frame, &request.message) {
        warn!("Failed to encode publish payload: {error}");
        return StatusCode::BAD_REQUEST;
    }
    let Ok(payload_len) = u16::try_from(frame.len() - 2) else {
        return StatusCode::PAYLOAD_TOO_LARGE;
    };
    frame[..2].copy_from_slice(&payload_len.to_be_bytes());

    if let Err(status) = state.broadcast(&request.channel, frame) {
        return status;
    }
    state
        .metrics
        .publish_requests_total
        .fetch_add(1, Ordering::Relaxed);
    state
        .metrics
        .published_bytes_total
        .fetch_add(u64::from(payload_len), Ordering::Relaxed);

    StatusCode::NO_CONTENT
}

async fn stream_channel(
    Path(channel): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(request): Json<StreamRequest>,
) -> Result<Response<Body>, StatusCode> {
    if !is_valid_channel(&channel) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let authorization = verify_token(
        &state.configuration.subscription_secret,
        &channel,
        &request.token,
    )
    .and_then(|claims| subscription_deadline(claims.exp).map(|deadline| (claims.exp, deadline)));
    let (expires_at, deadline) = authorization.inspect_err(|status| {
        if *status == StatusCode::UNAUTHORIZED {
            state
                .metrics
                .auth_failures_total
                .fetch_add(1, Ordering::Relaxed);
            state
                .metrics
                .subscription_auth_failures_total
                .fetch_add(1, Ordering::Relaxed);
        }
    })?;

    let subscription = state.subscribe(channel)?;
    let heartbeat_interval = state.configuration.heartbeat_interval;
    let body_stream = stream! {
        let mut subscription = subscription;
        if now_unix_seconds() >= expires_at || time::Instant::now() >= deadline {
            return;
        }
        yield Result::<Bytes, std::io::Error>::Ok(heartbeat_frame());

        let mut heartbeat = time::interval(heartbeat_interval);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // The initial heartbeat was sent above.
        heartbeat.tick().await;
        let expiration = time::sleep_until(deadline);
        tokio::pin!(expiration);

        loop {
            let received = tokio::select! {
                biased;
                _ = &mut expiration => break,
                _ = heartbeat.tick() => Ok(heartbeat_frame()),
                received = subscription.recv() => received,
            };
            if now_unix_seconds() >= expires_at || time::Instant::now() >= deadline {
                break;
            }
            match received {
                Ok(frame) => yield Ok(frame),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    subscription.record_lag(missed);
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
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
    {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(claims)
}

fn subscription_deadline(expires_at: u64) -> Result<time::Instant, StatusCode> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let remaining = Duration::from_secs(expires_at)
        .checked_sub(now)
        .filter(|duration| !duration.is_zero())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    time::Instant::now()
        .checked_add(remaining)
        .ok_or(StatusCode::UNAUTHORIZED)
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

fn parse_limit(name: &str, default: usize) -> Result<usize, String> {
    match read_required_env(name) {
        Ok(value) => value
            .parse::<usize>()
            .map_err(|error| format!("Invalid {name} value: {error}")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(format!("Invalid {name} value: {error}")),
    }
}

fn valid_heartbeat_interval(interval: Duration) -> bool {
    !interval.is_zero() && interval <= MAX_HEARTBEAT_INTERVAL
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
            "subscription_auth_failures_total: {}",
            response.subscription_auth_failures_total
        ),
        format!(
            "stream_admission_rejections_total: {}",
            response.stream_admission_rejections_total
        ),
        format!("stream_lagged_total: {}", response.stream_lagged_total),
        format!("lagged_events_total: {}", response.lagged_events_total),
        format!(
            "publish_overload_total: {}",
            response.publish_overload_total
        ),
        format!(
            "channel_removals_total: {}",
            response.channel_removals_total
        ),
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
    append_xml_element(
        &mut xml,
        "subscriptionAuthFailuresTotal",
        &response.subscription_auth_failures_total.to_string(),
    );
    append_xml_element(
        &mut xml,
        "streamAdmissionRejectionsTotal",
        &response.stream_admission_rejections_total.to_string(),
    );
    append_xml_element(
        &mut xml,
        "streamLaggedTotal",
        &response.stream_lagged_total.to_string(),
    );
    append_xml_element(
        &mut xml,
        "laggedEventsTotal",
        &response.lagged_events_total.to_string(),
    );
    append_xml_element(
        &mut xml,
        "publishOverloadTotal",
        &response.publish_overload_total.to_string(),
    );
    append_xml_element(
        &mut xml,
        "channelRemovalsTotal",
        &response.channel_removals_total.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn listener_refuses_to_share_an_active_address() {
        let listener = create_listener("127.0.0.1:0".parse().unwrap(), 16).unwrap();
        let address = listener.local_addr().unwrap();
        assert_eq!(
            create_listener(address, 16).unwrap_err().kind(),
            std::io::ErrorKind::AddrInUse
        );
        drop(listener);
        assert!(create_listener(address, 16).is_ok());
    }

    #[test]
    fn token_expiration_is_exclusive() {
        assert_eq!(
            subscription_deadline(now_unix_seconds()),
            Err(StatusCode::UNAUTHORIZED)
        );
    }
}
