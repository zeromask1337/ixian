// ixian - minimal yt-dlp wrapper (youtube / x.com / instagram) on Bun.
import { existsSync } from "node:fs";

const TMP = Bun.env.TMPDIR ?? "/tmp";
const PORT = Number(Bun.env.PORT ?? 3000);
const HOST = Bun.env.HOST ?? "127.0.0.1";
const YTDLP = Bun.env.YTDLP_PATH ?? "yt-dlp";
const FORMAT = Bun.env.YTDLP_FORMAT ?? "bv*[vcodec^=avc1]+ba[acodec^=mp4a]/b[ext=mp4]/bv*+ba/b";
const TIMEOUT = Number(Bun.env.YTDLP_TIMEOUT_MS ?? 600_000);
const PLUGIN_DIR = Bun.env.YTDLP_PLUGIN_DIR ?? `${import.meta.dir}/../yt-dlp-plugins`;

const PLATFORMS = {
  youtube: { hosts: ["youtube.com", "youtu.be"], domains: [".youtube.com"] },
  x: { hosts: ["x.com", "twitter.com"], domains: [".x.com", ".twitter.com"] },
  instagram: { hosts: ["instagram.com"], domains: [".instagram.com"] },
} as const;

type Platform = keyof typeof PLATFORMS;
const NAMES = Object.keys(PLATFORMS) as Platform[];

class HttpError extends Error {
  constructor(message: string, readonly status = 400, readonly extra?: object) {
    super(message);
  }
}

const fail = (error: string, status = 400, extra?: object) => Response.json({ error, ...extra }, { status });

/** Request options, normalized from either query params or a JSON body. */
interface Options {
  url?: string;
  platform?: string;
  format?: string;
  timeoutMs?: number;
  cookies: Partial<Record<Platform, string>>;
}

function fromQuery(params: URLSearchParams): Options {
  const cookies = Object.fromEntries(NAMES.flatMap((p) => (params.has(`${p}_cookies`) ? [[p, params.get(`${p}_cookies`)!]] : [])));
  return {
    url: params.get("url") ?? undefined,
    platform: params.get("platform") ?? undefined,
    format: params.get("format") ?? undefined,
    timeoutMs: Number(params.get("timeout_ms")) || undefined,
    cookies,
  };
}

