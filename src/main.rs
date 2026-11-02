// ixian - minimal yt-dlp wrapper (youtube / x.com / instagram) in Rust.
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    env, io,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt, ReadBuf},
    net::TcpListener,
    process::Command,
    time::timeout,
};
use tokio_util::io::ReaderStream;
use url::Url;
use uuid::Uuid;

const DEFAULT_FORMAT: &str = "bv*[vcodec^=avc1]+ba[acodec^=mp4a]/b[ext=mp4]/bv*+ba/b";
const COOKIE_HINT: &str =
    "cookies may be invalid or incomplete; public posts download without cookies";
const DEFAULT_INFO_TIMEOUT_MS: u64 = 15_000;
const STREAM_CHUNK: usize = 64 * 1024;
const FILENAME_ENCODE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    YouTube,
    X,
    Instagram,
}

impl Platform {
    const ALL: [Platform; 3] = [Platform::YouTube, Platform::X, Platform::Instagram];

    fn name(self) -> &'static str {
        match self {
            Platform::YouTube => "youtube",
            Platform::X => "x",
            Platform::Instagram => "instagram",
        }
    }

    fn hosts(self) -> &'static [&'static str] {
        match self {
            Platform::YouTube => &["youtube.com", "youtu.be"],
            Platform::X => &["x.com", "twitter.com"],
            Platform::Instagram => &["instagram.com"],
        }
    }

    fn domains(self) -> &'static [&'static str] {
        match self {
            Platform::YouTube => &[".youtube.com"],
            Platform::X => &[".x.com", ".twitter.com"],
            Platform::Instagram => &[".instagram.com"],
        }
    }

    fn from_name(name: &str) -> Option<Platform> {
        Platform::ALL.into_iter().find(|p| p.name() == name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Video,
    Audio,
}

impl Mode {
    fn parse(value: Option<&str>) -> Result<Self, ApiError> {
        match value.filter(|value| !value.is_empty()) {
            None => Ok(Mode::Video),
            Some(value) if value.eq_ignore_ascii_case("video") => Ok(Mode::Video),
            Some(value) if value.eq_ignore_ascii_case("audio") => Ok(Mode::Audio),
            Some(value) => Err(ApiError::bad_request(format!(
                "unknown mode '{value}' (expected: video, audio)"
            ))),
        }
    }

    fn is_audio(self) -> bool {
        matches!(self, Mode::Audio)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Quality {
    Best,
    P1080,
    P720,
    P480,
    Data,
}

impl Quality {
    fn parse(value: Option<&str>) -> Result<Self, ApiError> {
        match value.filter(|value| !value.is_empty()) {
            None => Ok(Quality::Best),
            Some(value) if value.eq_ignore_ascii_case("best") => Ok(Quality::Best),
            Some("1080") => Ok(Quality::P1080),
            Some("720") => Ok(Quality::P720),
            Some("480") => Ok(Quality::P480),
            Some(value) if value.eq_ignore_ascii_case("data") => Ok(Quality::Data),
            Some(value) => Err(ApiError::bad_request(format!(
                "unknown quality '{value}' (expected: best, 1080, 720, 480, data)"
            ))),
        }
    }

    fn selector(self, container: Container, default_format: &str) -> String {
        let webm = container == Container::Webm;
        match self {
            Quality::Best if webm => concat!(
                "bv*[vcodec^=vp9]+ba[acodec=opus]/",
                "bv*[ext=webm]+ba/b[ext=webm]/bv*+ba/b"
            )
            .to_string(),
            Quality::Best => default_format.to_string(),
            Quality::P1080 => capped_selector(1080, webm),
            Quality::P720 => capped_selector(720, webm),
            Quality::P480 => capped_selector(480, webm),
            Quality::Data if webm => "b[height<=480][ext=webm]/b[height<=480]/b".to_string(),
            Quality::Data => "b[height<=480]/b".to_string(),
        }
    }
}

fn capped_selector(height: u16, webm: bool) -> String {
    if webm {
        format!(
            "bv*[height<={height}][vcodec^=vp9]+ba[acodec=opus]/\
             bv*[height<={height}]+ba/b[height<={height}]/b"
        )
    } else {
        format!(
            "bv*[height<={height}][vcodec^=avc1]+ba[acodec^=mp4a]/\
             bv*[height<={height}]+ba/b[height<={height}]/b"
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Container {
    Mp4,
    Webm,
}

impl Container {
    fn parse(value: Option<&str>) -> Result<Self, ApiError> {
        match value.filter(|value| !value.is_empty()) {
            None => Ok(Container::Mp4),
            Some(value) if value.eq_ignore_ascii_case("mp4") => Ok(Container::Mp4),
            Some(value) if value.eq_ignore_ascii_case("webm") => Ok(Container::Webm),
            Some(value) => Err(ApiError::bad_request(format!(
                "unknown container '{value}' (expected: mp4, webm)"
            ))),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Webm => "webm",
        }
    }
}

fn parse_item(value: Option<&str>) -> Result<usize, ApiError> {
    match value.filter(|value| !value.is_empty()) {
        None => Ok(1),
        Some(value) => value
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|item| *item > 0)
            .ok_or_else(|| {
                ApiError::bad_request(format!("invalid item '{value}' (expected a number >= 1)"))
            }),
    }
}

fn item_from_json(value: Option<&Value>) -> Result<usize, ApiError> {
    match value {
        None | Some(Value::Null) => Ok(1),
        Some(Value::Number(number)) => number
            .as_u64()
            .and_then(|item| usize::try_from(item).ok())
            .filter(|item| *item > 0)
            .ok_or_else(|| ApiError::bad_request("invalid 'item' (expected a number >= 1)")),
        Some(Value::String(value)) => parse_item(Some(value)),
        Some(_) => Err(ApiError::bad_request(
            "invalid 'item' (expected a number >= 1)",
        )),
    }
}

struct Config {
    host: String,
    port: u16,
    ytdlp: String,
    plugin_dir: Option<PathBuf>,
    format: String,
    timeout: Duration,
    info_timeout: Duration,
    tmp: PathBuf,
}

impl Config {
    fn from_env() -> Self {
        let port = env::var("PORT")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(3000);
        let host = non_empty(env::var("HOST").ok()).unwrap_or_else(|| "127.0.0.1".into());
        let ytdlp = non_empty(env::var("YTDLP_PATH").ok()).unwrap_or_else(|| "yt-dlp".into());
        let plugin_dir = non_empty(env::var("YTDLP_PLUGIN_DIR").ok())
            .map(PathBuf::from)
            .or_else(|| Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("yt-dlp-plugins")))
            .filter(|dir| dir.is_dir());
        let format =
            non_empty(env::var("YTDLP_FORMAT").ok()).unwrap_or_else(|| DEFAULT_FORMAT.to_string());
        let timeout_ms = env::var("YTDLP_TIMEOUT_MS")
            .ok()
            .and_then(|v| parse_ms(&v))
            .unwrap_or_else(|| Duration::from_millis(600_000));
        let info_timeout = env::var("YTDLP_INFO_TIMEOUT_MS")
            .ok()
            .and_then(|v| parse_ms(&v))
            .unwrap_or_else(|| Duration::from_millis(DEFAULT_INFO_TIMEOUT_MS));
        let tmp = non_empty(env::var("TMPDIR").ok())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));

        Self {
            host,
            port,
            ytdlp,
            plugin_dir,
            format,
            timeout: timeout_ms,
            info_timeout,
            tmp,
        }
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
    hint: Option<String>,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            hint: None,
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn bad_gateway(message: impl Into<String>, hint: Option<&str>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
            hint: hint.map(str::to_string),
        }
    }

    fn classified(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            hint: Some(hint_for(status).to_string()),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = Map::new();
        body.insert("error".into(), Value::String(self.message));
        body.insert("code".into(), json!(self.status.as_u16()));
        if let Some(hint) = self.hint {
            body.insert("hint".into(), Value::String(hint));
        }
        (self.status, Json(Value::Object(body))).into_response()
    }
}

fn hint_for(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "check the url; pass the full post or video link",
        StatusCode::UNAUTHORIZED => "sign in or pass cookies for this platform",
        StatusCode::FORBIDDEN => {
            "the post may be private or members-only; pass cookies with access"
        }
        StatusCode::NOT_FOUND => "the post or video may have been deleted or made private",
        StatusCode::TOO_MANY_REQUESTS => "rate limited by the platform; wait and retry",
        StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS => {
            "not available in this region; try a different network"
        }
        StatusCode::GATEWAY_TIMEOUT => "yt-dlp timed out; retry or raise the timeout",
        _ => "yt-dlp failed; check the url and try again",
    }
}

fn classify_ytdlp_error(stderr: &str) -> Option<StatusCode> {
    let text = stderr.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| text.contains(needle));

    for (code, status) in [
        ("429", StatusCode::TOO_MANY_REQUESTS),
        ("451", StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS),
        ("401", StatusCode::UNAUTHORIZED),
        ("403", StatusCode::FORBIDDEN),
        ("404", StatusCode::NOT_FOUND),
        ("504", StatusCode::GATEWAY_TIMEOUT),
        ("400", StatusCode::BAD_REQUEST),
    ] {
        if text.contains(&format!("http error {code}"))
            || text.contains(&format!("status code {code}"))
        {
            return Some(status);
        }
    }

    if has(&[
        "too many requests",
        "rate limit",
        "rate-limit",
        "rate limited",
    ]) {
        return Some(StatusCode::TOO_MANY_REQUESTS);
    }
    if has(&[
        "not available in your country",
        "not available in your region",
        "geo-restrict",
        "geo restrict",
        "geo-block",
        "geo blocked",
        "blocked in your country",
        "unavailable for legal reasons",
    ]) {
        return Some(StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS);
    }
    if has(&[
        "login required",
        "sign in to",
        "sign in required",
        "authentication required",
        "requires authentication",
        "confirm your age",
        "age-restricted",
        "age restricted",
        "cookies",
        "logged-in",
        "logged in",
    ]) {
        return Some(StatusCode::UNAUTHORIZED);
    }
    if has(&[
        "private video",
        "this video is private",
        "members-only",
        "members only",
        "join this channel",
        "subscriber",
        "paid",
        "purchase",
    ]) {
        return Some(StatusCode::FORBIDDEN);
    }
    if has(&[
        "video unavailable",
        "this video is unavailable",
        "does not exist",
        "no longer available",
        "has been removed",
        "removed by the uploader",
        "not found",
        "unable to find",
        "no video",
        "no media",
    ]) {
        return Some(StatusCode::NOT_FOUND);
    }
    if has(&["timed out", "timeout", "read timeout"]) {
        return Some(StatusCode::GATEWAY_TIMEOUT);
    }
    if has(&[
        "unsupported url",
        "is not a valid url",
        "invalid url",
        "invalid request",
        "no suitable format",
        "requested format is not available",
    ]) {
        return Some(StatusCode::BAD_REQUEST);
    }
    None
}

