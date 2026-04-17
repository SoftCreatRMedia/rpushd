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
use axum::http::Request;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use http_body_util::BodyExt;
use rpushd::{Configuration, build_app};
use serde_json::{Value, json};
use sha2::Sha256;
use tower::ServiceExt;

type HmacSha256 = Hmac<Sha256>;

fn test_app() -> axum::Router {
    build_app(Configuration::new(
        "127.0.0.1:45831".parse::<SocketAddr>().unwrap(),
        "subscription-secret",
        "publish-secret",
        Duration::from_secs(15),
        Duration::from_secs(3600),
    ))
}

fn now_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn subscribe_token(channel: &str, secret: &str, expires_in_secs: u64) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "aud": "rpushd",
            "channel": channel,
            "exp": now_unix_seconds() + expires_in_secs,
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
    assert_eq!(stats_body["retained_channels"], 1);
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
