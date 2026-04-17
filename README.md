# rpushd

`rpushd` is a reusable realtime push daemon for application integrations.

It keeps long-lived HTTP stream connections outside PHP-FPM and accepts
lightweight publish events from trusted application code.

## What It Does

- accepts signed browser subscriptions for named channels
- accepts authenticated publish requests from trusted server-side code
- streams framed realtime messages to subscribed clients
- exposes health and internal statistics endpoints

## Requirements

`rpushd` is intended for Linux systems where you can run an additional long-lived
service next to your application stack.

Required:

- a modern Linux distribution
- an application that:
  - publishes events to `rpushd`
  - mints signed subscribe tokens for browser clients
- a reverse proxy in front of the daemon
  - nginx
  - Apache 2.4
  - HAProxy
  - or an equivalent proxy
- a browser-facing public URL for the stream endpoint

Recommended:

- `systemd` for service management

## Installation

Choose one of these installation paths:

1. install from a precompiled release archive
2. build from source

### Install From A Precompiled Release Archive

Release archives are published on GitHub Releases.

Each archive already contains:

- `rpushd`
- `rpushd.service`
- `nginx-location.conf`
- `README.md`

Current release variants:

- `rpushd-linux-x86_64-gnu.tar.gz`
  - for `x86_64` Ubuntu, Debian, Arch, and other glibc-based Linux distributions
- `rpushd-linux-x86_64-musl.tar.gz`
  - for `x86_64` Alpine Linux
- `rpushd-linux-aarch64-gnu.tar.gz`
  - for `aarch64` / `arm64` Ubuntu, Debian, and other glibc-based Linux distributions
- `rpushd-linux-aarch64-musl.tar.gz`
  - for `aarch64` / `arm64` Alpine Linux

Example installation:

```bash
curl -LO https://github.com/SoftCreatRMedia/rpushd/releases/latest/download/rpushd-linux-x86_64-gnu.tar.gz
curl -LO https://github.com/SoftCreatRMedia/rpushd/releases/latest/download/rpushd-linux-x86_64-gnu.tar.gz.sha256
sha256sum -c rpushd-linux-x86_64-gnu.tar.gz.sha256
tar -xzf rpushd-linux-x86_64-gnu.tar.gz

mkdir -p /opt/rpushd
cp rpushd-linux-x86_64-gnu/rpushd /opt/rpushd/rpushd
cp rpushd-linux-x86_64-gnu/rpushd.service /opt/rpushd/
cp rpushd-linux-x86_64-gnu/nginx-location.conf /opt/rpushd/
cp rpushd-linux-x86_64-gnu/README.md /opt/rpushd/

cd /opt/rpushd
```

Replace `rpushd-linux-x86_64-gnu.tar.gz` with the archive that matches your
platform.

After that, continue with `Configuration`, `Service Setup`, and `Reverse Proxy`.

### Build From Source

Clone the repository:

```bash
git clone https://github.com/SoftCreatRMedia/rpushd.git
cd rpushd
```

Install a Rust toolchain and basic build dependencies.

Ubuntu / Debian:

```bash
apt update
apt install -y build-essential pkg-config curl ca-certificates
curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"
rustup default stable
rustup component add rustfmt
```

Alpine:

```bash
apk add --no-cache alpine-sdk pkgconf curl ca-certificates rustup
rustup-init -y --profile minimal
. "$HOME/.cargo/env"
rustup default stable
rustup component add rustfmt
```

Arch:

```bash
pacman -Sy --needed base-devel pkgconf curl ca-certificates rustup
rustup default stable
rustup component add rustfmt
```

Build:

```bash
. "$HOME/.cargo/env"
cargo build --release
```

Deploy the built files:

```bash
mkdir -p /opt/rpushd
cp target/release/rpushd /opt/rpushd/rpushd
cp rpushd.service /opt/rpushd/
cp nginx-location.conf /opt/rpushd/
cp README.md /opt/rpushd/

cd /opt/rpushd
```

After that, continue with `Configuration`, `Service Setup`, and `Reverse Proxy`.

## Configuration

`rpushd` uses two different secrets:

- `RPUSHD_SECRET`
  - used to verify signed browser subscribe tokens
- `RPUSHD_PUBLISH_SECRET`
  - used to authenticate privileged publish and stats requests

Optional environment variables:

- `RPUSHD_LISTEN`
  - default: `127.0.0.1:45831`
- `RPUSHD_HEARTBEAT_SECS`
  - default: `15`
- `RPUSHD_CHANNEL_IDLE_TTL_SECS`
  - default: `3600`

Generate strong random secrets before starting the daemon.

Tip:

- if Python 3 is available, this works on Linux, macOS, and Windows:

```bash
python -c "import secrets; print(secrets.token_urlsafe(64))"
```

- run it twice and use different values for:
  - `RPUSHD_SECRET`
  - `RPUSHD_PUBLISH_SECRET`

