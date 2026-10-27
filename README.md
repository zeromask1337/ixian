# ixian

Minimal yt-dlp wrapper server written in TypeScript, running on [Bun](https://bun.sh).
Downloads videos from **YouTube**, **x.com/twitter.com** and **Instagram**, with per-platform cookie support.

## Requirements

- [Bun](https://bun.sh)
- [yt-dlp](https://github.com/yt-dlp/yt-dlp) (`brew install yt-dlp`)
- [ffmpeg](https://ffmpeg.org) (needed to merge best video + audio; `brew install ffmpeg`)

## Run

```sh
bun install
bun run dev     # watch mode
bun run start   # plain
```

Listens on `http://127.0.0.1:3000` by default.

### Docker

```sh
docker build -t ixian .
docker run -d --name ixian -p 127.0.0.1:3000:3000 --restart unless-stopped ixian
```

The image bundles Bun, yt-dlp (PyPI) and ffmpeg; `HOST=0.0.0.0` and `YTDLP_PATH` are preset. Any [config](#config-env) var can be overridden with `-e`, e.g. `-e YTDLP_FORMAT='bv*+ba/b'`.

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
`cookies.map(c => \`${c.name}=${c.value}\`).join("; ")`.

Cookies are written to a temp file (`chmod 600`), passed to yt-dlp via `--cookies`, and deleted with the download after the response is sent. They are never logged.

### Response

- Success: the media file itself (`content-disposition: attachment`, `x-ixian-platform` header). Temp files are cleaned up after streaming.
- Errors: JSON `{ "error": "...", "platform"?, "hint"? }` with status `400` (bad request), `404`, `422` (yt-dlp failed/timeout; not 502 because Cloudflare replaces 502 bodies), `500`.
- `url` may be a whole shared text; the first http(s) link is used. Client disconnect kills yt-dlp and removes temp files.

## Apple Shortcut

`shortcut/build.ts` generates and signs the shortcut (macOS only):

```sh
bun shortcut/build.ts        # -> shortcut/ixian.shortcut
open shortcut/ixian.shortcut
```

On import it asks for the server URL and the Cloudflare Access service token (`CF-Access-Client-Id` / `CF-Access-Client-Secret`), so no secrets live in the file. Keep the token in a password manager or keychain, not in git.

Flow: share a link (or copy one and run from the menu bar) -> POST `/download` -> menu: Save to Photos / Share / Save to Files. Server errors and Cloudflare Access pages show as alerts.

## Logging

JSON lines on stdout (`docker logs ixian`), every line tagged with a request `id` (also returned as the `x-ixian-request-id` header and in error JSON). Cookie values are never logged, only their size.

| `evt` | what it tells you |
| --- | --- |
| `request` | method, client ip / country / `cf-ray` / user-agent (from Cloudflare headers) |
| `download_start` | url, platform, format selector, timeout, cookie presence |
| `ytdlp_format` | resolved format (id, resolution, codecs, bitrate, approx size, protocol) and `extractMs` |
| `download_progress` | every 5s: bytes, total, speed, eta, % (spot throttling) |
| `ytdlp_exit` | exit code/signal, timed out/aborted, extract vs download ms, avg/peak speed, size, yt-dlp `WARNING`s |
| `ytdlp_failed` | error summary and last 30 stderr lines |
| `stream_start` / `stream_done` | file size, then bytes sent, stream ms and avg throughput to the client |
| `stream_aborted` / `client_abort` | client hung up: bytes sent so far, %, time (e.g. Shortcut timeout) |
| `response` | final status and time-to-headers |

Slow-download triage: `extractMs` (YouTube/JS challenges) vs `downloadMs`/`avgBps` (server fetch) vs `stream_done.avgBps` or `stream_aborted.pct` (the leg to the phone).

## Config (env)

| var | default | description |
| --- | --- | --- |
| `PORT` | `3000` | listen port |
| `HOST` | `127.0.0.1` | listen host (only local by default) |
| `YTDLP_PATH` | `yt-dlp` | yt-dlp binary |
| `YTDLP_PLUGIN_DIR` | `<repo>/yt-dlp-plugins` | yt-dlp plugin directory |
| `YTDLP_FORMAT` | best playable mp4 (H.264/AAC) | default format when `format` is not passed |
| `YTDLP_TIMEOUT_MS` | `600000` | default per-download timeout |
| `IXIAN_SNI` | `example.com` | decoy TLS SNI for Meta hosts (empty/`off` disables); see below |
| `IXIAN_IG_CDN_HOST` | `scontent.cdninstagram.com` | shared Instagram CDN host (empty disables the rewrite) |

## Notes

- The whole server is one file (`src/index.ts`) built on `Bun.serve`, `Bun.spawn` and `Bun.file` — no runtime dependencies.
- The default format prefers H.264/AAC so files play in QuickTime. Instagram's 1080p DASH formats are VP9 (and YouTube's 4K is VP9/AV1) which QuickTime cannot decode — pass `"format": "bv*+ba/b"` to fetch those (playable in VLC/ffmpeg), or install `ffmpeg` and transcode yourself.
- Playlists are disabled (`--no-playlist`); pass individual video URLs.
- Downloads are saved to a unique temp file and streamed back, then deleted; the stream has to finish within Bun's max idle timeout (255s), so very large downloads may need a smaller `format` (e.g. `b[ext=mp4]/b`).
- Instagram: some networks drop TLS for `*.instagram.com` and geo-picked CDN edges. Two bundled yt-dlp plugins work around that:
  - `ixian_net` connects to Meta with a decoy SNI (`IXIAN_SNI`) while verifying the certificate against the real host.
  - `ixian_ig` rewrites media URLs to the shared `scontent.cdninstagram.com` endpoint (`IXIAN_IG_CDN_HOST`).
  The plugins are loaded with `--plugin-dirs` (set `YTDLP_PLUGIN_DIR` in Docker); no global yt-dlp config is touched.
- Respect each platform's terms of service and only download content you have the right to.
