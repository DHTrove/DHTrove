# Running DHTrove

DHTrove provides a standalone headless Rust server with its own web
dashboard and authenticated JSON API. Web/API and crawler lifecycles are
independent.

**Important:** Docker startup, OS boot and dashboard visits do NOT start DHT
crawling. The initial state is Stopped, including after a container reboot.
Press Start crawler or invoke the authenticated API to begin.

## Docker Compose

From the repository root:

    docker compose up -d
    docker exec dhtrove cat /data/admin.token

The second command prints the automatically generated API token. Open
http://localhost:8080, paste the token, and choose Start crawler.

The default Compose file publishes HTTP only on the host's loopback interface,
127.0.0.1:8080. Remote access should use HTTPS with a secure reverse proxy or
VPN/tunnel. Do not directly expose the API publicly without TLS.

When DHTROVE_API_TOKEN is explicitly set in the environment/.env file, that
token is used instead of generating a file (minimum 16 characters). The
randomly generated token is stored in a persistent Docker volume with
restrictive Unix permissions.

Default ports and volumes:

- TCP 8080: localhost-only dashboard and JSON API.
- UDP 6881: IPv4 Mainline DHT (only active while crawler is running).
- /data: persistent volume for settings, credentials and storage segments.

When changing the DHT UDP port in Settings, also change the Compose UDP port
mapping. Docker restart never automatically starts the crawler.

The container runs as an unprivileged user (UID 10001); bind-mounted data
folders must be writable by that user.

## Operation

1. Start the container and authenticate. The dashboard says Stopped.
2. Open Settings; adjust UDP port, discovery rate, metadata workers, memory
   queue, maximum metadata size, disk quota and checkpoint cadence.
3. Save settings; they persist to /data/settings.json and can only change
   while the crawler is stopped.
4. Press Start crawler. Discovery and metadata fetch begin; live counters
   update every two seconds.
5. Press Stop crawler. The application signals DHT tasks to stop and awaits
   the disk writer and checkpoint before reporting Stopped.

Activity counts are per session, not historical statistics.

## Authenticated API

Set the header Authorization: Bearer YOUR_API_TOKEN for every /api request.

- GET /api/status: phase, live counters, last error, uptime
- GET /api/settings: current settings
- PUT /api/settings: save validated settings while crawler is stopped
- POST /api/crawler/start: start; HTTP 202 or conflict on repeated start
- POST /api/crawler/stop: request graceful stop; safe to repeat
- GET /healthz: simple unauthenticated process health endpoint

Example:

    TOKEN=$(docker exec dhtrove cat /data/admin.token)
    curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/status
    curl -X POST -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/crawler/start
    curl -X POST -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/crawler/stop

## Standalone headless binary

Install the appropriate GitHub Release archive and run:

    DHTROVE_DATA_DIR=./dhtrove-data DHTROVE_BIND=127.0.0.1:8080 ./dhtrove

Read the generated admin.token in the data directory. On Windows, use the
PowerShell environment variable syntax and run dhtrove.exe.

## Release scheme

The release GitHub Actions workflow runs on relevant pushes to main or
workflow_dispatch. The version comes from crates/dhtrove-server/Cargo.toml.
If that version already has a Release, publishing is skipped. Bump the
version to publish a new release.

For a fresh version (first planned release v0.1.0), GitHub Actions:

- validates Rustfmt, Clippy and all tests;
- builds Linux x86_64, Windows x86_64 and macOS arm64 headless binaries;
- builds a Docker image for linux/amd64 and linux/arm64;
- pushes ghcr.io/dhtrove/dhtrove:vX.Y.Z, :X.Y.Z and :latest;
- creates a normal GitHub Release with binary archives.

All CI and builds happen in GitHub Actions, not on personal servers.

## Scope

This release adds the headless deployment and manual crawler management, not
yet a full-text torrent search catalog or Torznab endpoint. Public-DHT
interoperability and long-term resource usage still need real-world validation.
