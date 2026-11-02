# ixian

Minimal yt-dlp wrapper server written in Rust.
Downloads videos from **YouTube**, **x.com/twitter.com**, **Instagram** and **TikTok**, with per-platform cookie support.

## Requirements

- [Rust](https://rustup.rs) (edition 2021)
- [yt-dlp](https://github.com/yt-dlp/yt-dlp) (`brew install yt-dlp`)
- [ffmpeg](https://ffmpeg.org) (needed to merge best video + audio; `brew install ffmpeg`)

## Run

```sh
cargo run --release
```

Listens on `http://127.0.0.1:3000` by default.

### Docker

Every GitHub release publishes a multi-arch (`linux/amd64`, `linux/arm64`) image to GitHub Container Registry:

```sh
docker run -d --name ixian -p 127.0.0.1:3000:3000 --restart unless-stopped \
  ghcr.io/zeromask1337/ixian:latest
```

Tags per release: `latest` (newest stable release), `0.1.1` and `0.1` (semver) and `v0.1.1` (same as the release tag). For immutable deploys pin the digest:

```sh
docker pull ghcr.io/zeromask1337/ixian@sha256:<digest>
```

The digest for each release is in the **Publish image** workflow run summary (Actions -> Publish image).

To build locally instead:

```sh
docker build -t ixian .
docker run -d --name ixian -p 127.0.0.1:3000:3000 --restart unless-stopped ixian
```

Multi-stage build: `rust:1.99` compiles the binary, `node:24-bookworm-slim` runs it with Node
(yt-dlp's JS runtime for YouTube challenges), yt-dlp (PyPI + EJS) and ffmpeg. `HOST=0.0.0.0`,
`YTDLP_PATH`, `YTDLP_PLUGIN_DIR`, `IXIAN_COOKIE_DIR=/data/cookies` and the yt-dlp config
(`--js-runtimes node`) are preset. Any [config](#config-env) var can be overridden with `-e`,
e.g. `-e YTDLP_FORMAT='bv*+ba/b'`.

Mount a volume at `/data/cookies` to persist the [cookie store](#cookie-store) across restarts:

```sh
docker run -d --name ixian -p 127.0.0.1:3000:3000 --restart unless-stopped \
  -v ixian-cookies:/data/cookies \
  -e IXIAN_TOKEN='change-me' \
  ghcr.io/zeromask1337/ixian:latest
```

For a bind mount, make it writable by the container user (uid/gid `1000`):
`mkdir -p ./cookies && sudo chown 1000:1000 ./cookies && chmod 700 ./cookies`, then
`-v "$PWD/cookies:/data/cookies"`.

## API

### `GET /download`

| param | description |
| --- | --- |
| `url` | **required** video URL; must be `http(s)` and match a supported platform host |
| `platform` | optional override: `youtube`, `x`, `instagram`, `tiktok` (otherwise detected from the URL). The URL must still belong to that platform |
| `format` | optional yt-dlp format selector (default: best mp4) |
| `timeout_ms` | optional download timeout in milliseconds (default `600000`, or `YTDLP_TIMEOUT_MS`); `timeoutMs` is accepted as an alias |
| `inline` | `1` serves `content-disposition: inline` for browser playback (default: attachment) |

Cookies are **not** accepted on `GET`: they would leak into access logs and browser
history. A `*_cookies` parameter returns `400` with a hint to use `POST /download`.
Stored cookies from the [cookie store](#cookie-store) are still used automatically.

```sh
curl -G http://127.0.0.1:3000/download \
  --data-urlencode 'url=https://www.youtube.com/watch?v=dQw4w9WgXcQ' \
  -o video.mp4
```

### `POST /download`

Same params in a JSON body (`timeout_ms`, canonical; `timeoutMs` also accepted). Prefer this for cookies (query strings have length limits). Cookies go in `cookies: { "<platform>": "..." }` or `<platform>_cookies`.

```sh
# public posts work without cookies
curl -X POST http://127.0.0.1:3000/download \
  -H 'content-type: application/json' \
  -d '{"url":"https://www.instagram.com/reel/xxxx/"}' \
  -o video.mp4
```

Cookies are only needed for login/age-restricted content — use real ones from a logged-in session, never placeholders (bad cookies make yt-dlp fail where an anonymous download would have worked).

To send a real cookie file, JSON-encode it:

```sh
jq -n --rawfile c cookies.txt \
  '{url:"https://x.com/user/status/123", cookies:{x:$c}}' |
  curl -X POST http://127.0.0.1:3000/download \
    -H 'content-type: application/json' --data-binary @- -o video.mp4
```

### Cookie formats

The cookie parameter accepts either:

1. A Netscape cookie file (`# Netscape HTTP Cookie File ...`), passed through as-is.
2. A raw `Cookie` header string: `"auth_token=abc; ct0=def"` — expanded to Netscape format for the platform's domain(s).

For Playwright exports, join the cookie array into a header string:
``cookies.map(c => `${c.name}=${c.value}`).join("; ")``.

Cookies (request or [store](#cookie-store)) are written atomically to a `0600` temp file, passed to yt-dlp via `--cookies`, and deleted with the download after the response is sent. They are never logged.

### Cookie store

Set `IXIAN_COOKIE_DIR` to keep cookies on the server instead of sending them with every request. The server auto-loads `<platform>.txt` (`youtube.txt`, `x.txt`, `instagram.txt`, `tiktok.txt`) per download — a Netscape export or a raw `Cookie:` header — and cookies in the request JSON body take precedence over the file.

Export cookies for a logged-in session as a Netscape file, for example with the [Get cookies.txt LOCALLY](https://chromewebstore.google.com/detail/get-cookiestxt-locally/cclelndahbckbenkjhflpdbgdldlbecc) browser extension, then either drop the file into the directory or upload it:

```sh
mkdir -p ~/.ixian-cookies && chmod 700 ~/.ixian-cookies
IXIAN_COOKIE_DIR=~/.ixian-cookies cargo run --release

# copy an export into place, or upload it (atomic replace)
cp ~/Downloads/www.youtube.com_cookies.txt ~/.ixian-cookies/youtube.txt
curl -X PUT http://127.0.0.1:3000/cookies/youtube \
  --data-binary @www.youtube.com_cookies.txt

# raw Cookie header works too
curl -X PUT http://127.0.0.1:3000/cookies/x \
  --data-binary 'auth_token=...; ct0=...'

curl http://127.0.0.1:3000/cookies          # presence + mtime only, never values
curl -X DELETE http://127.0.0.1:3000/cookies/youtube
```

- `PUT` accepts a raw `Cookie:` header or a Netscape file and normalizes it to Netscape format. `DELETE` is idempotent (`{"deleted":false}` when nothing was stored).
- Files are written atomically (`0600`) in a `0700` directory, so a crash or concurrent `PUT` never leaves a partial jar. Each download gets a private copy, so rotating cookies does not affect downloads already running.
- Set `IXIAN_TOKEN` to require `Authorization: Bearer <token>` on cookie writes (`PUT`/`DELETE /cookies/{platform}`). If it is unset, writes are open like the rest of the API — keep the server on localhost or behind Cloudflare Access, and set `IXIAN_TOKEN` whenever others can reach it.
- `GET /cookies` stays open and returns only `platform`, `present` and `mtime` (unix seconds).
- Rotate by repeating `PUT`; the replacement is atomic. Re-export after a login/password change or extractor `401`/login failures, and before cookies expire. Expired cookies are worse than none for some platforms.
- Docker: the image sets `IXIAN_COOKIE_DIR=/data/cookies`; see [Docker](#docker) for the volume mount. For a bind mount, make the host directory writable by uid `1000` (the `node` user).

### Cache

Downloads are cached on disk. The key is
`SHA-256(platform + normalized URL + format + cookie fingerprint)`, so tracking
params, `youtu.be`/`twitter.com` aliases and query order do not cause duplicate
downloads. Requests that send cookies are cached under a hash of the cookies,
never the cookie contents; cookies auto-loaded from the [cookie store](#cookie-store)
are part of the key too.

Cache hits are served through `tower-http`'s `ServeFile`, so `Range`,
`If-None-Match`, `ETag` and `HEAD` work on both cache hits and misses. Concurrent
requests for the same key share one yt-dlp process. Entries are evicted least
recently used once `CACHE_MAX_BYTES` is exceeded.

```sh
curl 'http://127.0.0.1:3000/cache/stats'
curl -X DELETE 'http://127.0.0.1:3000/cache'
```

`GET /cache/stats` and `DELETE /cache` are open by default. Set `IXIAN_TOKEN` to
require `Authorization: Bearer <token>` (or `x-ixian-token: <token>`).

### Response

- Success: the media file itself (`content-disposition: attachment`, `x-ixian-platform`, `Cache-Control: private`, `ETag`, `Accept-Ranges: bytes`); cache hits also answer `Range`, `HEAD` and `If-None-Match` (`304`). `inline=1` (or `"inline": true` in JSON) switches to `content-disposition: inline` for browser playback. Temp files are cleaned up after streaming.
- Errors: JSON `{ "error": "...", "hint"? }` with status `400` (bad request, including cookies on `GET` and URLs outside the platform allowlist), `401` (cookie write or cache admin without a valid `IXIAN_TOKEN`), `404`, `405`, `413` (cookie too large), `429` (too many concurrent downloads), `502` (yt-dlp failed/timeout), `500`.

### Limits

| limit | value |
| --- | --- |
| concurrent downloads | `4` (`MAX_CONCURRENT`; excess requests return `429`) |
| request timeout | `600000` ms (`MAX_TIMEOUT_MS`; larger values return `400`) |
| cookie parameter | `256 KiB` (`MAX_COOKIE_BYTES`) |
| stored cookie file | `1 MiB` (`MAX_STORED_COOKIE_BYTES`) |
| captured stderr | `64 KiB` (`MAX_STDERR_BYTES`) |
| output file | `2 GiB` (`MAX_FILESIZE`, also passed to yt-dlp as `--max-filesize`) |
| request body | `16 MiB` |

yt-dlp runs with `--socket-timeout 10` and `--retries 3`. The URL is passed after
`--`, so a leading dash can never be parsed as a yt-dlp option. A `platform`
override never bypasses the host allowlist, and the URL must use `http` or `https`.

Each download runs in its own process group; a timeout or client disconnect kills
yt-dlp and its ffmpeg children. The temp directory is created `0700` and the
cookie jar `0600` (both atomically).

## Apple Shortcut

Install the latest signed build from [GitHub Releases](https://github.com/zeromask1337/ixian/releases/latest):

1. Open the [latest `ixian.shortcut`](https://github.com/zeromask1337/ixian/releases/latest/download/ixian.shortcut) on your iPhone.
2. Tap the download, then open it from **Files -> Downloads**.
3. Tap **Add Shortcut**.

By default it asks for the server URL and the Cloudflare Access service token (`CF-Access-Client-Id` / `CF-Access-Client-Secret`) on import, so no secrets live in the file. Keep the token in a password manager or keychain, not in git.

`src/bin/build-shortcut.rs` generates and signs a new build (macOS only: needs `plutil` + `shortcuts sign`, and an iCloud login for public signing):

```sh
cargo run --bin build-shortcut        # -> target/ixian.shortcut
open target/ixian.shortcut
```

Pass `--server URL --id ID --secret S` to bake values in instead of asking on import. To publish a release:

```sh
cargo run --bin build-shortcut
gh release create v0.1.1 target/ixian.shortcut -t v0.1.1 --generate-notes
```

Publishing the release also triggers the `Publish image` workflow, which builds `linux/amd64` + `linux/arm64` and pushes `ghcr.io/zeromask1337/ixian` (see [Docker](#docker)). To backfill an existing tag:

```sh
gh workflow run publish-image.yml -f tag=v0.1.1
```

Flow: share a link (or copy one and run from the menu bar) -> POST `/download` -> menu: Save to Photos / Share / Save to Files. Server errors and Cloudflare Access pages show as alerts.

## Config (env)

| var | default | description |
| --- | --- | --- |
| `PORT` | `3000` | listen port |
| `HOST` | `127.0.0.1` | listen host (only local by default) |
| `YTDLP_PATH` | `yt-dlp` | yt-dlp binary |
| `YTDLP_PLUGIN_DIR` | `<repo>/yt-dlp-plugins` | yt-dlp plugin directory (skipped when missing) |
| `YTDLP_FORMAT` | best playable mp4 (H.264/AAC) | default format when `format` is not passed |
| `YTDLP_TIMEOUT_MS` | `600000` | default per-download timeout (capped at `MAX_TIMEOUT_MS`) |
| `IXIAN_SNI` | `example.com` | decoy TLS SNI for Meta hosts (empty/`off` disables); see below |
| `IXIAN_IG_CDN_HOST` | `scontent.cdninstagram.com` | shared Instagram CDN host (empty disables the rewrite) |
| `IXIAN_COOKIE_DIR` | unset | cookie store directory (Docker: `/data/cookies`); auto-loads `<platform>.txt` and enables cookie writes |
| `IXIAN_CACHE_DIR` | `<tmp>/ixian-cache` | disk cache directory (`off` disables caching) |
| `CACHE_TTL` | `604800` | entry lifetime in seconds (`0` never expires) |
| `CACHE_MAX_BYTES` | `10737418240` | LRU byte budget (`0` unlimited) |
| `IXIAN_TOKEN` | unset | require `Authorization: Bearer <token>` on cookie writes and cache admin routes; unset leaves them open |

## Notes

- The whole server is one file (`src/main.rs`) built on `axum`, `tokio::process` and `tokio::fs`; the only external runtime dependencies are `yt-dlp` and `ffmpeg`.
- The default format prefers H.264/AAC so files play in QuickTime. Instagram's 1080p DASH formats are VP9 (and YouTube's 4K is VP9/AV1) which QuickTime cannot decode — pass `"format": "bv*+ba/b"` to fetch those (playable in VLC/ffmpeg), or install `ffmpeg` and transcode yourself.
- Playlists are disabled (`--no-playlist` plus `--playlist-items 1`): one URL yields one file. For an X post that quotes another post (or carries several videos), the linked post's first video is returned, not the quoted/original one.
- Downloads are staged in a unique temp directory. With caching enabled the media is hard-linked/copied into `IXIAN_CACHE_DIR` and served from there; without a cached copy the temp file is streamed and then deleted. There is no server idle timeout, so large files can stream for as long as the client reads them.
- Instagram: some networks drop TLS for `*.instagram.com` and geo-picked CDN edges. Two bundled yt-dlp plugins work around that:
  - `ixian_net` connects to Meta with a decoy SNI (`IXIAN_SNI`) while verifying the certificate against the real host.
  - `ixian_ig` rewrites media URLs to the shared `scontent.cdninstagram.com` endpoint (`IXIAN_IG_CDN_HOST`).
  The plugins are loaded with `--plugin-dirs` (`YTDLP_PLUGIN_DIR`; the default `yt-dlp-plugins` next to `src/` is skipped when missing); no global yt-dlp config is touched.
- TikTok: `tiktok.com`, `vm.tiktok.com` and `vt.tiktok.com` links are detected; cookies are written for `.tiktok.com`. TikTok rotates session cookies (`ttwid`, `msToken`, `tt_chain_token`, `sessionid`), so a stale export can cause empty formats or login errors — re-export from a logged-in browser session and `PUT` again. Region- and age-gated posts need cookies from an account allowed to view them; photo/slideshow posts are not videos. TikTok uses the default format selector (H.264/AAC preferred) and returns whatever yt-dlp extracts (watermark availability depends on the extractor).
- Respect each platform's terms of service and only download content you have the right to.

## Tests

```sh
cargo test          # unit + HTTP integration tests (stubbed yt-dlp)
cargo clippy --all-targets -- -D warnings
```

The integration tests exercise streaming, temp-file cleanup, cookie jar generation and permissions (temp dir `0700`, jar `0600`), cookie store auto-load/override/atomic write, URL/platform allowlist checks, GET cookie rejection, timeout aliases and caps, concurrency (`429`), stderr/output-size caps, `--` argument termination, process-group kills (timeout and cancellation), yt-dlp failures and bounded pipe drains without touching the network. Cache tests cover hit/miss, URL normalization, `Range`, `HEAD`, `304`, concurrent same-key dedupe, LRU eviction and admin auth.

Smoke-tested end to end against public media (2026-10-27):

| platform | URL | result |
| --- | --- | --- |
| YouTube | `https://youtu.be/aqz-KE-bpKQ` | 28,864,389-byte MP4, H.264/AAC 640×360, 634.6 s |
| x.com | `https://x.com/i/web/status/910031516746514432` | 7,761,912-byte MP4 |
| Instagram | `https://www.instagram.com/reel/Chunk8-jurw/` | 1,949,801-byte MP4 |

Public URLs can disappear or become login-gated; use cookies when a platform asks for authentication.