function fromBody(text: string): Options {
  type Body = Partial<Record<"url" | "platform" | "format" | `${Platform}_cookies`, string>> & {
    timeoutMs?: number | string;
    cookies?: Partial<Record<Platform, string>>;
  };
  // retry with stray backslashes (e.g. from copy-pasted urls like `\?utm_source\=x`) removed
  const body = (parseJson(text) ?? parseJson(text.replace(/\\(?![\\\/"bfnrtu])/g, ""))) as Body | null;
  if (!body || typeof body !== "object") throw new HttpError("invalid JSON body (check for stray escapes in the url)");
  const cookies = Object.fromEntries(NAMES.flatMap((p) => {
    const value = body.cookies?.[p] ?? body[`${p}_cookies`];
    return value ? [[p, value]] : [];
  }));
  return { url: body.url, platform: body.platform, format: body.format, timeoutMs: Number(body.timeoutMs) || undefined, cookies };
}

function parseJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

/** First http(s) URL in the input, so shared text like "look at this https://x.com/..." works. */
function extractUrl(input: string): string {
  const url = input.replace(/\\/g, "").match(/https?:\/\/[^\s"'<>]+/i)?.[0];
  if (!url || !URL.canParse(url)) throw new HttpError("missing or invalid 'url'");
  return url;
}

function resolvePlatform(url: string, requested?: string): Platform {
  if (requested) {
    if (Object.hasOwn(PLATFORMS, requested)) return requested as Platform;
    throw new HttpError(`unknown platform '${requested}' (expected: ${NAMES.join(", ")})`);
  }
  const host = new URL(url).hostname.replace(/^www\./, "");
  const found = NAMES.find((p) => PLATFORMS[p].hosts.some((h) => host === h || host.endsWith(`.${h}`)));
  if (!found) throw new HttpError(`cannot detect platform (supported: ${NAMES.join(", ")}); pass 'platform' to override`);
  return found;
}

/** Netscape cookie file from either a Netscape file (passed through) or a raw `Cookie:` header string. */
function cookieFile(cookies: string, platform: Platform): string {
  if (cookies.includes("\t")) return cookies;
  const lines = cookies
    .replace(/^\s*cookie:\s*/i, "")
    .split(";")
    .flatMap((pair) => {
      const eq = pair.indexOf("=");
      const name = pair.slice(0, eq).trim();
      if (eq < 0 || !name) return [];
      const value = pair.slice(eq + 1).trim().replace(/^"|"$/g, "");
      return PLATFORMS[platform].domains.map((domain) => [domain, "TRUE", "/", "TRUE", "2147483647", name, value].join("\t"));
    });
  return ["# Netscape HTTP Cookie File", ...lines, ""].join("\n");
}

/** One JSON object per line on stdout. Never pass cookie values in here. */
function log(evt: string, fields: Record<string, unknown> = {}) {
  console.log(JSON.stringify({ ts: new Date().toISOString(), evt, ...fields }));
}

const ms = (since: number) => Math.round(performance.now() - since);

async function readLines(stream: ReadableStream<Uint8Array>, onLine: (line: string) => void) {
  const decoder = new TextDecoder();
  let buf = "";
  for await (const chunk of stream) {
    buf += decoder.decode(chunk, { stream: true });
    for (let i = buf.indexOf("\n"); i >= 0; i = buf.indexOf("\n")) {
      onLine(buf.slice(0, i).replace(/\r$/, ""));
      buf = buf.slice(i + 1);
    }
  }
  if (buf) onLine(buf);
}

const META = "ixian-meta:";
const PROGRESS = "ixian-progress:";
const PROGRESS_EVERY_MS = 5_000;
const num = (s: string | undefined) => (s === undefined || s === "NA" || s === "" ? null : Number(s));

/** Short, human-readable error from yt-dlp's stderr (the ERROR lines, else the last line). */
function summarize(stderr: string): string | undefined {
  const lines = stderr.trim().split("\n").filter(Boolean);
  const errors = lines.filter((l) => l.startsWith("ERROR:"));
  return (errors.length ? errors : lines.slice(-1)).map((l) => l.replace(/^ERROR:\s*/, "")).join("\n") || undefined;
}

async function download(opts: Options, signal: AbortSignal, id: string): Promise<Response> {
  const rlog = (evt: string, fields: Record<string, unknown> = {}) => log(evt, { id, ...fields });
  if (!opts.url) throw new HttpError("missing 'url'");
  const url = extractUrl(opts.url);
  const platform = resolvePlatform(url, opts.platform);
  const cookies = opts.cookies[platform];
  const format = opts.format || FORMAT;
  const timeoutMs = opts.timeoutMs ?? TIMEOUT;

  const dir = `${TMP}/ixian-${crypto.randomUUID()}`;
  const cleanup = async () => void (await Bun.$`rm -rf ${dir}`.quiet().nothrow());
  try {
    const jar = `${dir}/cookies.txt`;
    if (cookies) {
      await Bun.write(jar, cookieFile(cookies, platform), { createPath: true });
      // Bun.write's `mode` option is ignored here (oven-sh/bun#25903), so lock down explicitly
      await Bun.$`chmod 700 ${dir} && chmod 600 ${jar}`.quiet();
    }

    rlog("download_start", { url, platform, format, customFormat: !!opts.format, timeoutMs, cookies: cookies ? { platform, bytes: cookies.length } : null });

    const startedAt = performance.now();
    const proc = Bun.spawn(
      [
        YTDLP,
        "--no-playlist",
        "--progress",
        "--newline",
        "--progress-template", `download:${PROGRESS}%(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s %(progress.speed)s %(progress.eta)s %(info.format_id)s`,
        "--no-simulate",
        "--print", `before_dl:${META}%(.{id,title,duration,extractor_key,format_id,ext,width,height,fps,vcodec,acodec,tbr,filesize,filesize_approx,protocol})j`,
        "--print", "after_move:filepath",
        "--merge-output-format", "mp4",
        "-f", format,
        "-P", dir,
        "-o", "%(title).100B [%(id)s].%(ext)s",
        ...(existsSync(PLUGIN_DIR) ? ["--plugin-dirs", PLUGIN_DIR] : []),
        ...(cookies ? ["--cookies", jar] : []),
        "--", url,
      ],
      { stdout: "pipe", stderr: "pipe", timeout: timeoutMs, signal },
    );
    rlog("ytdlp_spawn", { pid: proc.pid, ytdlp: YTDLP });

    const stdoutLines: string[] = [];
    const stderrLines: string[] = [];
    let meta: Record<string, unknown> | null = null;
    let metaAt: number | null = null;
    let lastProgressLog = 0;
    let peakSpeed = 0;
    let lastProgress: Record<string, unknown> | null = null;

    const onLine = (line: string) => {
      if (line.startsWith(PROGRESS)) {
        const [downloaded, total, estimate, speed, eta, formatId] = line.slice(PROGRESS.length).trim().split(/\s+/);
        const bytes = num(downloaded), size = num(total) ?? num(estimate), bps = num(speed);
        if (bps) peakSpeed = Math.max(peakSpeed, bps);
        lastProgress = { formatId, bytes, size, bps, etaS: num(eta) };
        if (performance.now() - lastProgressLog >= PROGRESS_EVERY_MS) {
          lastProgressLog = performance.now();
          rlog("download_progress", { ...lastProgress, pct: bytes && size ? Math.round((bytes / size) * 1000) / 10 : null, elapsedMs: ms(startedAt) });
        }
      } else if (line.startsWith(META)) {
        try {
          meta = JSON.parse(line.slice(META.length));
        } catch {
          meta = { raw: line.slice(META.length) };
        }
        metaAt = ms(startedAt);
        const { id: videoId, ...rest } = meta as Record<string, unknown>;
        rlog("ytdlp_format", { extractMs: metaAt, videoId, ...rest });
      } else {
        stdoutLines.push(line);
      }
    };

    const readStdout = readLines(proc.stdout, onLine);
    const readStderr = readLines(proc.stderr, (line) => (line.startsWith(PROGRESS) ? onLine(line) : stderrLines.push(line)));
    const code = await proc.exited;
    await Promise.all([readStdout, readStderr]);
    const ytdlpMs = ms(startedAt);
    const stderr = stderrLines.join("\n");
    const warnings = stderrLines.filter((l) => /^WARNING:/.test(l));

    const path = stdoutLines.filter(Boolean).pop();
    const file = path ? Bun.file(path) : null;
    const size = file && (await file.exists()) ? file.size : null;
    const timedOut = proc.signalCode === "SIGTERM" && ytdlpMs >= timeoutMs - 1_000;
    rlog("ytdlp_exit", {
      code, signal: proc.signalCode, timedOut, aborted: signal.aborted, ytdlpMs, extractMs: metaAt,
      downloadMs: metaAt === null ? null : ytdlpMs - metaAt, bytes: size,
      avgBps: size && metaAt !== null && ytdlpMs > metaAt ? Math.round(size / ((ytdlpMs - metaAt) / 1000)) : null,
      peakBps: peakSpeed || null, lastProgress, warnings,
    });

    if (code !== 0 || !file || size === null) {
      const error = summarize(stderr) ?? `yt-dlp exited with ${proc.signalCode ?? `code ${code}`}`;
      const hint = cookies && "cookies may be invalid or incomplete; public posts download without cookies";
      rlog("ytdlp_failed", { error, stderrTail: stderrLines.slice(-30) });
      // 422, not 502: Cloudflare replaces 502 bodies with its own error page
      throw new HttpError(error, 422, { platform, ...(hint && { hint }) });
    }

    const name = path!.split("/").pop()!;
    const ascii = name.replace(/[^\x20-\x7e]|["\\]/g, "_");
    const streamStart = performance.now();
    let sent = 0;
    let finished = false;
    signal.addEventListener(
      "abort",
      () => {
        if (!finished) rlog("stream_aborted", { reason: "client_disconnected", sent, size, pct: Math.round((sent / size) * 1000) / 10, streamMs: ms(streamStart), sinceStartMs: ms(startedAt) });
        void cleanup();
      },
      { once: true },
    );
    const body = file.stream().pipeThrough(
      new TransformStream<Uint8Array, Uint8Array>({
        transform(chunk, controller) {
          sent += chunk.byteLength;
          controller.enqueue(chunk);
        },
        flush() {
          finished = true;
          const streamMs = ms(streamStart);
          rlog("stream_done", { sent, size, streamMs, avgBps: Math.round(sent / Math.max(streamMs / 1000, 0.001)), totalMs: ms(startedAt) });
          void cleanup();
        },
      }),
    );
    rlog("stream_start", { file: name, size, contentType: file.type });
    return new Response(body, {
      headers: {
        "content-type": file.type || "application/octet-stream",
        "content-disposition": `attachment; filename="${ascii}"; filename*=UTF-8''${encodeURIComponent(name)}`,
        "x-ixian-platform": platform,
        "x-ixian-request-id": id,
      },
    });
  } catch (error) {
    await cleanup();
    throw error;
  }
}

/** Runs a handler with a request id and logging, turning thrown `HttpError`s into JSON error responses. */
const handle = (fn: (req: Request, id: string) => Promise<Response>) => async (req: Request) => {
  const id = crypto.randomUUID().slice(0, 8);
  const t0 = performance.now();
  const h = req.headers;
  log("request", {
    id, method: req.method, path: new URL(req.url).pathname,
    ip: h.get("cf-connecting-ip") ?? h.get("x-forwarded-for"), country: h.get("cf-ipcountry"), ray: h.get("cf-ray"),
    ua: h.get("user-agent"), contentLength: h.get("content-length"),
  });
  req.signal.addEventListener("abort", () => log("client_abort", { id, afterMs: ms(t0) }), { once: true });
  try {
    const res = await fn(req, id);
    log("response", { id, status: res.status, headersAtMs: ms(t0), contentType: res.headers.get("content-type") });
    return res;
  } catch (error) {
    if (error instanceof HttpError) {
      log("response", { id, status: error.status, error: error.message, extra: error.extra, headersAtMs: ms(t0) });
      return fail(error.message, error.status, { ...error.extra, id });
    }
    log("unhandled_error", { id, error: String(error), stack: error instanceof Error ? error.stack : undefined, afterMs: ms(t0) });
    throw error;
  }
};

Bun.serve({
  port: PORT,
  hostname: HOST,
  idleTimeout: 255,
  routes: {
    "/": { GET: () => Response.json({ message: "Hello!", platforms: NAMES }) },
    "/download": {
      GET: handle((req, id) => download(fromQuery(new URL(req.url).searchParams), req.signal, id)),
      POST: handle(async (req, id) => download(fromBody(await req.text()), req.signal, id)),
    },
  },
  fetch: (req) => (log("not_found", { method: req.method, path: new URL(req.url).pathname }), fail("not found", 404)),
  error: (error) => (log("server_error", { error: error.message, stack: error.stack }), fail(error.message || "internal error", 500)),
});

log("start", { host: HOST, port: PORT, ytdlp: YTDLP, plugins: existsSync(PLUGIN_DIR) ? PLUGIN_DIR : null, format: FORMAT, timeoutMs: TIMEOUT, tmp: TMP, bun: Bun.version });
