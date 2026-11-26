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

use std::net::SocketAddr;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use http_body_util::BodyExt;
use rpushd::{Configuration, build_app};
use serde_json::{Value, json};
use sha2::Sha256;
use tower::ServiceExt;

type HmacSha256 = Hmac<Sha256>;

fn test_configuration(heartbeat: Duration) -> Configuration {
    Configuration::new(
        "127.0.0.1:45831".parse::<SocketAddr>().unwrap(),
        "subscription-secret",
        "publish-secret",
        heartbeat,
        Duration::ZERO,
    )
}

fn test_app() -> axum::Router {
    build_app(test_configuration(Duration::from_secs(15)))
}

fn now_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn subscribe_token(channel: &str, secret: &str, expires_in_secs: u64) -> String {
    subscribe_token_until(channel, secret, now_unix_seconds() + expires_in_secs)
}

fn subscribe_token_until(channel: &str, secret: &str, expires_at: u64) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "aud": "rpushd",
            "channel": channel,
            "exp": expires_at,
            "scope": "subscribe"
        }))
        .unwrap(),
    );
    let signing_input = format!("{header}.{payload}");
    let mut mac = <HmacSha256 as KeyInit>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(signing_input.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());

    format!("{signing_input}.{signature}")
}

#[tokio::test]
async fn healthz_returns_ok() {
    let response = test_app()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "application/json"
    );
}