Recommended:

- keep the daemon bound to `127.0.0.1` or another private interface
- store secrets in a dedicated environment file instead of hardcoding them into
  the unit file
- treat `RPUSHD_PUBLISH_SECRET` as especially sensitive because it authorizes
  event injection

Example environment file:

```bash
install -m 600 -o root -g root /dev/null /etc/rpushd.env
editor /etc/rpushd.env
```

Example `/etc/rpushd.env` contents:

```ini
RPUSHD_LISTEN=127.0.0.1:45831
RPUSHD_SECRET=replace-with-a-long-random-secret
RPUSHD_PUBLISH_SECRET=replace-with-a-different-long-random-secret
RPUSHD_HEARTBEAT_SECS=15
RPUSHD_CHANNEL_IDLE_TTL_SECS=3600
```

## Service Setup

The repository ships a hardened `systemd` unit.

Install it:

```bash
cp /opt/rpushd/rpushd.service /etc/systemd/system/rpushd.service
editor /etc/systemd/system/rpushd.service
```

Replace the inline `Environment=` lines with:

```ini
EnvironmentFile=/etc/rpushd.env
```

Then enable and start the service:

```bash
systemctl daemon-reload
systemctl enable --now rpushd
systemctl status rpushd
```

If your system does not use `systemd`, run the same binary with the same
environment variables through your native service manager.

## Reverse Proxy

Only expose these endpoints publicly:

- `/healthz`
- `/api/stream/`

Do not expose these endpoints publicly:

- `/api/publish`
- `/api/stats`

Trusted application code or internal admin tooling should call those endpoints
directly through the internal daemon address, for example:

- `http://127.0.0.1:45831/api/publish`
- `http://127.0.0.1:45831/api/stats`

### nginx

Install the shipped snippet:

```bash
mkdir -p /etc/nginx/snippets
cp /opt/rpushd/nginx-location.conf /etc/nginx/snippets/rpushd.conf
editor /etc/nginx/sites-enabled/your-site.conf
```

Inside the relevant `server { ... }` block, add:

```nginx
include snippets/rpushd.conf;
```

Validate and reload:

```bash
nginx -t
systemctl reload nginx
```

The shipped nginx snippet intentionally documents rate limiting and only exposes
the public stream and health endpoints.

### Apache 2.4

Enable the required modules:

```bash
a2enmod proxy proxy_http headers ssl
systemctl reload apache2
```

Then add something like this to the relevant `VirtualHost`:

```apache
ProxyPreserveHost On
ProxyTimeout 75

ProxyPass        /push-daemon/healthz http://127.0.0.1:45831/healthz timeout=15 keepalive=On
ProxyPassReverse /push-daemon/healthz http://127.0.0.1:45831/healthz

ProxyPass        /push-daemon/api/stream/ http://127.0.0.1:45831/api/stream/ timeout=75 keepalive=On
ProxyPassReverse /push-daemon/api/stream/ http://127.0.0.1:45831/api/stream/

<Location "/push-daemon/api/stream/">
    Header always set Cache-Control "no-cache, no-store, must-revalidate, no-transform"
    Header always set Pragma "no-cache"
    Header always set Expires "0"
</Location>
```

Keep `/api/publish` and `/api/stats` internal-only here as well.

### HAProxy

A typical frontend/backend split looks like this:

```haproxy
frontend https_in
    bind *:443 ssl crt /etc/haproxy/certs alpn h2,http/1.1
    mode http

    acl path_push_daemon path_beg /push-daemon/
    use_backend push_daemon if path_push_daemon

backend push_daemon
    mode http
    option forwardfor
    http-reuse safe
    timeout server 75s
    timeout tunnel 75s
    server local_push 127.0.0.1:45831 check
```

Again, expose only the public stream and health paths.

## Application Integration

Your application should be configured so that:

- browsers use the public stream base URL
  - for example: `https://your-domain.tld/push-daemon`
- privileged publish requests target the internal daemon URL
  - for example: `http://127.0.0.1:45831`
- subscribe tokens are signed with `RPUSHD_SECRET`
- privileged publish and stats requests use `RPUSHD_PUBLISH_SECRET`

## HTTP API

Available endpoints:

- `GET /healthz`
- `POST /api/publish`
- `GET /api/stats`
- `POST /api/stream/{channel}`

### `POST /api/publish`

Request body:

```json
{
  "channel": "example-channel",
  "message": {
    "foo": "bar"
  }
}
```

Required header:

```text
Authorization: Bearer <publish-secret>
```

### `POST /api/stream/{channel}`

Request body:

```json
{
  "token": "<signed-subscribe-token>"
}
```

The response is an `application/octet-stream` body using a two-byte big-endian
length prefix. Zero-length frames are heartbeats.

### `GET /api/stats`

Required header:

```text
Authorization: Bearer <publish-secret>
```

Supported output modes:

- no `mode` parameter: plain text
- `?mode=json`
- `?mode=xml`

Example plain text request:

```bash
curl -sS \
  -H 'Authorization: Bearer replace-with-the-publish-secret' \
  http://127.0.0.1:45831/api/stats
```

Example plain text response:

```text
started_at: 1776181200
uptime_seconds: 842
active_channels: 3
active_subscribers: 7
active_stream_connections: 7
stream_connections_total: 24
publish_requests_total: 18
published_bytes_total: 2914
auth_failures_total: 0
memory_rss_bytes: 7348224
channels:
  - name: notifications:96501
    subscribers: 1
    idle_seconds: 3
  - name: thread-posts:459
    subscribers: 3
    idle_seconds: 1
  - name: thread-writers:459
    subscribers: 3
    idle_seconds: 0
```

Example JSON request:

```bash
curl -sS \
  -H 'Authorization: Bearer replace-with-the-publish-secret' \
  'http://127.0.0.1:45831/api/stats?mode=json' | jq
```

Example JSON response:

```json
{
  "active_channels": 3,
  "active_stream_connections": 7,
  "active_subscribers": 7,
  "auth_failures_total": 0,
  "channels": [
    {
      "idle_seconds": 3,
      "name": "notifications:96501",
      "subscribers": 1
    },
    {
      "idle_seconds": 1,
      "name": "thread-posts:459",
      "subscribers": 3
    },
    {
      "idle_seconds": 0,
      "name": "thread-writers:459",
      "subscribers": 3
    }
  ],
  "memory_rss_bytes": 7348224,
  "publish_requests_total": 18,
  "published_bytes_total": 2914,
  "started_at": 1776181200,
  "stream_connections_total": 24,
  "uptime_seconds": 842
}
```

Example XML request:

```bash
curl -sS \
  -H 'Authorization: Bearer replace-with-the-publish-secret' \
  'http://127.0.0.1:45831/api/stats?mode=xml'
```

Example XML response:

```xml
<stats>
  <started_at>1776181200</started_at>
  <uptime_seconds>842</uptime_seconds>
  <active_channels>3</active_channels>
  <active_subscribers>7</active_subscribers>
  <active_stream_connections>7</active_stream_connections>
  <stream_connections_total>24</stream_connections_total>
  <publish_requests_total>18</publish_requests_total>
  <published_bytes_total>2914</published_bytes_total>
  <auth_failures_total>0</auth_failures_total>
  <memory_rss_bytes>7348224</memory_rss_bytes>
  <channels>
    <channel>
      <name>notifications:96501</name>
      <subscribers>1</subscribers>
      <idle_seconds>3</idle_seconds>
    </channel>
    <channel>
      <name>thread-posts:459</name>
      <subscribers>3</subscribers>
      <idle_seconds>1</idle_seconds>
    </channel>
    <channel>
      <name>thread-writers:459</name>
      <subscribers>3</subscribers>
      <idle_seconds>0</idle_seconds>
    </channel>
  </channels>
</stats>
```

## Monitoring

Useful operational checks:

- `systemctl status rpushd`
- `journalctl -u rpushd -f`
- reverse-proxy access and error logs for `/push-daemon/`
- `GET /api/stats` from trusted internal tooling

At minimum, watch these signals:

- active stream count
- reconnect rate
- publish request rate
- daemon restarts or crashes
- `401`, `403`, and `429` responses at the proxy layer

## Operational Security

For a strong production setup:

- bind the daemon only to `127.0.0.1` or another private interface
- never expose the raw daemon port directly to the internet
- proxy only `/healthz` and `/api/stream/` publicly
- keep `/api/publish` and `/api/stats` internal-only
- call `/api/publish` only from trusted application code
- call `/api/stats` only from trusted internal admin tooling
- store secrets outside the service unit if possible
- rotate secrets during a planned deployment window

Suggested rotation order:

1. rotate the publish secret
2. update the application publish side
3. verify publish requests still work
4. rotate the subscription secret
5. allow old subscribe tokens to expire

If publish traffic ever has to cross hosts, prefer a private network, VPN, IP
allowlisting, or mTLS rather than exposing privileged daemon traffic publicly.

## Uninstall

If an application currently uses `rpushd`, disable that integration first.

Then remove the service and reverse-proxy configuration:

```bash
systemctl disable --now rpushd
rm -f /etc/systemd/system/rpushd.service
systemctl daemon-reload

editor /etc/nginx/sites-enabled/your-site.conf
rm -f /etc/nginx/snippets/rpushd.conf
nginx -t
systemctl reload nginx
```

Finally remove the deployed files:

```bash
rm -rf /opt/rpushd
rm -f /etc/rpushd.env
```

## License

Copyright by SoftCreatR.dev.

License terms:

- https://softcreatr.dev/license-terms
