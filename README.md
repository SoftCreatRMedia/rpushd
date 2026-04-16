# rpushd

This service is a reusable realtime push backend for application integrations.

It keeps long-lived HTTP stream connections outside PHP-FPM and accepts lightweight
publish events from application code.

## Prerequisites

The daemon is intended for Linux servers where you can run an additional service
next to PHP-FPM. It is not meant for shared hosting.

Required:

- A modern Linux distribution
- An application that publishes events to the daemon and mints signed subscribe tokens
- nginx, Apache 2.4, HAProxy, or another reverse proxy in front of the daemon
- A public URL that browsers can reach for the push daemon

Recommended:

- systemd for service management

The included service file targets `systemd`, but the daemon itself is not tied to a
specific distribution. It should work on other Linux distributions as long as you can
run the binary as a long-lived service and expose it through a reverse proxy.

## Linux Installation

Precompiled release binaries are available through GitHub Releases as `.tar.gz`
archives. Current target variants:

- `rpushd-linux-x86_64-gnu.tar.gz`
  - use for `x86_64` Ubuntu, Debian, Arch, and other glibc-based Linux distributions
- `rpushd-linux-x86_64-musl.tar.gz`
  - use for `x86_64` Alpine Linux
- `rpushd-linux-aarch64-gnu.tar.gz`
  - use for `aarch64` / `arm64` Ubuntu, Debian, and other glibc-based Linux distributions
- `rpushd-linux-aarch64-musl.tar.gz`
  - use for `aarch64` / `arm64` Alpine Linux

If you use a release archive, unpack it and continue with the deployment and reverse
proxy steps below. If no suitable precompiled binary exists for your platform, build
from source as described here.

Typical installation from a release archive:

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
```

Replace `rpushd-linux-x86_64-gnu.tar.gz` with the archive that matches
your platform.

Clone the repository and enter the working directory:

```bash
git clone https://github.com/SoftCreatRMedia/rpushd.git
cd rpushd
```

Install the Rust toolchain and basic build dependencies.

Ubuntu / Debian:

```bash
apt update
apt install -y build-essential pkg-config curl ca-certificates
curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"
rustup default stable
rustup component add rustfmt
```

Alpine Linux:

```bash
apk add --no-cache alpine-sdk pkgconf curl ca-certificates rustup
rustup-init -y --profile minimal
. "$HOME/.cargo/env"
rustup default stable
rustup component add rustfmt
```

Arch Linux:

```bash
pacman -Sy --needed base-devel pkgconf curl ca-certificates rustup
rustup default stable
rustup component add rustfmt
```

If your distribution already provides a sufficiently recent Rust toolchain, you can
use that instead. `rustup` is recommended because it keeps the build process
consistent across distributions.

Build the daemon:

```bash
. "$HOME/.cargo/env"
cargo build --release
```

Deploy the binary and supporting files:

```bash
mkdir -p /opt/rpushd
cp target/release/rpushd /opt/rpushd/rpushd
cp rpushd.service /opt/rpushd/
cp nginx-location.conf /opt/rpushd/
```

Generate two long random secrets:

- one for signed browser subscribe tokens
- one for privileged server-side publish requests

Those values must match:

- `RPUSHD_SECRET`
- `RPUSHD_PUBLISH_SECRET`

Recommended:

- store them in a root-readable only environment file instead of hardcoding them
  into the unit itself
- rotate them occasionally
- treat the publish secret as especially sensitive because it authorizes event injection

Install and adjust the unit:

```bash
cp rpushd.service /etc/systemd/system/rpushd.service
editor /etc/systemd/system/rpushd.service
systemctl daemon-reload
systemctl enable --now rpushd
systemctl status rpushd
```

If your distribution does not use `systemd`, use the same binary and environment
variables with the native service manager for that platform instead.

The shipped `systemd` unit already includes a hardened baseline. If you prefer
separate secret storage, replace the inline `Environment=` lines with something like:

```ini
EnvironmentFile=/etc/rpushd.env
```

and store the secrets there with restrictive permissions, for example:

```bash
install -m 600 -o root -g root /dev/null /etc/rpushd.env
editor /etc/rpushd.env
```

Expose the daemon through nginx:

```bash
mkdir -p /etc/nginx/snippets
cp nginx-location.conf /etc/nginx/snippets/rpushd.conf
editor /etc/nginx/sites-enabled/your-site.conf
nginx -t
systemctl reload nginx
```

Inside the relevant nginx `server { ... }` block, add:

```nginx
include snippets/rpushd.conf;
```

This keeps the daemon routing in a dedicated snippet, so future updates only need
to replace `/etc/nginx/snippets/rpushd.conf` instead of manually
copying directives into every virtual host configuration.

Apache 2.4 works as well. Enable the required modules first:

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

Keep `/api/publish` and `/api/stats` internal-only there as well. Trusted
application or admin tooling should call those endpoints directly via the internal
daemon URL instead of exposing them through Apache.

HAProxy works as well. A typical frontend/backend split looks like this:

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

Expose only the public stream and health paths through that public HAProxy route.
Do not proxy `/api/publish` or `/api/stats` publicly. Let trusted application or
admin tooling call those endpoints directly through the internal daemon URL instead.

Then configure your application so that:

- browsers use the public stream base URL, for example `https://your-domain.tld/push-daemon`
- server-side publish requests target the internal daemon URL, for example `http://127.0.0.1:45831`
- subscribe tokens are signed with `RPUSHD_SECRET`
- privileged publish requests use `RPUSHD_PUBLISH_SECRET`