#[tokio::test]
async fn stats_requires_publish_secret() {
    let response = test_app()
        .oneshot(
            Request::builder()
                .uri("/api/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn stats_supports_text_json_and_xml_modes() {
    let app = test_app();

    let text_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/stats")
                .header(AUTHORIZATION, "Bearer publish-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(text_response.status(), 200);
    assert_eq!(
        text_response.headers().get(CONTENT_TYPE).unwrap(),
        "text/plain; charset=utf-8"
    );
    let text_body = String::from_utf8(
        to_bytes(text_response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(text_body.contains("rpushd stats"));

    let json_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/stats?mode=json")
                .header(AUTHORIZATION, "Bearer publish-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json_response.status(), 200);
    assert_eq!(
        json_response.headers().get(CONTENT_TYPE).unwrap(),
        "application/json; charset=utf-8"
    );
    let json_body: Value = serde_json::from_slice(
        &to_bytes(json_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(json_body["active_channels"], 0);

    let xml_response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats?mode=xml")
                .header(AUTHORIZATION, "Bearer publish-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(xml_response.status(), 200);
    assert_eq!(
        xml_response.headers().get(CONTENT_TYPE).unwrap(),
        "application/xml; charset=utf-8"
    );
    let xml_body = String::from_utf8(
        to_bytes(xml_response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(xml_body.contains("<stats>"));
}

#[tokio::test]
async fn publish_requires_secret_and_updates_stats() {
    let app = test_app();

    let unauthorized = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/publish")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "channel": "thread-posts:459",
                        "message": { "postID": 123 }
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);

    let authorized = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/publish")
                .header(AUTHORIZATION, "Bearer publish-secret")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "channel": "thread-posts:459",
                        "message": { "postID": 123 }
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(authorized.status(), 204);

    let stats = app
        .oneshot(
            Request::builder()
                .uri("/api/stats?mode=json")
                .header(AUTHORIZATION, "Bearer publish-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let stats_body: Value =
        serde_json::from_slice(&to_bytes(stats.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(stats_body["publish_requests_total"], 1);
    assert_eq!(stats_body["active_channels"], 0);
    assert_eq!(stats_body["retained_channels"], 0);
}

#[tokio::test]
async fn stream_requires_valid_token_and_emits_heartbeat() {
    let channel = "thread-writers:459";
    let token = subscribe_token(channel, "subscription-secret", 60);
    let app = test_app();

    let unauthorized = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/stream/{channel}"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "token": "invalid" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/stream/{channel}"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "token": token })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "application/octet-stream"
    );

    let mut body = response.into_body();
    let first_frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(first_frame.as_ref(), &[0, 0]);
}

async fn open_stream(app: &axum::Router, channel: &str, token: &str) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::post(format!("/api/stream/{channel}"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "token": token }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn publish_event(app: &axum::Router, channel: &str, message: Value) -> StatusCode {
    app.clone()
        .oneshot(
            Request::post("/api/publish")
                .header(AUTHORIZATION, "Bearer publish-secret")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "channel": channel, "message": message }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn stats_json(app: &axum::Router, verbose: bool) -> Value {
    let response = app
        .clone()
        .oneshot(
            Request::get(format!("/api/stats?mode=json&verbose={verbose}"))
                .header(AUTHORIZATION, "Bearer publish-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

async fn read_frame(body: &mut Body) -> bytes::Bytes {
    tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("stream did not respond")
        .expect("stream ended early")
        .unwrap()
        .into_data()
        .unwrap()
}

#[tokio::test]
async fn unpolled_streams_receive_publishes_and_release_the_last_channel() {
    let app = test_app();
    let channel = "live";
    let token = subscribe_token(channel, "subscription-secret", 60);
    let unpolled = open_stream(&app, channel, &token).await;
    let second = open_stream(&app, channel, &token).await;
    assert_eq!(unpolled.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);

    let stats = stats_json(&app, true).await;
    assert_eq!(stats["active_stream_connections"], 2);
    assert_eq!(stats["channels"][0]["subscribers"], 2);
    drop(second);

    assert_eq!(
        publish_event(&app, channel, json!(42)).await,
        StatusCode::NO_CONTENT
    );
    let mut body = unpolled.into_body();
    assert_eq!(read_frame(&mut body).await.as_ref(), &[0, 0]);
    assert_eq!(read_frame(&mut body).await.as_ref(), b"\x00\x0242");
    drop(body);

    let stats = stats_json(&app, true).await;
    assert_eq!(stats["active_stream_connections"], 0);
    assert_eq!(stats["active_subscribers"], 0);
    assert_eq!(stats["retained_channels"], 0);
    assert_eq!(stats["channel_removals_total"], 1);
    assert_eq!(stats["channels"], json!([]));

    let replacement = open_stream(&app, channel, &token).await;
    assert_eq!(replacement.status(), StatusCode::OK);
    assert_eq!(
        publish_event(&app, channel, json!(99)).await,
        StatusCode::NO_CONTENT
    );
    let mut body = replacement.into_body();
    assert_eq!(read_frame(&mut body).await.as_ref(), &[0, 0]);
    assert_eq!(read_frame(&mut body).await.as_ref(), b"\x00\x0299");
}

#[tokio::test]
async fn expiry_ends_an_idle_stream_and_rejects_the_old_token() {
    let app = test_app();
    let channel = "short-lived";
    let expires_at = now_unix_seconds() + 2;
    let token = subscribe_token_until(channel, "subscription-secret", expires_at);
    let response = open_stream(&app, channel, &token).await;
    let unpolled = open_stream(&app, channel, &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(unpolled.status(), StatusCode::OK);

    let mut body = response.into_body();
    assert_eq!(read_frame(&mut body).await.as_ref(), &[0, 0]);
    assert!(
        tokio::time::timeout(Duration::from_secs(3), body.frame())
            .await
            .expect("stream outlived token")
            .is_none()
    );

    assert_eq!(
        publish_event(&app, channel, json!(42)).await,
        StatusCode::NO_CONTENT
    );
    assert!(unpolled.into_body().frame().await.is_none());
    assert_eq!(
        open_stream(&app, channel, &token).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let stats = stats_json(&app, false).await;
    assert_eq!(stats["auth_failures_total"], 1);
    assert_eq!(stats["subscription_auth_failures_total"], 1);
    assert_eq!(stats["active_stream_connections"], 0);
    assert_eq!(stats["retained_channels"], 0);
}

#[tokio::test]
async fn lag_closes_the_stream_without_sending_the_remaining_backlog() {
    let app = test_app();
    let channel = "lagging";
    let token = subscribe_token(channel, "subscription-secret", 60);
    let mut body = open_stream(&app, channel, &token).await.into_body();
    assert_eq!(read_frame(&mut body).await.as_ref(), &[0, 0]);

    for event in 0..513 {
        assert_eq!(
            publish_event(&app, channel, json!(event)).await,
            StatusCode::NO_CONTENT
        );
    }

    assert!(body.frame().await.is_none());
    let stats = stats_json(&app, false).await;
    assert_eq!(stats["stream_lagged_total"], 1);
    assert_eq!(stats["lagged_events_total"], 1);
    assert_eq!(stats["active_stream_connections"], 0);
    assert_eq!(stats["retained_channels"], 0);
}

#[tokio::test]
async fn admission_bounds_unpolled_responses_and_releases_slots_on_disconnect() {
    let app = build_app(
        test_configuration(Duration::from_secs(15))
            .with_limits(2, 1, 16_384)
            .unwrap(),
    );
    let first_token = subscribe_token("first", "subscription-secret", 60);
    let second_token = subscribe_token("second", "subscription-secret", 60);
    let first = open_stream(&app, "first", &first_token).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(
        open_stream(&app, "second", &second_token).await.status(),
        StatusCode::SERVICE_UNAVAILABLE,
    );
    let second = open_stream(&app, "first", &first_token).await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(
        open_stream(&app, "first", &first_token).await.status(),
        StatusCode::SERVICE_UNAVAILABLE,
    );
    assert_eq!(
        stats_json(&app, false).await["stream_admission_rejections_total"],
        2
    );

    drop(first);
    drop(second);
    assert_eq!(
        open_stream(&app, "second", &second_token).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn byte_budget_is_shared_and_released_after_the_last_frame_reference() {
    let app = build_app(
        test_configuration(Duration::from_secs(15))
            .with_limits(4, 2, 4)
            .unwrap(),
    );
    let token = subscribe_token("shared", "subscription-secret", 60);
    let other_token = subscribe_token("other", "subscription-secret", 60);
    let mut first = open_stream(&app, "shared", &token).await.into_body();
    let mut second = open_stream(&app, "shared", &token).await.into_body();
    let _other = open_stream(&app, "other", &other_token).await;
    assert_eq!(read_frame(&mut first).await.as_ref(), &[0, 0]);
    assert_eq!(read_frame(&mut second).await.as_ref(), &[0, 0]);

    assert_eq!(
        publish_event(&app, "shared", json!(42)).await,
        StatusCode::NO_CONTENT
    );
    let first_frame = read_frame(&mut first).await;
    assert_eq!(first_frame.as_ref(), b"\x00\x0242");
    assert_eq!(
        publish_event(&app, "other", json!(42)).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(read_frame(&mut second).await, first_frame);
    assert_eq!(
        publish_event(&app, "other", json!(42)).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    drop(first_frame);

    assert_eq!(
        publish_event(&app, "other", json!(42)).await,
        StatusCode::NO_CONTENT
    );
    let stats = stats_json(&app, false).await;
    assert_eq!(stats["publish_overload_total"], 2);
    assert_eq!(stats["publish_requests_total"], 2);
}

#[tokio::test]
async fn an_embedded_zero_heartbeat_uses_a_safe_default() {
    let app = build_app(test_configuration(Duration::ZERO));
    let token = subscribe_token("heartbeat", "subscription-secret", 60);
    let mut body = open_stream(&app, "heartbeat", &token).await.into_body();
    assert_eq!(read_frame(&mut body).await.as_ref(), &[0, 0]);
    assert_eq!(
        publish_event(&app, "heartbeat", json!(42)).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(read_frame(&mut body).await.as_ref(), b"\x00\x0242");
}