fn ytdlp_failure(stderr: &str, status_code: Option<i32>, fallback_hint: Option<&str>) -> ApiError {
    let detail = if !stderr.trim().is_empty() {
        stderr.trim().to_string()
    } else {
        match status_code {
            Some(code) => format!("yt-dlp exited with code {code}"),
            None => "yt-dlp was terminated by a signal".to_string(),
        }
    };
    match classify_ytdlp_error(&detail) {
        Some(status) => ApiError::classified(status, detail),
        None => ApiError::bad_gateway(
            detail,
            Some(fallback_hint.unwrap_or_else(|| hint_for(StatusCode::BAD_GATEWAY))),
        ),
    }
}

type ApiResult = Result<Response, ApiError>;

fn resolve_platform(url: &str, requested: Option<&str>) -> Result<Platform, ApiError> {
    if let Some(requested) = requested.filter(|name| !name.is_empty()) {
        return Platform::from_name(requested).ok_or_else(|| {
            ApiError::bad_request(format!(
                "unknown platform '{requested}' (expected: youtube, x, instagram)"
            ))
        });
    }

    detect_platform(url).ok_or_else(|| {
        ApiError::bad_request(
            "cannot detect platform (supported: youtube, x, instagram); pass 'platform' to override",
        )
    })
}

fn detect_platform(raw_url: &str) -> Option<Platform> {
    let url = Url::parse(raw_url).ok()?;
    let host = url.host_str()?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    Platform::ALL.into_iter().find(|platform| {
        platform
            .hosts()
            .iter()
            .any(|candidate| host == *candidate || host.ends_with(&format!(".{candidate}")))
    })
}

/// Netscape cookie file from either a Netscape file (passed through) or a raw `Cookie:` header string.
fn cookie_file(cookies: &str, platform: Platform) -> String {
    if cookies.contains('\t') {
        return cookies.to_string();
    }

    let trimmed = cookies.trim_start();
    let header = strip_prefix_ignore_case(trimmed, "cookie:")
        .unwrap_or(trimmed)
        .trim_start();

    let mut lines = vec!["# Netscape HTTP Cookie File".to_string()];
    for pair in header.split(';') {
        let Some((name, value)) = pair.trim().split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let value = value.trim().trim_matches('"');
        for domain in platform.domains() {
            lines.push(format!(
                "{domain}\tTRUE\t/\tTRUE\t2147483647\t{name}\t{value}"
            ));
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

fn strip_prefix_ignore_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let head = value.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &value[prefix.len()..])
}

/// yt-dlp prints one filepath per downloaded item. A tweet that quotes another
/// tweet (or carries several videos) is a playlist: the linked tweet's own
/// media comes first, the referenced tweet's video after it. `item` is 1-based.
fn nth_filepath(stdout: &str, item: usize) -> Option<PathBuf> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .nth(item.saturating_sub(1))
        .map(PathBuf::from)
}

