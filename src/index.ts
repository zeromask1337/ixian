// ixian - minimal yt-dlp wrapper (youtube / x.com / instagram) on Bun.
const TMP = Bun.env.TMPDIR ?? "/tmp";
const PORT = Number(Bun.env.PORT ?? 3000);
const HOST = Bun.env.HOST ?? "127.0.0.1";
const YTDLP = Bun.env.YTDLP_PATH ?? "yt-dlp";
const FORMAT = Bun.env.YTDLP_FORMAT ?? "bv*[vcodec^=avc1]+ba[acodec^=mp4a]/b[ext=mp4]/bv*+ba/b";
const TIMEOUT = Number(Bun.env.YTDLP_TIMEOUT_MS ?? 600_000);

const PLATFORMS = {
  youtube: { hosts: ["youtube.com", "youtu.be"], domains: [".youtube.com"] },
  x: { hosts: ["x.com", "twitter.com"], domains: [".x.com", ".twitter.com"] },
  instagram: { hosts: ["instagram.com"], domains: [".instagram.com"] },
} as const;

type Platform = keyof typeof PLATFORMS;
const NAMES = Object.keys(PLATFORMS) as Platform[];

const fail = (error: string, status = 400) => Response.json({ error }, { status });

function parseJson<T>(text: string): T | null {
  try {
    return JSON.parse(text) as T;
  } catch {
    return null;
  }
}

function resolvePlatform(url: string, requested?: string | null): Platform | Response {
  if (requested) {
    return Object.hasOwn(PLATFORMS, requested)
      ? (requested as Platform)
      : fail(`unknown platform '${requested}' (expected: ${NAMES.join(", ")})`);
  }
  const host = URL.parse(url)?.hostname.replace(/^www\./, "");
  const found = host && NAMES.find((p) => PLATFORMS[p].hosts.some((h) => host === h || host.endsWith(`.${h}`)));
  return found || fail(`cannot detect platform (supported: ${NAMES.join(", ")}); pass 'platform' to override`);
}

/** Netscape cookie file from either a Netscape file (passed through) or a raw `Cookie:` header string. */
function cookieFile(cookies: string, platform: Platform): string {
  if (cookies.includes("\t")) return cookies;
  const lines = cookies
    .replace(/^\s*cookie:\s*/i, "")
    .split(";")
    .map((pair) => pair.trim().split("="))
    .filter(([name, ...rest]) => Boolean(name) && rest.length > 0)
    .flatMap(([name, ...rest]) => {
      const value = rest.join("=").trim().replace(/^"|"$/g, "");
      return PLATFORMS[platform].domains.map((domain) =>
        [domain, "TRUE", "/", "TRUE", "2147483647", name!.trim(), value].join("\t"),
      );
    });
  return ["# Netscape HTTP Cookie File", ...lines, ""].join("\n");
}

interface Target {
  url: string;
  platform: Platform;
  cookies?: string;
  format?: string;
  timeoutMs?: number;
}

async function download({ url: rawUrl, platform, cookies, format, timeoutMs = TIMEOUT }: Target): Promise<Response> {
  const url = rawUrl.replace(/\\/g, "");
  const marker = `ixian-${crypto.randomUUID()}-`;
  const prefix = `${TMP}/${marker}`;
  const jar = `${prefix}cookies.txt`;
  if (cookies) {
    await Bun.write(jar, cookieFile(cookies, platform));
    await Bun.$`chmod 600 ${jar}`.quiet().nothrow();
  }

  const proc = Bun.spawn(
    [
      YTDLP,
      "--no-playlist",
      "--no-progress",
      "--no-simulate",
      "--print", "after_move:filepath",
      "--merge-output-format", "mp4",
      "-f", format || FORMAT,
      "-o", `${prefix}%(title)s [%(id)s].%(ext)s`,
      ...(cookies ? ["--cookies", jar] : []),
      url,
    ],
    { stdout: "pipe", stderr: "pipe", timeout: timeoutMs },
  );

  const [stdout, stderr, code] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);

  const path = stdout.trim().split("\n").pop();
  const file = path ? Bun.file(path) : null;
  const cleanup = () => Promise.allSettled([file?.delete(), Bun.file(jar).delete()]);

  if (code !== 0 || !file || !(await file.exists())) {
    await cleanup();
    const detail = stderr.trim() || `yt-dlp exited with ${proc.signalCode ?? `code ${code}`}`;
    return Response.json(
      { error: detail, ...(cookies && { hint: "cookies may be invalid or incomplete; public posts download without cookies" }) },
      { status: 502 },
    );
  }

  const name = path!.split(marker).pop()!;
  return new Response(
    file.stream().pipeThrough(new TransformStream({ flush: () => void cleanup() })),
    {
      headers: {
        "content-type": file.type || "application/octet-stream",
        "content-length": String(file.size),
        "content-disposition": `attachment; filename*=UTF-8''${encodeURIComponent(name)}`,
      },
    },
  );
}

Bun.serve({
  port: PORT,
  hostname: HOST,
  idleTimeout: 255,
  routes: {
    "/": { GET: () => Response.json({ message: "Hello!", platforms: NAMES }) },
    "/download": {
      GET: (req) => {
        const params = new URL(req.url).searchParams;
        const url = params.get("url");
        if (!url) return fail("missing 'url' query parameter");
        const platform = resolvePlatform(url, params.get("platform"));
        return platform instanceof Response
          ? platform
          : download({
              url,
              platform,
              cookies: params.get(`${platform}_cookies`) ?? undefined,
              format: params.get("format") ?? undefined,
              timeoutMs: Number(params.get("timeout_ms")) || undefined,
            });
      },
      POST: async (req) => {
        type Body = Partial<Record<"url" | "platform" | "format" | "timeoutMs" | `${Platform}_cookies`, string>> & {
          cookies?: Partial<Record<Platform, string>>;
        };
        const text = await req.text();
        // retry with stray backslashes (e.g. from copy-pasted urls like `\?utm_source\=x`) removed
        const body = parseJson<Body>(text) ?? parseJson<Body>(text.replace(/\\(?![\\\/"bfnrtu])/g, ""));
        if (!body) return fail("invalid JSON body (check for stray escapes in the url)");
        if (!body.url) return fail("missing 'url' in JSON body");
        const platform = resolvePlatform(body.url, body.platform);
        return platform instanceof Response
          ? platform
          : download({
              url: body.url,
              platform,
              cookies: body.cookies?.[platform] ?? body[`${platform}_cookies`] ?? undefined,
              format: body.format ?? undefined,
              timeoutMs: Number(body.timeoutMs) || undefined,
            });
      },
    },
  },
  fetch: () => fail("not found", 404),
  error: (error) => fail(error.message || "internal error", 500),
});

console.log(`ixian on http://${HOST}:${PORT}`);
