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

## Config (env)

| var | default | description |
| --- | --- | --- |
| `PORT` | `3000` | listen port |
| `HOST` | `127.0.0.1` | listen host (only local by default) |
| `YTDLP_PATH` | `yt-dlp` | yt-dlp binary |
| `YTDLP_FORMAT` | best playable mp4 (H.264/AAC) | default format when `format` is not passed |
| `YTDLP_TIMEOUT_MS` | `600000` | default per-download timeout |

## Notes

- The whole server is one file (`src/main.rs`) built on `axum`, `tokio::process` and `tokio::fs`; the only external runtime dependencies are `yt-dlp` and `ffmpeg`.
- The default format prefers H.264/AAC so files play in QuickTime. Instagram's 1080p DASH formats are VP9 (and YouTube's 4K is VP9/AV1) which QuickTime cannot decode — pass `"format": "bv*+ba/b"` to fetch those (playable in VLC/ffmpeg), or install `ffmpeg` and transcode yourself.
- Playlists are disabled (`--no-playlist`); pass individual video URLs.
- Downloads are saved to a unique temp file and streamed back, then deleted. There is no server idle timeout, so large files can stream for as long as the client reads them.
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

## Performance

Benchmarked against the original Bun implementation on `master` with six public
videos (two each from YouTube, x.com and Instagram) under concurrent load:

| metric | Bun (master) | Rust |
| --- | ---: | ---: |
| 6 concurrent, median wall | 3.045 s | 3.229 s |
| 12 concurrent, median wall | 3.612 s | 3.613 s |
| cold start | 12.5 ms | 3.4 ms |
| idle RSS | 11.9 MiB | 2.8 MiB |
| RSS after benchmark | 47.7 MiB | 4.3 MiB |

Downloads are effectively tied: yt-dlp, ffmpeg and the network are the
bottleneck, not the HTTP server. The Rust rewrite's gains are startup time,
memory use and removing Bun's 255-second streaming timeout. See
[`bench/README.md`](bench/README.md) for methodology and the reproducible
benchmark (`bench/download_bench.py`).