fn parse_ms(value: &str) -> Option<Duration> {
    let ms = value.trim().parse::<f64>().ok()?;
    (ms.is_finite() && ms > 0.0).then(|| Duration::from_millis(ms.min(u64::MAX as f64) as u64))
}

fn parse_ms_value(value: Option<&Value>) -> Option<Duration> {
    match value? {
        Value::String(text) => parse_ms(text),
        Value::Number(number) => {
            let ms = number.as_f64()?;
            (ms.is_finite() && ms > 0.0)
                .then(|| Duration::from_millis(ms.min(u64::MAX as f64) as u64))
        }
        _ => None,
    }
}

/// Removes stray backslashes (e.g. from copy-pasted urls like `\?utm_source\=x`) while keeping valid JSON escapes.
fn sanitize_json_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.peek().copied() {
                Some(next)
                    if matches!(next, '\\' | '/' | '"' | 'b' | 'f' | 'n' | 'r' | 't' | 'u') =>
                {
                    out.push(ch);
                    out.push(next);
                    chars.next();
                }
                _ => {}
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn field(value: &Value, key: &str) -> Value {
    value.get(key).cloned().unwrap_or(Value::Null)
}

fn info_entry(value: &Value) -> &Value {
    value
        .get("entries")
        .and_then(Value::as_array)
        .and_then(|entries| entries.iter().find(|entry| entry.is_object()))
        .unwrap_or(value)
}

fn trim_formats(entry: &Value) -> Vec<Value> {
    entry
        .get("formats")
        .and_then(Value::as_array)
        .map(|formats| {
            formats
                .iter()
                .map(|format| {
                    json!({
                        "height": field(format, "height"),
                        "ext": field(format, "ext"),
                        "vcodec": field(format, "vcodec"),
                        "acodec": field(format, "acodec"),
                        "filesize": field(format, "filesize"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn trim_info(value: &Value) -> Value {
    let entry = info_entry(value);
    json!({
        "title": field(entry, "title"),
        "duration": field(entry, "duration"),
        "thumbnail": field(entry, "thumbnail"),
        "uploader": field(entry, "uploader"),
        "is_live": entry.get("is_live").cloned().unwrap_or(Value::Bool(false)),
        "formats": trim_formats(entry),
    })
}

struct DownloadRequest {
    url: String,
    platform: Platform,
    cookies: Option<String>,
    format: Option<String>,
    mode: Mode,
    quality: Quality,
    container: Container,
    item: usize,
    timeout: Option<Duration>,
}

struct InfoRequest {
    url: String,
    platform: Platform,
    cookies: Option<String>,
    timeout: Option<Duration>,
}

/// Removes the per-download temp directory (media, cookie jar, yt-dlp fragments).
struct Cleanup {
    dir: PathBuf,
}

impl Cleanup {
    fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Owns the downloaded file and deletes it (plus the cookie jar) once the response body is dropped.
struct TempFileReader {
    file: fs::File,
    _cleanup: Cleanup,
}

impl AsyncRead for TempFileReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.file).poll_read(cx, buf)
    }
}

/// Creates the per-request temp directory and writes the optional cookie jar.
async fn prepare_cookies(
    config: &Config,
    cookies: Option<&str>,
    platform: Platform,
) -> Result<(PathBuf, Cleanup, Option<PathBuf>), ApiError> {
    let dir = config.tmp.join(format!("ixian-{}", Uuid::new_v4()));
    fs::create_dir(&dir)
        .await
        .map_err(|error| ApiError::internal(format!("cannot create temp directory: {error}")))?;
    let cleanup = Cleanup::new(dir.clone());
    if !cookies.is_some_and(|cookies| !cookies.is_empty()) {
        return Ok((dir, cleanup, None));
    }

    let jar = dir.join("cookies.txt");
    fs::write(&jar, cookie_file(cookies.unwrap_or_default(), platform))
        .await
        .map_err(|error| ApiError::internal(format!("cannot write cookie jar: {error}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&jar, std::fs::Permissions::from_mode(0o600)).await;
    }
    Ok((dir, cleanup, Some(jar)))
}

struct YtdlpOutput {
    success: bool,
    status_code: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn run_ytdlp(mut command: Command, limit: Duration) -> Result<YtdlpOutput, ApiError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command.spawn().map_err(|error| {
        ApiError::bad_gateway(
            format!("failed to run yt-dlp: {error}"),
            Some(hint_for(StatusCode::BAD_GATEWAY)),
        )
    })?;

    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| ApiError::internal("cannot capture yt-dlp stdout"))?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| ApiError::internal("cannot capture yt-dlp stderr"))?;
    let stdout_task = tokio::spawn(async move {
        let mut text = String::new();
        stdout_pipe.read_to_string(&mut text).await.map(|_| text)
    });
    let stderr_task = tokio::spawn(async move {
        let mut text = String::new();
        stderr_pipe.read_to_string(&mut text).await.map(|_| text)
    });

    let status = match timeout(limit, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            return Err(ApiError::bad_gateway(
                format!("failed to wait for yt-dlp: {error}"),
                Some(hint_for(StatusCode::BAD_GATEWAY)),
            ));
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            // Do not wait on the pipes: yt-dlp's children (ffmpeg, curl) can keep them open.
            stdout_task.abort();
            stderr_task.abort();
            return Err(ApiError::classified(
                StatusCode::GATEWAY_TIMEOUT,
                format!("yt-dlp timed out after {}ms", limit.as_millis()),
            ));
        }
    };

    let stdout = stdout_task
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let stderr = stderr_task
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();

    Ok(YtdlpOutput {
        success: status.success(),
        status_code: status.code(),
        stdout,
        stderr,
    })
}

async fn download(config: &Config, request: DownloadRequest) -> ApiResult {
    let DownloadRequest {
        url,
        platform,
        cookies,
        format,
        mode,
        quality,
        container,
        item,
        timeout: request_timeout,
    } = request;
    let url = url.replace('\\', "");
    let hint = cookies
        .as_deref()
        .is_some_and(|cookies| !cookies.is_empty())
        .then_some(COOKIE_HINT);

    let (dir, cleanup, jar) = prepare_cookies(config, cookies.as_deref(), platform).await?;

    let selector = format.unwrap_or_else(|| {
        if mode.is_audio() {
            "ba/b".to_string()
        } else {
            quality.selector(container, &config.format)
        }
    });

    let mut cmd = Command::new(&config.ytdlp);
    cmd.arg("--no-playlist")
        // `--no-playlist` does not collapse tweets that reference other tweets
        // (yt-dlp returns them as a playlist), so pin the requested item explicitly.
        .arg("--playlist-items")
        .arg(item.to_string())
        .arg("--no-progress")
        .arg("--no-simulate")
        .arg("--print")
        .arg("after_move:filepath")
        .arg("-f")
        .arg(&selector)
        .arg("-o")
        .arg(dir.join("%(title)s [%(id)s].%(ext)s"));
    if mode.is_audio() {
        cmd.arg("-x")
            .arg("--audio-format")
            .arg("m4a")
            .arg("--audio-quality")
            .arg("0");
    } else {
        cmd.arg("--merge-output-format").arg(container.name());
    }
    if let Some(plugin_dir) = &config.plugin_dir {
        cmd.arg("--plugin-dirs").arg(plugin_dir);
    }
    if let Some(jar) = &jar {
        cmd.arg("--cookies").arg(jar);
    }
    cmd.arg(&url);

    let limit = request_timeout.unwrap_or(config.timeout);
    let output = run_ytdlp(cmd, limit).await?;
    if !output.success {
        return Err(ytdlp_failure(&output.stderr, output.status_code, hint));
    }

    let media = nth_filepath(&output.stdout, item).ok_or_else(|| {
        ApiError::bad_gateway(format!("yt-dlp did not produce item {item}"), hint)
    })?;

    let metadata = fs::metadata(&media)
        .await
        .map_err(|_| ApiError::bad_gateway("yt-dlp did not produce an output file", hint))?;
    if !metadata.is_file() {
        return Err(ApiError::bad_gateway(
            "yt-dlp did not produce an output file",
            hint,
        ));
    }

    let file = fs::File::open(&media).await.map_err(|error| {
        ApiError::bad_gateway(format!("cannot read output file: {error}"), hint)
    })?;
    let name = media
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(if mode.is_audio() { "audio" } else { "video" })
        .to_string();
    let mime = if mode.is_audio() {
        "audio/mp4".to_string()
    } else {
        mime_guess::from_path(&media)
            .first_or_octet_stream()
            .to_string()
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&metadata.len().to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    let disposition = format!(
        "attachment; filename*=UTF-8''{}",
        percent_encoding::utf8_percent_encode(&name, FILENAME_ENCODE)
    );
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    headers.insert(
        "x-ixian-platform",
        HeaderValue::from_static(platform.name()),
    );

    let reader = TempFileReader {
        file,
        _cleanup: cleanup,
    };
    let body = Body::from_stream(ReaderStream::with_capacity(reader, STREAM_CHUNK));
    let mut response = Response::new(body);
    *response.headers_mut() = headers;
    Ok(response)
}

async fn info(config: &Config, request: InfoRequest) -> Result<Json<Value>, ApiError> {
    let InfoRequest {
        url,
        platform,
        cookies,
        timeout: request_timeout,
    } = request;
    let url = url.replace('\\', "");
    let hint = cookies
        .as_deref()
        .is_some_and(|cookies| !cookies.is_empty())
        .then_some(COOKIE_HINT);

    let (_dir, _cleanup, jar) = prepare_cookies(config, cookies.as_deref(), platform).await?;

    let mut cmd = Command::new(&config.ytdlp);
    cmd.arg("-J")
        .arg("--no-playlist")
        .arg("--playlist-items")
        .arg("1")
        .arg("--no-progress");
    if let Some(plugin_dir) = &config.plugin_dir {
        cmd.arg("--plugin-dirs").arg(plugin_dir);
    }
    if let Some(jar) = &jar {
        cmd.arg("--cookies").arg(jar);
    }
    cmd.arg(&url);

    let limit = request_timeout.unwrap_or(config.info_timeout);
    let output = run_ytdlp(cmd, limit).await?;
    if !output.success {
        return Err(ytdlp_failure(&output.stderr, output.status_code, hint));
    }

    let parsed: Value = serde_json::from_str(output.stdout.trim()).map_err(|_| {
        ApiError::bad_gateway(
            "yt-dlp returned invalid JSON",
            Some("the url may not be a single video; try the full post link"),
        )
    })?;
    Ok(Json(trim_info(&parsed)))
}

async fn root() -> Json<Value> {
    Json(json!({
        "message": "Hello!",
        "platforms": Platform::ALL.map(Platform::name),
    }))
}

fn query_params(uri: &Uri) -> HashMap<String, String> {
    url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
        .into_owned()
        .collect()
}

fn parse_json_body(body: &str) -> Result<Map<String, Value>, ApiError> {
    let parsed: Value = match serde_json::from_str(body) {
        Ok(value) => value,
        Err(_) => serde_json::from_str(&sanitize_json_escapes(body)).map_err(|_| {
            ApiError::bad_request("invalid JSON body (check for stray escapes in the url)")
        })?,
    };
    match parsed {
        Value::Object(object) => Ok(object),
        _ => Err(ApiError::bad_request(
            "invalid JSON body (expected an object with a 'url' field)",
        )),
    }
}

fn query_url(params: &HashMap<String, String>) -> Result<&str, ApiError> {
    params
        .get("url")
        .map(String::as_str)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| ApiError::bad_request("missing 'url' query parameter"))
}

fn object_url(object: &Map<String, Value>) -> Result<&str, ApiError> {
    object
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| ApiError::bad_request("missing 'url' in JSON body"))
}

fn query_cookies(params: &HashMap<String, String>, platform: Platform) -> Option<String> {
    params.get(&format!("{}_cookies", platform.name())).cloned()
}

fn object_cookies(object: &Map<String, Value>, platform: Platform) -> Option<String> {
    object
        .get("cookies")
        .and_then(Value::as_object)
        .and_then(|cookies| cookies.get(platform.name()))
        .and_then(Value::as_str)
        .or_else(|| {
            object
                .get(&format!("{}_cookies", platform.name()))
                .and_then(Value::as_str)
        })
        .map(str::to_string)
}

fn object_format(object: &Map<String, Value>) -> Option<String> {
    object
        .get("format")
        .and_then(Value::as_str)
        .filter(|format| !format.is_empty())
        .map(str::to_string)
}

async fn download_get(State(config): State<Arc<Config>>, uri: Uri) -> ApiResult {
    let params = query_params(&uri);
    let url = query_url(&params)?;
    let platform = resolve_platform(url, params.get("platform").map(String::as_str))?;

    download(
        &config,
        DownloadRequest {
            url: url.to_string(),
            platform,
            cookies: query_cookies(&params, platform),
            format: params
                .get("format")
                .filter(|format| !format.is_empty())
                .cloned(),
            mode: Mode::parse(params.get("mode").map(String::as_str))?,
            quality: Quality::parse(params.get("quality").map(String::as_str))?,
            container: Container::parse(params.get("container").map(String::as_str))?,
            item: parse_item(params.get("item").map(String::as_str))?,
            timeout: params.get("timeout_ms").and_then(|value| parse_ms(value)),
        },
    )
    .await
}

async fn download_post(State(config): State<Arc<Config>>, body: String) -> ApiResult {
    let object = parse_json_body(&body)?;
    let url = object_url(&object)?;
    let platform = resolve_platform(url, object.get("platform").and_then(Value::as_str))?;

    download(
        &config,
        DownloadRequest {
            url: url.to_string(),
            platform,
            cookies: object_cookies(&object, platform),
            format: object_format(&object),
            mode: Mode::parse(object.get("mode").and_then(Value::as_str))?,
            quality: Quality::parse(object.get("quality").and_then(Value::as_str))?,
            container: Container::parse(object.get("container").and_then(Value::as_str))?,
            item: item_from_json(object.get("item"))?,
            timeout: parse_ms_value(object.get("timeoutMs")),
        },
    )
    .await
}

async fn info_get(State(config): State<Arc<Config>>, uri: Uri) -> Result<Json<Value>, ApiError> {
    let params = query_params(&uri);
    let url = query_url(&params)?;
    let platform = resolve_platform(url, params.get("platform").map(String::as_str))?;

    info(
        &config,
        InfoRequest {
            url: url.to_string(),
            platform,
            cookies: query_cookies(&params, platform),
            timeout: params.get("timeout_ms").and_then(|value| parse_ms(value)),
        },
    )
    .await
}

async fn info_post(
    State(config): State<Arc<Config>>,
    body: String,
) -> Result<Json<Value>, ApiError> {
    let object = parse_json_body(&body)?;
    let url = object_url(&object)?;
    let platform = resolve_platform(url, object.get("platform").and_then(Value::as_str))?;

    info(
        &config,
        InfoRequest {
            url: url.to_string(),
            platform,
            cookies: object_cookies(&object, platform),
            timeout: parse_ms_value(object.get("timeoutMs")),
        },
    )
    .await
}

async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not found")
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
}

fn app(config: Arc<Config>) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/download", get(download_get).post(download_post))
        .route("/info", get(info_get).post(info_post))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(config)
}

#[tokio::main]
async fn main() {
    let config = Arc::new(Config::from_env());
    let listener = TcpListener::bind((config.host.as_str(), config.port))
        .await
        .unwrap_or_else(|error| {
            panic!("cannot listen on {}:{}: {error}", config.host, config.port)
        });
    println!("ixian on http://{}:{}", config.host, config.port);
    axum::serve(listener, app(config))
        .await
        .unwrap_or_else(|error| panic!("server error: {error}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_supported_hosts() {
        assert_eq!(
            detect_platform("https://www.youtube.com/watch?v=abc"),
            Some(Platform::YouTube)
        );
        assert_eq!(
            detect_platform("https://youtu.be/abc"),
            Some(Platform::YouTube)
        );
        assert_eq!(
            detect_platform("https://m.youtube.com/watch?v=abc"),
            Some(Platform::YouTube)
        );
        assert_eq!(
            detect_platform("https://x.com/user/status/1"),
            Some(Platform::X)
        );
        assert_eq!(
            detect_platform("https://twitter.com/user/status/1"),
            Some(Platform::X)
        );
        assert_eq!(
            detect_platform("https://www.instagram.com/reel/abc/"),
            Some(Platform::Instagram)
        );
        assert_eq!(detect_platform("https://example.com/video"), None);
        assert_eq!(detect_platform("not a url"), None);
    }

    #[test]
    fn platform_override_wins_and_validates() {
        assert_eq!(
            resolve_platform("https://example.com/v", Some("instagram")).unwrap(),
            Platform::Instagram
        );
        assert!(resolve_platform("https://example.com/v", Some("tiktok")).is_err());
        assert_eq!(
            resolve_platform("https://example.com/v", Some(""))
                .unwrap_err()
                .status,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn builds_netscape_cookie_file_from_header() {
        let file = cookie_file("Cookie: auth_token=abc; ct0=d=ef", Platform::X);
        let lines: Vec<_> = file.lines().collect();
        assert_eq!(lines[0], "# Netscape HTTP Cookie File");
        assert!(file.contains(".x.com\tTRUE\t/\tTRUE\t2147483647\tauth_token\tabc"));
        assert!(file.contains(".twitter.com\tTRUE\t/\tTRUE\t2147483647\tauth_token\tabc"));
        assert!(file.contains(".x.com\tTRUE\t/\tTRUE\t2147483647\tct0\td=ef"));
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn passes_through_netscape_cookie_files() {
        let netscape =
            "# Netscape HTTP Cookie File\n.instagram.com\tTRUE\t/\tTRUE\t0\tsessionid\tx\n";
        assert_eq!(cookie_file(netscape, Platform::Instagram), netscape);
    }

    #[test]
    fn sanitizes_only_stray_backslashes() {
        let sanitized = sanitize_json_escapes(r#"{"url":"https:\/\/x.com\/a\?b\=c"}"#);
        let value: Value = serde_json::from_str(&sanitized).unwrap();
        assert_eq!(value["url"], "https://x.com/a?b=c");
        assert_eq!(
            sanitize_json_escapes(r#"{"url":"https://x.com/a\\b"}"#),
            r#"{"url":"https://x.com/a\\b"}"#
        );
    }

    #[test]
    fn picks_nth_output_filepath() {
        assert_eq!(
            nth_filepath("/tmp/a.mp4\n/tmp/b.mp4\n", 1),
            Some(PathBuf::from("/tmp/a.mp4"))
        );
        assert_eq!(
            nth_filepath("\n  /tmp/a.mp4  \n", 1),
            Some(PathBuf::from("/tmp/a.mp4"))
        );
        assert_eq!(
            nth_filepath("/tmp/a.mp4\n/tmp/b.mp4\n", 2),
            Some(PathBuf::from("/tmp/b.mp4"))
        );
        assert_eq!(nth_filepath("/tmp/a.mp4\n", 2), None);
        assert_eq!(nth_filepath(" \n\n", 1), None);
    }

    #[test]
    fn maps_quality_presets_to_selectors() {
        let selector = |quality: Quality| quality.selector(Container::Mp4, DEFAULT_FORMAT);
        assert_eq!(selector(Quality::Best), DEFAULT_FORMAT);
        assert!(selector(Quality::P1080).contains("height<=1080"));
        assert!(selector(Quality::P720).contains("height<=720"));
        assert!(selector(Quality::P480).contains("height<=480"));
        assert!(selector(Quality::Data).contains("height<=480"));
        assert!(Quality::Best
            .selector(Container::Webm, DEFAULT_FORMAT)
            .contains("webm"));
    }

    #[test]
    fn parses_modes_qualities_containers_and_items() {
        assert_eq!(Mode::parse(None).unwrap(), Mode::Video);
        assert_eq!(Mode::parse(Some("audio")).unwrap(), Mode::Audio);
        assert!(Mode::parse(Some("video ")).is_err());

        assert_eq!(Quality::parse(Some("720")).unwrap(), Quality::P720);
        assert_eq!(Quality::parse(Some("data")).unwrap(), Quality::Data);
        assert!(Quality::parse(Some("144")).is_err());

        assert_eq!(Container::parse(Some("webm")).unwrap(), Container::Webm);
        assert!(Container::parse(Some("mkv")).is_err());

        assert_eq!(parse_item(None).unwrap(), 1);
        assert_eq!(parse_item(Some("2")).unwrap(), 2);
        assert!(parse_item(Some("0")).is_err());
        assert!(parse_item(Some("x")).is_err());
        assert_eq!(item_from_json(Some(&json!(3))).unwrap(), 3);
        assert!(item_from_json(Some(&json!(0))).is_err());
    }

    #[test]
    fn trims_info_metadata_and_formats() {
        let info = json!({
            "_type": "playlist",
            "entries": [{
                "title": "Example",
                "duration": 42.5,
                "thumbnail": "https://img.example/x.jpg",
                "uploader": "Channel",
                "is_live": false,
                "formats": [{
                    "format_id": "137",
                    "height": 1080,
                    "ext": "mp4",
                    "vcodec": "avc1.640028",
                    "acodec": "none",
                    "filesize": 123,
                    "url": "https://secret.example/video"
                }]
            }]
        });

        let trimmed = trim_info(&info);
        assert_eq!(trimmed["title"], "Example");
        assert_eq!(trimmed["duration"], 42.5);
        assert_eq!(trimmed["thumbnail"], "https://img.example/x.jpg");
        assert_eq!(trimmed["uploader"], "Channel");
        assert_eq!(trimmed["is_live"], false);
        assert_eq!(trimmed["formats"][0]["height"], 1080);
        assert_eq!(trimmed["formats"][0]["ext"], "mp4");
        assert_eq!(trimmed["formats"][0]["filesize"], 123);
        assert!(trimmed["formats"][0].get("url").is_none());
        assert!(trimmed["formats"][0].get("format_id").is_none());
    }

    #[test]
    fn classifies_ytdlp_errors() {
        let cases = [
            (
                "ERROR: HTTP Error 400: Bad Request",
                StatusCode::BAD_REQUEST,
            ),
            (
                "ERROR: Sign in to confirm your age",
                StatusCode::UNAUTHORIZED,
            ),
            ("ERROR: This video is private", StatusCode::FORBIDDEN),
            ("ERROR: Video unavailable", StatusCode::NOT_FOUND),
            (
                "ERROR: HTTP Error 429: Too Many Requests",
                StatusCode::TOO_MANY_REQUESTS,
            ),
            (
                "ERROR: not available in your country",
                StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS,
            ),
            (
                "ERROR: HTTP Error 504: Gateway Timeout",
                StatusCode::GATEWAY_TIMEOUT,
            ),
        ];
        for (stderr, status) in cases {
            let error = ytdlp_failure(stderr, Some(1), None);
            assert_eq!(error.status, status, "{stderr}");
            assert!(error.hint.is_some(), "{stderr}");
        }

        let error = ytdlp_failure("extractor exploded", Some(1), None);
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert!(error.hint.is_some());
    }

    #[test]
    fn parses_timeouts() {
        assert_eq!(parse_ms("1500"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_ms("0"), None);
        assert_eq!(parse_ms("-5"), None);
        assert_eq!(parse_ms("nope"), None);
    }

    #[test]
    fn maps_output_extensions_to_mime_types() {
        assert_eq!(
            mime_guess::from_path("/tmp/a.mp4")
                .first_or_octet_stream()
                .as_ref(),
            "video/mp4"
        );
        assert_eq!(
            mime_guess::from_path("/tmp/a.webm")
                .first_or_octet_stream()
                .as_ref(),
            "video/webm"
        );
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use axum::http::{Method, Request};
    use http_body_util::BodyExt;
    use std::{fs as stdfs, os::unix::fs::PermissionsExt};
    use tempfile::TempDir;
    use tower::ServiceExt;

    struct Harness {
        tmp: TempDir,
        logs: TempDir,
        script: PathBuf,
    }

    impl Harness {
        fn new(extra: &str) -> Self {
            let tmp = TempDir::new().unwrap();
            let logs = TempDir::new().unwrap();
            let script = logs.path().join("yt-dlp-stub.sh");
            let log_dir = logs.path().display();
            stdfs::write(
                &script,
                format!(
                    r#"#!/bin/sh
set -eu
LOG="{log_dir}"
out=""
prev=""
jar=""
ext="mp4"
for arg in "$@"; do
  case "$prev" in
    -o) out="$arg" ;;
    --cookies) jar="$arg" ;;
    --merge-output-format) ext="$arg" ;;
    --audio-format) ext="$arg" ;;
  esac
  prev="$arg"
done
printf '%s\n' "$@" > "$LOG/args.txt"
if [ -n "$jar" ]; then cp -p "$jar" "$LOG/jar.txt"; fi
{extra}
prefix="${{out%%\%*}}"
printf 'partial' > "${{prefix}}partial.part"
if [ -f "$LOG/items" ]; then
  n=$(cat "$LOG/items")
  i=1
  while [ "$i" -le "$n" ]; do
    printf 'item %s' "$i" > "${{prefix}}item${{i}}.${{ext}}"
    printf '%s\n' "${{prefix}}item${{i}}.${{ext}}"
    i=$((i+1))
  done
  exit 0
fi
printf 'fake video data' > "${{prefix}}fake.${{ext}}"
printf '%s\n' "${{prefix}}fake.${{ext}}"
"#
                ),
            )
            .unwrap();
            stdfs::set_permissions(&script, stdfs::Permissions::from_mode(0o755)).unwrap();
            Self { tmp, logs, script }
        }

        fn config(&self, timeout: Duration) -> Arc<Config> {
            self.config_full(timeout, timeout, None)
        }

        fn config_with_plugin_dir(
            &self,
            timeout: Duration,
            plugin_dir: Option<PathBuf>,
        ) -> Arc<Config> {
            self.config_full(timeout, timeout, plugin_dir)
        }

        fn config_with_info_timeout(
            &self,
            timeout: Duration,
            info_timeout: Duration,
        ) -> Arc<Config> {
            self.config_full(timeout, info_timeout, None)
        }

        fn config_full(
            &self,
            timeout: Duration,
            info_timeout: Duration,
            plugin_dir: Option<PathBuf>,
        ) -> Arc<Config> {
            Arc::new(Config {
                host: "127.0.0.1".into(),
                port: 0,
                ytdlp: self.script.to_string_lossy().into_owned(),
                plugin_dir,
                format: DEFAULT_FORMAT.into(),
                timeout,
                info_timeout,
                tmp: self.tmp.path().to_path_buf(),
            })
        }
    }

    async fn request(
        app: Router,
        method: Method,
        uri: &str,
        body: Option<String>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut builder = Request::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        let request = builder.body(Body::from(body.unwrap_or_default())).unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, headers, body)
    }

    fn arg_value(args: &str, flag: &str) -> String {
        let lines: Vec<&str> = args.lines().collect();
        let index = lines
            .iter()
            .position(|arg| *arg == flag)
            .unwrap_or_else(|| panic!("missing {flag} in {args}"));
        lines[index + 1].to_string()
    }

    #[tokio::test]
    async fn streams_download_and_cleans_temp_files() {
        let harness = Harness::new("");
        let app = app(harness.config(Duration::from_secs(30)));
        let payload =
            r#"{"url":"https://www.youtube.com/watch?v=abc","format":"b[ext=mp4]"}"#.to_string();

        let (status, headers, body) = request(app, Method::POST, "/download", Some(payload)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE].to_str().unwrap(), "video/mp4");
        assert_eq!(headers["x-ixian-platform"].to_str().unwrap(), "youtube");
        let disposition = headers[header::CONTENT_DISPOSITION].to_str().unwrap();
        assert!(
            disposition.contains("filename*=UTF-8''fake.mp4"),
            "{disposition}"
        );
        assert!(!disposition.contains("ixian-"), "{disposition}");
        assert_eq!(body, b"fake video data");
        tokio::task::yield_now().await;
        assert_eq!(stdfs::read_dir(harness.tmp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn writes_cookie_jar_and_passes_it_to_ytdlp() {
        let harness = Harness::new("");
        let app = app(harness.config(Duration::from_secs(30)));
        let payload = serde_json::json!({
            "url": "https://x.com/user/status/1",
            "cookies": { "x": "auth_token=abc; ct0=d=ef" },
        })
        .to_string();

        let (status, headers, _) = request(app, Method::POST, "/download", Some(payload)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["x-ixian-platform"].to_str().unwrap(), "x");
        let args = stdfs::read_to_string(harness.logs.path().join("args.txt")).unwrap();
        assert!(args.lines().any(|arg| arg == "--cookies"), "{args}");
        let jar_path = harness.logs.path().join("jar.txt");
        let jar = stdfs::read_to_string(&jar_path).unwrap();
        assert!(jar.contains(".x.com\tTRUE\t/\tTRUE\t2147483647\tauth_token\tabc"));
        assert!(jar.contains(".twitter.com\tTRUE\t/\tTRUE\t2147483647\tct0\td=ef"));
        let mode = stdfs::metadata(&jar_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn passes_plugin_dir_when_configured() {
        let harness = Harness::new("");
        let plugins = harness.tmp.path().join("plugins");
        stdfs::create_dir(&plugins).unwrap();
        let app =
            app(harness.config_with_plugin_dir(Duration::from_secs(30), Some(plugins.clone())));
        let payload = serde_json::json!({ "url": "https://youtu.be/abc" }).to_string();

        let (status, _, _) = request(app, Method::POST, "/download", Some(payload)).await;

        assert_eq!(status, StatusCode::OK);
        let args = stdfs::read_to_string(harness.logs.path().join("args.txt")).unwrap();
        let lines: Vec<&str> = args.lines().collect();
        let index = lines
            .iter()
            .position(|arg| *arg == "--plugin-dirs")
            .unwrap();
        assert_eq!(lines[index + 1], plugins.to_string_lossy().as_ref());
    }

    #[tokio::test]
    async fn limits_downloads_to_first_playlist_item() {
        let harness = Harness::new("");
        let app = app(harness.config(Duration::from_secs(30)));
        let payload = serde_json::json!({ "url": "https://x.com/user/status/1" }).to_string();

        let (status, _, _) = request(app, Method::POST, "/download", Some(payload)).await;

        assert_eq!(status, StatusCode::OK);
        let args = stdfs::read_to_string(harness.logs.path().join("args.txt")).unwrap();
        let lines: Vec<&str> = args.lines().collect();
        let index = lines
            .iter()
            .position(|arg| *arg == "--playlist-items")
            .unwrap();
        assert_eq!(lines[index + 1], "1");
    }

    #[tokio::test]
    async fn picks_requested_playlist_item() {
        let harness = Harness::new("");
        stdfs::write(harness.logs.path().join("items"), "3").unwrap();
        let app = app(harness.config(Duration::from_secs(30)));
        let payload =
            serde_json::json!({ "url": "https://x.com/user/status/1", "item": 2 }).to_string();

        let (status, headers, body) = request(app, Method::POST, "/download", Some(payload)).await;

        assert_eq!(status, StatusCode::OK);
        let disposition = headers[header::CONTENT_DISPOSITION].to_str().unwrap();
        assert!(disposition.contains("item2.mp4"), "{disposition}");
        assert_eq!(body, b"item 2");
        let args = stdfs::read_to_string(harness.logs.path().join("args.txt")).unwrap();
        assert_eq!(arg_value(&args, "--playlist-items"), "2");
    }

    #[tokio::test]
    async fn audio_mode_extracts_m4a() {
        let harness = Harness::new("");
        let app = app(harness.config(Duration::from_secs(30)));
        let payload = serde_json::json!({
            "url": "https://www.youtube.com/watch?v=abc",
            "mode": "audio",
        })
        .to_string();

        let (status, headers, body) = request(app, Method::POST, "/download", Some(payload)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE].to_str().unwrap(), "audio/mp4");
        assert!(headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .contains("fake.m4a"));
        assert_eq!(body, b"fake video data");
        let args = stdfs::read_to_string(harness.logs.path().join("args.txt")).unwrap();
        assert_eq!(arg_value(&args, "-f"), "ba/b");
        assert!(args.lines().any(|arg| arg == "-x"), "{args}");
        assert_eq!(arg_value(&args, "--audio-format"), "m4a");
        assert_eq!(arg_value(&args, "--audio-quality"), "0");
        assert!(
            !args.lines().any(|arg| arg == "--merge-output-format"),
            "{args}"
        );
    }

    #[tokio::test]
    async fn maps_quality_and_container_args() {
        let harness = Harness::new("");
        let app = app(harness.config(Duration::from_secs(30)));

        let payload = serde_json::json!({
            "url": "https://youtu.be/abc",
            "quality": "720",
        })
        .to_string();
        let (status, _, _) = request(app.clone(), Method::POST, "/download", Some(payload)).await;
        assert_eq!(status, StatusCode::OK);
        let args = stdfs::read_to_string(harness.logs.path().join("args.txt")).unwrap();
        let selector = arg_value(&args, "-f");
        assert!(selector.contains("height<=720"), "{selector}");
        assert_eq!(arg_value(&args, "--merge-output-format"), "mp4");

        let payload = serde_json::json!({
            "url": "https://youtu.be/abc",
            "container": "webm",
        })
        .to_string();
        let (status, headers, _) =
            request(app.clone(), Method::POST, "/download", Some(payload)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers[header::CONTENT_TYPE].to_str().unwrap(),
            "video/webm"
        );
        let args = stdfs::read_to_string(harness.logs.path().join("args.txt")).unwrap();
        assert_eq!(arg_value(&args, "--merge-output-format"), "webm");

        let payload = serde_json::json!({
            "url": "https://youtu.be/abc",
            "quality": "720",
            "format": "b[ext=webm]",
        })
        .to_string();
        let (status, _, _) = request(app, Method::POST, "/download", Some(payload)).await;
        assert_eq!(status, StatusCode::OK);
        let args = stdfs::read_to_string(harness.logs.path().join("args.txt")).unwrap();
        assert_eq!(arg_value(&args, "-f"), "b[ext=webm]");
    }

    #[tokio::test]
    async fn probes_info_and_trims_formats() {
        let info = serde_json::json!({
            "title": "Example",
            "duration": 42.5,
            "thumbnail": "https://img.example/x.jpg",
            "uploader": "Channel",
            "is_live": false,
            "formats": [{
                "format_id": "137",
                "height": 1080,
                "ext": "mp4",
                "vcodec": "avc1.640028",
                "acodec": "none",
                "filesize": 123,
                "url": "https://secret.example/video"
            }]
        });
        let harness = Harness::new(&format!("printf '%s' '{info}'\nexit 0"));
        let app = app(harness.config(Duration::from_secs(30)));

        let (status, headers, body) = request(
            app,
            Method::GET,
            "/info?url=https://www.youtube.com/watch?v=abc",
            None,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers[header::CONTENT_TYPE].to_str().unwrap(),
            "application/json"
        );
        let info: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(info["title"], "Example");
        assert_eq!(info["duration"], 42.5);
        assert_eq!(info["thumbnail"], "https://img.example/x.jpg");
        assert_eq!(info["uploader"], "Channel");
        assert_eq!(info["is_live"], false);
        assert_eq!(info["formats"][0]["height"], 1080);
        assert_eq!(info["formats"][0]["filesize"], 123);
        assert!(info["formats"][0].get("url").is_none());
        assert!(info["formats"][0].get("format_id").is_none());

        let args = stdfs::read_to_string(harness.logs.path().join("args.txt")).unwrap();
        assert_eq!(args.lines().next(), Some("-J"));
        assert_eq!(arg_value(&args, "--playlist-items"), "1");
    }

    #[tokio::test]
    async fn info_reads_first_playlist_entry() {
        let info = serde_json::json!({
            "_type": "playlist",
            "entries": [
                { "title": "First", "formats": [] },
                { "title": "Second", "formats": [] }
            ]
        });
        let harness = Harness::new(&format!("printf '%s' '{info}'\nexit 0"));
        let app = app(harness.config(Duration::from_secs(30)));
        let payload = serde_json::json!({ "url": "https://x.com/user/status/1" }).to_string();

        let (status, _, body) = request(app, Method::POST, "/info", Some(payload)).await;

        assert_eq!(status, StatusCode::OK);
        let info: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(info["title"], "First");
    }

    #[tokio::test]
    async fn info_times_out_with_gateway_timeout() {
        let harness = Harness::new("sleep 5");
        let app =
            app(harness
                .config_with_info_timeout(Duration::from_secs(30), Duration::from_millis(100)));
        let payload = serde_json::json!({ "url": "https://youtu.be/abc" }).to_string();

        let started = std::time::Instant::now();
        let (status, _, body) = request(app, Method::POST, "/info", Some(payload)).await;

        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["code"], 504);
        assert!(error["hint"].is_string());
    }

    #[tokio::test]
    async fn classifies_download_errors_with_code_and_hint() {
        let harness = Harness::new("echo 'ERROR: [youtube] abc: Video unavailable' >&2; exit 1");
        let app = app(harness.config(Duration::from_secs(30)));
        let payload = serde_json::json!({ "url": "https://youtu.be/abc" }).to_string();

        let (status, _, body) = request(app, Method::POST, "/download", Some(payload)).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["code"], 404);
        assert!(error["hint"].is_string());
    }

    #[tokio::test]
    async fn reports_ytdlp_failures_as_bad_gateway() {
        let harness = Harness::new("echo 'extractor exploded' >&2; exit 1");
        let app = app(harness.config(Duration::from_secs(30)));
        let payload = serde_json::json!({
            "url": "https://www.instagram.com/reel/abc/",
            "cookies": { "instagram": "sessionid=bad" },
        })
        .to_string();

        let (status, _, body) = request(app, Method::POST, "/download", Some(payload)).await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"], "extractor exploded");
        assert_eq!(error["hint"], COOKIE_HINT);
        tokio::task::yield_now().await;
        assert_eq!(stdfs::read_dir(harness.tmp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn times_out_without_waiting_for_child_pipes() {
        let harness = Harness::new("sleep 5");
        let app = app(harness.config(Duration::from_secs(30)));
        let payload = serde_json::json!({
            "url": "https://youtu.be/abc",
            "timeoutMs": 100,
        })
        .to_string();

        let started = std::time::Instant::now();
        let (status, _, body) = request(app, Method::POST, "/download", Some(payload)).await;

        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["code"], 504);
        assert!(
            error["error"].as_str().unwrap().contains("timed out"),
            "{error}"
        );
        assert_eq!(stdfs::read_dir(harness.tmp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn validates_requests() {
        let harness = Harness::new("");
        let app = app(harness.config(Duration::from_secs(30)));

        let (status, _, body) = request(app.clone(), Method::GET, "/download", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"], "missing 'url' query parameter");

        let payload = r#"{"url":"https://example.com/video","platform":"tiktok"}"#.to_string();
        let (status, _, body) =
            request(app.clone(), Method::POST, "/download", Some(payload)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert!(error["error"]
            .as_str()
            .unwrap()
            .contains("unknown platform"));

        let payload = r#"{"url":"https://example.com/video"}"#.to_string();
        let (status, _, body) =
            request(app.clone(), Method::POST, "/download", Some(payload)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert!(error["error"]
            .as_str()
            .unwrap()
            .contains("cannot detect platform"));

        let payload = r#"{"url":"https://www.youtube.com/watch\?v\=abc"}"#.to_string();
        let (status, _, _) = request(app.clone(), Method::POST, "/download", Some(payload)).await;
        assert_eq!(status, StatusCode::OK);

        for payload in [
            json!({ "url": "https://youtu.be/abc", "mode": "radio" }),
            json!({ "url": "https://youtu.be/abc", "quality": "144" }),
            json!({ "url": "https://youtu.be/abc", "container": "mkv" }),
            json!({ "url": "https://youtu.be/abc", "item": 0 }),
        ] {
            let (status, _, body) = request(
                app.clone(),
                Method::POST,
                "/download",
                Some(payload.to_string()),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{payload}");
            let error: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(error["code"], 400, "{payload}");
        }

        let (status, _, body) = request(app, Method::PUT, "/download", None).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"], "method not allowed");
    }
}
