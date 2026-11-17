# Deploying and managing DHTrove

DHTrove contains a headless Rust service, a dashboard and an authenticated
JSON API. The crawler remains **stopped** when the executable/container
starts and does not automatically resume after reboot.

## Docker

    docker compose up -d

Open http://localhost:8080 and follow the registration screen.

- At the first launch (no accounts in /data/accounts.sqlite3), the initial
  registration creates the **administrator** account automatically.
- Further registrations create standard member accounts, never additional
  administrators.
- Administrators control the crawler, network/storage settings and whether
  future account registration is allowed.
- Members can sign in and view crawler status and configuration, but cannot
  start/stop indexing or update settings.
- Administrator-only endpoints enforce roles server-side, not just through
  hidden buttons in the web UI.

Account creation uses an atomic SQLite transaction: simultaneous setup
requests cannot create two administrators. Users, password hashes,
registration preferences and revocable sessions survive container restarts.
Passwords are never stored in plaintext: Argon2id with independent salts is
used. Sessions last seven days, are represented by HTTP-only SameSite=Strict
cookies, and store only hashed session secrets in SQLite.

**Security notice:** Whoever first registers on an uninitialized installation
becomes administrator. Keep the dashboard bound to localhost until you create
your first account. If exposing the service over HTTPS, set
DHTROVE_SECURE_COOKIES=true (or 1) in Docker Compose and restart the web
service. This adds the Secure flag to session cookies. Do not expose plain
HTTP sign-in to the internet, and do not trust forwarded TLS headers from
untrusted sources.

The previous admin.token / DHTROVE_API_TOKEN login mechanism has been
completely discontinued. Upgrading an existing v0.1.0 instance **does not
migrate tokens into accounts**; the first visit to v0.2.0 triggers account
creation. Existing torrent metadata and settings remain in the same volume.
An old admin.token file can be removed after migration.

The default Compose file binds the web port to 127.0.0.1:8080/tcp and
publishes the BitTorrent DHT UDP port 6881. Persistent data lives in /data.
The image runs as unprivileged UID 10001. Bind-mounted directories must be
writable by that UID.

## Web UI

The main page shows crawler state, DHT observations, discovered metadata,
queue sizes, and runtime metrics. Start crawler and Stop crawler are manual
admin-only controls. Stopping the crawler does not stop the dashboard.

Settings let an admin adjust UDP port, query rate, worker limits, metadata
size, disk budget and checkpoint interval, while the crawler is stopped.
Settings are persisted in /data/settings.json.

The Users page (administrator only) lists usernames/roles and permits
enabling or disabling additional self-registration. No direct password
recovery workflow, invitation system or multiple-admin promotion UI exists
in this release. Take proper backups of accounts.sqlite3 and its WAL/SHM
files while the service is stopped if preserving user identities matters.

## HTTP API

Public endpoints:
- GET /api/auth/setup — whether the initial administrator is missing and
  whether registration is enabled
- POST /api/auth/register — username/password JSON, creates administrator if
  no users exist, otherwise a standard member; logs the user in
- POST /api/auth/login — username/password JSON, sets a session cookie
- GET /healthz — process health

Authenticated endpoints (session cookie required):
- GET /api/auth/me — current user and role
- POST /api/auth/logout — revoke the current session
- GET /api/status — crawler state and metrics
- GET /api/settings — saved settings

Administrator endpoints:
- PUT /api/settings — save settings while the crawler is stopped
- POST /api/crawler/start — explicitly start DHT discovery
- POST /api/crawler/stop — request graceful shutdown
- GET /api/admin/users — list account names and roles
- POST /api/admin/registration — JSON {"enabled":true|false}

State-changing authenticated endpoints require the X-DHTrove-Request: 1
request header, in addition to the session cookie. The web UI sends it
automatically. Non-GET API responses are not cached and do not disclose
passwords or raw session tokens.

The first setup and password login routes also require JSON request bodies.
The login endpoint is subject to basic attempt throttling (ten failures
per username per five minutes). For public deployments, put a reverse proxy
with TLS, IP-based rate limiting and access controls in front of DHTrove.

## Standalone binary

Install the GitHub Release archive for your platform, then run:

    DHTROVE_DATA_DIR=./dhtrove-data DHTROVE_BIND=127.0.0.1:8080 ./dhtrove

On Windows configure the same environment variables using PowerShell and
run dhtrove.exe. No separate web frontend installation is needed.

## Releases

Pushing a changed version of crates/dhtrove-server/Cargo.toml to main
triggers GitHub Actions. After Rustfmt, Clippy and tests pass, it builds
Linux x86_64, Windows x86_64, macOS arm64 binaries, a linux/amd64 and arm64
GHCR image, and a normal GitHub Release.

Docker tags: ghcr.io/dhtrove/dhtrove:vX.Y.Z, :X.Y.Z and :latest.
Compilation and tests run in GitHub Actions, never on personal servers.

## Remaining work

DHT live network interoperability and long-term throughput benchmarks
have not yet been completed. Search, Torznab, IPv6 and extended operator
controls remain separate future features.
