# ixian

Minimal yt-dlp wrapper server written in Rust.
Downloads videos from **YouTube**, **x.com/twitter.com** and **Instagram**, with per-platform cookie support.

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

```sh
docker build -t ixian .
docker run -d --name ixian -p 127.0.0.1:3000:3000 --restart unless-stopped ixian
```

Multi-stage build: `rust:1.99` compiles the binary, `node:24-bookworm-slim` runs it with Node
(yt-dlp's JS runtime for YouTube challenges), yt-dlp (PyPI + EJS) and ffmpeg. `HOST=0.0.0.0`,
`YTDLP_PATH`, `YTDLP_PLUGIN_DIR` and the yt-dlp config (`--js-runtimes node`) are preset.
Any [config](#config-env) var can be overridden with `-e`, e.g. `-e YTDLP_FORMAT='bv*+ba/b'`.

## API

### `GET /download`

| param | description |
| --- | --- |
| `url` | **required** video URL |
| `platform` | optional override: `youtube`, `x`, `instagram` (otherwise detected from the URL) |
| `youtube_cookies` / `x_cookies` / `instagram_cookies` | cookies for that platform (only the matching one is used) |
| `format` | optional yt-dlp format selector (default: best mp4) |
| `timeout_ms` | optional download timeout (default `600000`, or `YTDLP_TIMEOUT_MS`) |

```sh
curl -G http://127.0.0.1:3000/download \
  --data-urlencode 'url=https://www.youtube.com/watch?v=dQw4w9WgXcQ' \
  -o video.mp4
```

### `POST /download`

Same params in a JSON body. Prefer this for cookies (query strings have length limits).

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

Cookies are written to a temp file (`chmod 600`), passed to yt-dlp via `--cookies`, and deleted with the download after the response is sent. They are never logged.

### Response

- Success: the media file itself (`content-disposition: attachment`, `x-ixian-platform` header). Temp files are cleaned up after streaming.
- Errors: JSON `{ "error": "...", "hint"? }` with status `400` (bad request), `404`, `405`, `502` (yt-dlp failed/timeout), `500`.

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
gh release create v0.1.0 target/ixian.shortcut -t v0.1.0 --generate-notes
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
| `YTDLP_TIMEOUT_MS` | `600000` | default per-download timeout |
| `IXIAN_SNI` | `example.com` | decoy TLS SNI for Meta hosts (empty/`off` disables); see below |
| `IXIAN_IG_CDN_HOST` | `scontent.cdninstagram.com` | shared Instagram CDN host (empty disables the rewrite) |

## Notes

- The whole server is one file (`src/main.rs`) built on `axum`, `tokio::process` and `tokio::fs`; the only external runtime dependencies are `yt-dlp` and `ffmpeg`.
- The default format prefers H.264/AAC so files play in QuickTime. Instagram's 1080p DASH formats are VP9 (and YouTube's 4K is VP9/AV1) which QuickTime cannot decode — pass `"format": "bv*+ba/b"` to fetch those (playable in VLC/ffmpeg), or install `ffmpeg` and transcode yourself.
- Playlists are disabled (`--no-playlist`); pass individual video URLs.
- Downloads are saved to a unique temp file and streamed back, then deleted. There is no server idle timeout, so large files can stream for as long as the client reads them.
- Instagram: some networks drop TLS for `*.instagram.com` and geo-picked CDN edges. Two bundled yt-dlp plugins work around that:
  - `ixian_net` connects to Meta with a decoy SNI (`IXIAN_SNI`) while verifying the certificate against the real host.
  - `ixian_ig` rewrites media URLs to the shared `scontent.cdninstagram.com` endpoint (`IXIAN_IG_CDN_HOST`).
  The plugins are loaded with `--plugin-dirs` (`YTDLP_PLUGIN_DIR`; the default `yt-dlp-plugins` next to `src/` is skipped when missing); no global yt-dlp config is touched.
- Respect each platform's terms of service and only download content you have the right to.

## Tests

```sh
cargo test          # unit + HTTP integration tests (stubbed yt-dlp)
cargo clippy --all-targets -- -D warnings
```

The integration tests exercise streaming, temp-file cleanup, cookie jar generation/permissions, request validation, yt-dlp failures and timeouts without touching the network.

Smoke-tested end to end against public media (2026-10-27):

| platform | URL | result |
| --- | --- | --- |
| YouTube | `https://youtu.be/aqz-KE-bpKQ` | 28,864,389-byte MP4, H.264/AAC 640×360, 634.6 s |
| x.com | `https://x.com/i/web/status/910031516746514432` | 7,761,912-byte MP4 |
| Instagram | `https://www.instagram.com/reel/Chunk8-jurw/` | 1,949,801-byte MP4 |

Public URLs can disappear or become login-gated; use cookies when a platform asks for authentication.
