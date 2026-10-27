# Concurrent download benchmark

Compares this Rust implementation against the original Bun implementation on
`master` with the same `yt-dlp`, `ffmpeg`, machine and network.

## Workload

Six public videos — two each from YouTube, x.com and Instagram:

| platform | URL |
| --- | --- |
| YouTube | `https://www.youtube.com/watch?v=jNQXAC9IVRw` |
| YouTube | `https://www.youtube.com/watch?v=dQw4w9WgXcQ` |
| x.com | `https://x.com/i/web/status/910031516746514432` |
| x.com | `https://twitter.com/captainamerica/status/719944021058060289` |
| Instagram | `https://www.instagram.com/reel/Chunk8-jurw/` |
| Instagram | `https://www.instagram.com/reel/Cop84x6u7CP/` |

All requests use:

```
bv*[height<=360][vcodec^=avc1]+ba[ext=m4a]/b[ext=mp4]/bv*+ba/b
```

The six-video batch is 25,248,934 bytes. The 12-way run submits each URL twice
(50,497,868 bytes). Every request is a separate `GET /download`; the client
reads the complete response body. Order alternates between the two servers for
each repetition, and both servers get one warm-up download first.

## Run it

Start the original server from `master` on port 3211:

```sh
git worktree add /tmp/ixian-main master
cd /tmp/ixian-main
PORT=3211 bun src/index.ts
```

Start the Rust server from this branch on port 3212:

```sh
PORT=3212 cargo run --release
```

Then run:

```sh
# 6 concurrent requests (each URL once)
python3 bench/download_bench.py --reps 5 --levels 6

# 12 concurrent requests (each URL twice)
python3 bench/download_bench.py --reps 5 --levels 12 --repeat 2

# sequential baseline
python3 bench/download_bench.py --reps 3 --levels 1
```

Use `--a` / `--b` for other endpoints, and `--json-out` to save every request
timing. Exit status is non-zero if any request fails.

## Results

macOS, 2026-10-27. Bun 1.4.2, Rust 1.97.1, yt-dlp 2026.08.19, ffmpeg 9.0.2.

| metric | Bun (master) | Rust |
| --- | ---: | ---: |
| 6 concurrent, median wall | 3.045 s | 3.229 s |
| 6 concurrent, throughput | 66.3 Mbps | 62.6 Mbps |
| 6 concurrent, median request | 1.874 s | 2.087 s |
| 12 concurrent, median wall | 3.612 s | 3.613 s |
| 12 concurrent, throughput | 111.9 Mbps | 111.8 Mbps |
| 12 concurrent, median request | 2.320 s | 2.276 s |
| cold start (7 restarts) | 12.5 ms | 3.4 ms |
| idle RSS | 11.9 MiB | 2.8 MiB |
| RSS after benchmark runs | 47.7 MiB | 4.3 MiB |
| release artifact | Bun runtime + TS source | 1.5 MiB binary |

All 180 measured requests returned `200` (90 per server).

## Interpretation

There is **no meaningful download speedup**. Both servers spend almost all of
their time waiting for yt-dlp, ffmpeg and the platforms' CDNs; at 12 concurrent
requests the network tops out around 112 Mbps for both. The small difference at
6 concurrent is within run-to-run variance, and several individual runs had one
server ahead of the other.

The Rust rewrite wins on operational overhead instead:

- ~3.7× faster cold start (3.4 ms vs 12.5 ms).
- ~4× lower idle memory and ~11× lower memory after sustained downloads.
- No 255-second Bun idle-timeout limit while streaming large files.

Public URLs can disappear or become login-gated. Re-run the commands above if
the workload needs refreshing.