If you want runtime statistics, query the daemon directly on the internal address.
Do not expose the stats endpoint publicly.

## Operational Security

For a strong production setup, keep these points in mind:

- bind the daemon only to `127.0.0.1` or another private interface
- never expose the raw daemon port directly to the internet
- proxy only `/healthz` and `/api/stream/` publicly
- keep `/api/publish` and `/api/stats` internal-only
- call `/api/publish` only from trusted application code
- call `/api/stats` only from trusted internal admin tooling
- store secrets outside the service unit if possible
- rotate secrets with a planned deployment window

Suggested rotation order:

1. rotate the publish secret
2. update the application publish side
3. verify publishing still works
4. rotate the subscription secret
5. allow old subscribe tokens to expire

If publish traffic ever has to cross hosts, prefer a private network, VPN, IP
allowlisting, or mTLS in front of the daemon rather than exposing publish traffic
openly on the public internet.

## Build

```bash
cargo build --release
```

## Run

```bash
export RPUSHD_SECRET='replace-with-a-long-random-secret'
export RPUSHD_PUBLISH_SECRET='replace-with-a-different-long-random-secret'
export RPUSHD_LISTEN='127.0.0.1:45831'
cargo run --release
```

Optional environment variables:

- `RPUSHD_HEARTBEAT_SECS`
  Default: `15`
- `RPUSHD_CHANNEL_IDLE_TTL_SECS`
  Default: `3600`

## HTTP API

- `GET /healthz`
- `POST /api/publish`
- `GET /api/stats`
- `POST /api/stream/{channel}`

`/api/publish` expects:

```json
{
  "channel": "example-channel",
  "message": {
    "foo": "bar"
  }
}
```

with header:

```text
Authorization: Bearer <publish-secret>
```

`/api/stream/{channel}` expects:

```json
{
  "token": "<signed-subscribe-token>"
}
```

The response is an `application/octet-stream` body using the same two-byte
big-endian length prefix that the browser-side `PushClient` already understands.
Zero-length frames are heartbeats.

`/api/stats` expects:

```text
Authorization: Bearer <publish-secret>
```

Without a `mode` parameter, it returns human-readable plain text.

Supported output modes:

- default / no `mode`: plain text
- `?mode=json`
- `?mode=xml`

It includes metrics such as:

- uptime
- active stream connections
- total stream connections opened
- publish request count
- published byte count
- current RSS memory usage
- channel count and per-channel subscriber counts

Example:

```bash
curl -sS \
  -H 'Authorization: Bearer replace-with-the-publish-secret' \
  http://127.0.0.1:45831/api/stats
```

Example plain-text response:

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

JSON:

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

XML:

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

## Reverse Proxy

The browser-facing daemon URL should usually be exposed through nginx, Apache 2.4,
HAProxy,
or another reverse proxy. A minimal nginx location is included in
[nginx-location.conf](./nginx-location.conf).
It intentionally exposes only the public stream and health endpoints. Keep
`/api/publish` and `/api/stats` internal-only and let trusted application or admin
tooling call the daemon directly via the internal daemon URL. The recommended
setup is to install that file as an nginx snippet and reference it from your
`server` block via `include snippets/rpushd.conf;`.

Typical setup:

- the public stream base URL points to the browser-facing URL, usually `https://your-domain.tld/push-daemon`
- privileged publish requests target the local daemon directly, for example `http://127.0.0.1:45831`

## Monitoring

At minimum, watch these signals:

- active stream count
- reconnect rate
- publish request rate
- `401`, `403`, and `429` responses at the proxy layer
- daemon restarts or crashes

Useful operational checks:

- `systemctl status rpushd`
- `journalctl -u rpushd -f`
- reverse proxy access/error logs for `/push-daemon/`

If you expect large traffic, set alerts for sudden reconnect spikes or sustained
auth failures. Those often indicate proxy buffering/timeouts, abusive clients, or
misconfigured secrets.

## systemd

A sample unit file is included in [rpushd.service](./rpushd.service).

## Uninstall

If the daemon is currently used by an application, disable that integration first.

Then remove the service and reverse proxy configuration:

```bash
systemctl disable --now rpushd
rm -f /etc/systemd/system/rpushd.service
systemctl daemon-reload

editor /etc/nginx/sites-enabled/your-site.conf
rm -f /etc/nginx/snippets/rpushd.conf
nginx -t
systemctl reload nginx
```

Finally remove the deployed daemon files if you no longer need them:

```bash
rm -rf /opt/rpushd
```

## License

Copyright by SoftCreatR.dev.

License terms:

- https://softcreatr.dev/license-terms
