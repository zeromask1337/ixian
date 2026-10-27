#!/usr/bin/env python3
"""Concurrent download benchmark for ixian servers (standard library only).

Example:
    python3 bench/download_bench.py --reps 5 --levels 6
    python3 bench/download_bench.py --reps 5 --levels 12 --repeat 2
"""
import argparse
import json
import statistics
import sys
import time
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed

URLS = [
    ("youtube", "https://www.youtube.com/watch?v=jNQXAC9IVRw"),
    ("youtube", "https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
    ("x", "https://x.com/i/web/status/910031516746514432"),
    ("x", "https://twitter.com/captainamerica/status/719944021058060289"),
    ("instagram", "https://www.instagram.com/reel/Chunk8-jurw/"),
    ("instagram", "https://www.instagram.com/reel/Cop84x6u7CP/"),
]
DEFAULT_FORMAT = "bv*[height<=360][vcodec^=avc1]+ba[ext=m4a]/b[ext=mp4]/bv*+ba/b"
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def fetch(base, item, media_format, timeout):
    platform, url = item
    query = urllib.parse.urlencode({"url": url, "format": media_format})
    started = time.perf_counter()
    with OPENER.open(f"{base}/download?{query}", timeout=timeout) as response:
        status = response.status
        body = response.read()
    return {
        "platform": platform,
        "url": url,
        "status": status,
        "bytes": len(body),
        "seconds": time.perf_counter() - started,
    }


def batch(base, concurrency, items, media_format, timeout):
    started = time.perf_counter()
    results = []
    with ThreadPoolExecutor(max_workers=concurrency) as pool:
        futures = [
            pool.submit(fetch, base, item, media_format, timeout) for item in items
        ]
        for future in as_completed(futures):
            results.append(future.result())
    return time.perf_counter() - started, results


def percentile(values, fraction):
    values = sorted(values)
    if not values:
        return 0.0
    index = min(len(values) - 1, max(0, round((len(values) - 1) * fraction)))
    return values[index]


def summarize(label, level, wall, results):
    latencies = [item["seconds"] for item in results]
    return {
        "label": label,
        "concurrency": level,
        "wall_seconds": round(wall, 3),
        "bytes": sum(item["bytes"] for item in results),
        "throughput_mbps": round(
            sum(item["bytes"] for item in results) * 8 / wall / 1_000_000, 2
        ),
        "latency_p50": round(statistics.median(latencies), 3),
        "latency_p95": round(percentile(latencies, 0.95), 3),
        "latency_max": round(max(latencies), 3),
        "failures": sum(item["status"] != 200 for item in results),
        "per_platform": {
            platform: {
                "count": len(matching),
                "bytes": sum(item["bytes"] for item in matching),
                "seconds": round(max(item["seconds"] for item in matching), 3),
            }
            for platform in sorted({result["platform"] for result in results})
            if (matching := [result for result in results if result["platform"] == platform])
        },
    }


def warmup(base, media_format, timeout):
    result = fetch(base, URLS[0], media_format, timeout)
    if result["status"] != 200:
        raise RuntimeError(f"warmup failed: {result}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--a",
        default="bun=http://127.0.0.1:3211",
        help="label=base URL for server A",
    )
    parser.add_argument(
        "--b",
        default="rust=http://127.0.0.1:3212",
        help="label=base URL for server B",
    )
    parser.add_argument("--reps", type=int, default=5, help="runs per server per level")
    parser.add_argument(
        "--levels", default="6", help="comma-separated client concurrency levels"
    )
    parser.add_argument(
        "--repeat",
        type=int,
        default=1,
        help="submit each URL N times (use 2 for 12-way load with 6 URLs)",
    )
    parser.add_argument("--format", default=DEFAULT_FORMAT, help="yt-dlp format selector")
    parser.add_argument("--timeout", type=float, default=600, help="request timeout (s)")
    parser.add_argument("--json-out", help="write per-run results to this JSON file")
    args = parser.parse_args()

    servers = []
    for value in (args.a, args.b):
        label, base = value.split("=", 1)
        servers.append((label, base))
    levels = [int(level) for level in args.levels.split(",")]
    items = URLS * args.repeat

    for label, base in servers:
        print(f"warmup {label} ...", flush=True)
        warmup(base, args.format, args.timeout)

    runs = []
    for rep in range(args.reps):
        for level in levels:
            order = (
                servers
                if (rep + levels.index(level)) % 2 == 0
                else list(reversed(servers))
            )
            for label, base in order:
                print(f"run {rep + 1}/{args.reps}: {label} @ {level} ...", flush=True)
                wall, results = batch(base, level, items, args.format, args.timeout)
                summary = summarize(label, level, wall, results)
                summary["rep"] = rep + 1
                summary["requests"] = results
                runs.append(summary)
                print(
                    f"  wall={summary['wall_seconds']:.2f}s "
                    f"throughput={summary['throughput_mbps']:.1f} Mbps "
                    f"p50={summary['latency_p50']:.2f}s "
                    f"failures={summary['failures']}",
                    flush=True,
                )
                if summary["failures"]:
                    print(json.dumps(results, indent=2), flush=True)

    print("\n=== medians ===", flush=True)
    print(
        f"{'server':<10} {'conc':>4} {'wall_s':>8} {'Mbps':>8} "
        f"{'p50_s':>7} {'p95_s':>7} {'fail':>4}",
        flush=True,
    )
    aggregates = []
    for label, _ in servers:
        for level in levels:
            selected = [
                run
                for run in runs
                if run["label"] == label and run["concurrency"] == level
            ]
            aggregate = {
                "label": label,
                "concurrency": level,
                "wall_seconds": round(
                    statistics.median(run["wall_seconds"] for run in selected), 3
                ),
                "throughput_mbps": round(
                    statistics.median(run["throughput_mbps"] for run in selected), 2
                ),
                "latency_p50": round(
                    statistics.median(run["latency_p50"] for run in selected), 3
                ),
                "latency_p95": round(
                    statistics.median(run["latency_p95"] for run in selected), 3
                ),
                "bytes": int(statistics.median(run["bytes"] for run in selected)),
                "failures": sum(run["failures"] for run in selected),
            }
            aggregates.append(aggregate)
            print(
                f"{label:<10} {level:>4} {aggregate['wall_seconds']:>8.3f} "
                f"{aggregate['throughput_mbps']:>8.2f} "
                f"{aggregate['latency_p50']:>7.3f} {aggregate['latency_p95']:>7.3f} "
                f"{aggregate['failures']:>4}",
                flush=True,
            )

    if args.json_out:
        with open(args.json_out, "w") as handle:
            json.dump({"runs": runs, "medians": aggregates}, handle, indent=2)
            handle.write("\n")

    if any(aggregate["failures"] for aggregate in aggregates):
        sys.exit(1)


if __name__ == "__main__":
    main()
