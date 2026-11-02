// ixian - minimal yt-dlp wrapper (youtube / x.com / instagram) in Rust.
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderMap, HeaderValue, Request, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, HashSet},
    env, io,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt, ReadBuf},
    net::TcpListener,
    process::Command,
    time::{interval, timeout},
};
use tokio_util::{io::ReaderStream, sync::CancellationToken};
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::{DefaultOnResponse, TraceLayer},
};
use tracing::{info, info_span, warn, Level};
use url::Url;
use uuid::Uuid;

const DEFAULT_FORMAT: &str = "bv*[vcodec^=avc1]+ba[acodec^=mp4a]/b[ext=mp4]/bv*+ba/b";
const COOKIE_HINT: &str =
    "cookies may be invalid or incomplete; public posts download without cookies";
const STREAM_CHUNK: usize = 64 * 1024;
const TEMP_PREFIX: &str = "ixian-";
const REAP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const ORPHAN_AGE: Duration = Duration::from_secs(60 * 60);
const REQUEST_ID_HEADER: &str = "x-request-id";
const YTDLP_VERSION_TIMEOUT: Duration = Duration::from_secs(5);
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct EmbedOptions {
    subs: bool,
    thumbnail: bool,
    metadata: bool,
}

impl EmbedOptions {
    /// Parses `subs,thumbnail,metadata`; an empty value means no embeds.
    fn parse(value: &str) -> Result<Self, ApiError> {
        let mut options = Self::default();
        for item in value
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            match item {
                "subs" => options.subs = true,
                "thumbnail" => options.thumbnail = true,
                "metadata" => options.metadata = true,
                other => {
                    return Err(ApiError::bad_request(format!(
                        "unknown embed '{other}' (expected: subs, thumbnail, metadata)"
                    )))
                }
            }
        }
        Ok(options)
    }

    fn from_json(value: Option<&Value>) -> Result<Self, ApiError> {
        match value {
            None | Some(Value::Null) => Ok(Self::default()),
            Some(Value::String(text)) => Self::parse(text),
            Some(Value::Array(items)) => {
                let mut options = Self::default();
                for item in items {
                    let text = item.as_str().ok_or_else(|| {
                        ApiError::bad_request("'embed' array entries must be strings")
                    })?;
                    options = options.merge(Self::parse(text)?);
                }
                Ok(options)
            }
            Some(_) => Err(ApiError::bad_request(
                "'embed' must be a string or an array of strings",
            )),
        }
    }

    fn merge(self, other: Self) -> Self {
        Self {
            subs: self.subs || other.subs,
            thumbnail: self.thumbnail || other.thumbnail,
            metadata: self.metadata || other.metadata,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SponsorBlock {
    #[default]
    Off,
    Remove,
    Mark,
}

impl SponsorBlock {
    fn parse(value: &str) -> Result<Self, ApiError> {
        match value.trim() {
            "" | "off" | "none" => Ok(Self::Off),
            "remove" => Ok(Self::Remove),
            "mark" => Ok(Self::Mark),
            other => Err(ApiError::bad_request(format!(
                "unknown sponsorblock '{other}' (expected: remove, mark)"
            ))),
        }
    }

    fn from_json(value: Option<&Value>) -> Result<Self, ApiError> {
        match value {
            None | Some(Value::Null) => Ok(Self::Off),
            Some(Value::String(text)) => Self::parse(text),
            Some(_) => Err(ApiError::bad_request(
                "'sponsorblock' must be a string (remove or mark)",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Compatibility {
    #[default]
    Off,
    Quicktime,
}

impl Compatibility {
    fn parse(value: &str) -> Result<Self, ApiError> {
        match value.trim() {
            "" | "off" | "none" => Ok(Self::Off),
            "quicktime" => Ok(Self::Quicktime),
            other => Err(ApiError::bad_request(format!(
                "unknown compatibility '{other}' (expected: quicktime)"
            ))),
        }
    }

    fn from_json(value: Option<&Value>) -> Result<Self, ApiError> {
        match value {
            None | Some(Value::Null) => Ok(Self::Off),
            Some(Value::String(text)) => Self::parse(text),
            Some(_) => Err(ApiError::bad_request(
                "'compatibility' must be a string (quicktime)",
            )),
        }
    }
}

#[derive(Clone)]
struct Config {
    host: String,
    port: u16,
    ytdlp: String,
    plugin_dir: Option<PathBuf>,
    format: String,
    timeout: Duration,
    tmp: PathBuf,
    ffmpeg: String,
    ffprobe: String,
    cache_dir: Option<PathBuf>,
    shutdown_grace: Duration,
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
        let tmp = non_empty(env::var("TMPDIR").ok())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let ffmpeg = non_empty(env::var("FFMPEG_PATH").ok()).unwrap_or_else(|| "ffmpeg".into());
        let ffprobe = non_empty(env::var("FFPROBE_PATH").ok()).unwrap_or_else(|| "ffprobe".into());
        let cache_dir = non_empty(env::var("IXIAN_CACHE_DIR").ok())
            .map(PathBuf::from)
            .filter(|dir| dir.is_dir());
        let shutdown_grace_ms = env::var("IXIAN_SHUTDOWN_GRACE_MS")
            .ok()
            .and_then(|v| parse_ms(&v))
            .unwrap_or_else(|| Duration::from_secs(15));

        Self {
            host,
            port,
            ytdlp,
            plugin_dir,
            format,
            timeout: timeout_ms,
            tmp,
            ffmpeg,
            ffprobe,
            cache_dir,
            shutdown_grace: shutdown_grace_ms,
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

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            warn!(status = self.status.as_u16(), error = %self.message, "request failed");
        }
        let mut body = Map::new();
        body.insert("error".into(), Value::String(self.message));
        if let Some(hint) = self.hint {
            body.insert("hint".into(), Value::String(hint));
        }
        (self.status, Json(Value::Object(body))).into_response()
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
/// media comes first, the referenced tweet's video after it. Keep the first path.
fn primary_filepath(stdout: &str) -> Option<PathBuf> {
    stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
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

struct AppState {
    config: Config,
    jobs: AtomicUsize,
    started: Instant,
    shutdown: CancellationToken,
    children: Mutex<HashSet<u32>>,
    temp_dirs: Mutex<HashSet<PathBuf>>,
}

impl AppState {
    fn new(config: Config) -> Self {
        Self {
            config,
            jobs: AtomicUsize::new(0),
            started: Instant::now(),
            shutdown: CancellationToken::new(),
            children: Mutex::new(HashSet::new()),
            temp_dirs: Mutex::new(HashSet::new()),
        }
    }

    fn lock_children(&self) -> std::sync::MutexGuard<'_, HashSet<u32>> {
        self.children
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn track_child(&self, pid: Option<u32>) {
        if let Some(pid) = pid {
            self.lock_children().insert(pid);
        }
    }

    fn untrack_child(&self, pid: Option<u32>) {
        if let Some(pid) = pid {
            self.lock_children().remove(&pid);
        }
    }

    fn register_dir(&self, dir: &Path) {
        self.temp_dirs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(dir.to_path_buf());
    }

    fn unregister_dir(&self, dir: &Path) {
        self.temp_dirs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(dir);
    }

    fn active_dirs(&self) -> HashSet<PathBuf> {
        self.temp_dirs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Stops new work, cancels in-flight downloads and kills their process groups.
    fn begin_shutdown(&self) {
        self.shutdown.cancel();
        let children: Vec<u32> = self.lock_children().iter().copied().collect();
        for pid in &children {
            kill_process_group(*pid);
        }
        info!(children = children.len(), "shutdown: cancelled downloads");
    }

    fn cleanup_temp_dirs(&self) {
        let dirs: Vec<PathBuf> = self
            .temp_dirs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drain()
            .collect();
        for dir in dirs {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Decrements the active-job counter when a download finishes or is cancelled.
struct JobGuard<'a>(&'a AtomicUsize);

impl Drop for JobGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Removes a tracked child from the shutdown registry on every exit path.
struct ChildGuard<'a> {
    state: &'a AppState,
    pid: Option<u32>,
}

impl Drop for ChildGuard<'_> {
    fn drop(&mut self) {
        self.state.untrack_child(self.pid);
    }
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    // The child was spawned with process_group(0), so its pid is the group id.
    // Negative pid signals the whole group (yt-dlp + ffmpeg/curl children).
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

async fn stop_child(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        kill_process_group(pid);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill().await;
    }
    let _ = child.wait().await;
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Codecs {
    video: Option<String>,
    audio: Option<String>,
}

/// Reads the first video/audio codec names. `None` means ffprobe is unavailable or the output is unreadable.
async fn probe_codecs(state: &AppState, media: &Path) -> Option<Codecs> {
    let output = timeout(
        Duration::from_secs(30),
        Command::new(&state.config.ffprobe)
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type,codec_name",
                "-of",
                "json",
            ])
            .arg(media)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let parsed: Value = serde_json::from_slice(&output.stdout).ok()?;
    let mut codecs = Codecs::default();
    for stream in parsed.get("streams")?.as_array()? {
        let kind = stream.get("codec_type").and_then(Value::as_str);
        let name = stream
            .get("codec_name")
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase);
        match kind {
            Some("video") if codecs.video.is_none() => codecs.video = name,
            Some("audio") if codecs.audio.is_none() => codecs.audio = name,
            _ => {}
        }
    }
    Some(codecs)
}

/// Guarantees an H.264/AAC MP4: remuxes when the codecs already qualify, otherwise re-encodes.
async fn ensure_quicktime(state: &AppState, media: &Path) -> Result<PathBuf, ApiError> {
    let codecs = probe_codecs(state, media).await;
    let video = codecs.as_ref().and_then(|codecs| codecs.video.as_deref());
    let audio = codecs.as_ref().and_then(|codecs| codecs.audio.as_deref());
    let compatible = video == Some("h264") && audio.is_none_or(|codec| codec == "aac");
    let extension = media
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if compatible && matches!(extension.as_str(), "mp4" | "m4v") {
        return Ok(media.to_path_buf());
    }

    let output = quicktime_output(media);
    let mut cmd = Command::new(&state.config.ffmpeg);
    cmd.args(["-nostdin", "-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(media);
    if compatible {
        cmd.args(["-c", "copy"]);
    } else {
        cmd.args([
            "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p", "-c:a",
            "aac", "-b:a", "192k",
        ]);
    }
    cmd.args(["-movflags", "+faststart"]).arg(&output);
    run_media_command(state, cmd, &output).await?;
    info!(
        from = %media.display(),
        to = %output.display(),
        remux = compatible,
        "converted for QuickTime"
    );
    Ok(output)
}

fn quicktime_output(media: &Path) -> PathBuf {
    let stem = media
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("video");
    let candidate = media.with_file_name(format!("{stem}.mp4"));
    if candidate == media {
        media.with_file_name(format!("{stem} [quicktime].mp4"))
    } else {
        candidate
    }
}

async fn run_media_command(
    state: &AppState,
    mut cmd: Command,
    output: &Path,
) -> Result<(), ApiError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|error| ApiError::internal(format!("failed to run ffmpeg: {error}")))?;
    let pid = child.id();
    state.track_child(pid);
    let _guard = ChildGuard { state, pid };
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| ApiError::internal("cannot capture ffmpeg stderr"))?;
    let stderr_task = tokio::spawn(async move {
        let mut text = String::new();
        stderr_pipe.read_to_string(&mut text).await.map(|_| text)
    });

    let waited = tokio::select! {
        result = timeout(state.config.timeout, child.wait()) => Some(result),
        _ = state.shutdown.cancelled() => None,
    };
    let status = match waited {
        Some(Ok(Ok(status))) => status,
        Some(Ok(Err(error))) => {
            stderr_task.abort();
            return Err(ApiError::internal(format!(
                "failed to wait for ffmpeg: {error}"
            )));
        }
        Some(Err(_)) => {
            stop_child(&mut child).await;
            stderr_task.abort();
            return Err(ApiError::bad_gateway(
                format!(
                    "ffmpeg timed out after {}ms",
                    state.config.timeout.as_millis()
                ),
                None,
            ));
        }
        None => {
            stop_child(&mut child).await;
            stderr_task.abort();
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "server shutting down",
            ));
        }
    };

    let stderr = stderr_task
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    if !status.success() {
        let detail = if stderr.trim().is_empty() {
            format!("ffmpeg exited with code {:?}", status.code())
        } else {
            stderr.trim().to_string()
        };
        return Err(ApiError::bad_gateway(
            format!("quicktime conversion failed: {detail}"),
            None,
        ));
    }
    if !output.is_file() {
        return Err(ApiError::internal("ffmpeg did not produce an output file"));
    }
    Ok(())
}

struct DownloadRequest {
    url: String,
    platform: Platform,
    cookies: Option<String>,
    format: Option<String>,
    timeout: Option<Duration>,
    embed: EmbedOptions,
    sponsorblock: SponsorBlock,
    compatibility: Compatibility,
}

/// Removes the per-download temp directory (media, cookie jar, yt-dlp fragments).
struct Cleanup {
    state: Arc<AppState>,
    dir: PathBuf,
}

impl Cleanup {
    fn new(state: Arc<AppState>, dir: PathBuf) -> Self {
        state.register_dir(&dir);
        Self { state, dir }
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        self.state.unregister_dir(&self.dir);
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

async fn download(state: &Arc<AppState>, request: DownloadRequest) -> ApiResult {
    let config = &state.config;
    let DownloadRequest {
        url,
        platform,
        cookies,
        format,
        timeout: request_timeout,
        embed,
        sponsorblock,
        compatibility,
    } = request;
    if state.shutdown.is_cancelled() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "server shutting down",
        ));
    }
    state.jobs.fetch_add(1, Ordering::Relaxed);
    let _job = JobGuard(&state.jobs);
    let url = url.replace('\\', "");
    let has_cookies = cookies.as_deref().is_some_and(|c| !c.is_empty());
    let hint = has_cookies.then_some(COOKIE_HINT);

    let dir = config.tmp.join(format!("{TEMP_PREFIX}{}", Uuid::new_v4()));
    fs::create_dir(&dir)
        .await
        .map_err(|error| ApiError::internal(format!("cannot create temp directory: {error}")))?;
    let jar = dir.join("cookies.txt");
    let cleanup = Cleanup::new(state.clone(), dir.clone());

    if has_cookies {
        let contents = cookie_file(cookies.as_deref().unwrap_or_default(), platform);
        fs::write(&jar, contents)
            .await
            .map_err(|error| ApiError::internal(format!("cannot write cookie jar: {error}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&jar, std::fs::Permissions::from_mode(0o600)).await;
        }
    }

    let mut cmd = Command::new(&config.ytdlp);
    cmd.arg("--no-playlist")
        // `--no-playlist` does not collapse tweets that reference other tweets
        // (yt-dlp returns them as a playlist), so pin the first item explicitly.
        .arg("--playlist-items")
        .arg("1")
        .arg("--no-progress")
        .arg("--no-simulate")
        .arg("--print")
        .arg("after_move:filepath")
        .arg("--merge-output-format")
        .arg("mp4")
        .arg("-f")
        .arg(format.as_deref().unwrap_or(&config.format))
        .arg("-o")
        .arg(dir.join("%(title)s [%(id)s].%(ext)s"));
    if let Some(plugin_dir) = &config.plugin_dir {
        cmd.arg("--plugin-dirs").arg(plugin_dir);
    }
    if embed.subs {
        cmd.arg("--embed-subs");
    }
    if embed.thumbnail {
        cmd.arg("--embed-thumbnail");
    }
    if embed.metadata {
        cmd.arg("--embed-metadata");
    }
    match sponsorblock {
        SponsorBlock::Off => {}
        SponsorBlock::Remove => {
            cmd.args(["--sponsorblock-remove", "all"]);
        }
        SponsorBlock::Mark => {
            cmd.args(["--sponsorblock-mark", "all"]);
        }
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    if has_cookies {
        cmd.arg("--cookies").arg(&jar);
    }
    cmd.arg(&url);

    let mut child = cmd
        .spawn()
        .map_err(|error| ApiError::bad_gateway(format!("failed to run yt-dlp: {error}"), None))?;
    let child_pid = child.id();
    state.track_child(child_pid);
    let _child_guard = ChildGuard {
        state,
        pid: child_pid,
    };

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

    let limit = request_timeout.unwrap_or(config.timeout);
    let waited = tokio::select! {
        result = timeout(limit, child.wait()) => Some(result),
        _ = state.shutdown.cancelled() => None,
    };
    let status = match waited {
        Some(Ok(Ok(status))) => status,
        Some(Ok(Err(error))) => {
            stop_child(&mut child).await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(ApiError::bad_gateway(
                format!("failed to wait for yt-dlp: {error}"),
                hint,
            ));
        }
        Some(Err(_)) => {
            stop_child(&mut child).await;
            // Do not wait on the pipes: yt-dlp's children (ffmpeg, curl) can keep them open.
            stdout_task.abort();
            stderr_task.abort();
            return Err(ApiError::bad_gateway(
                format!("yt-dlp timed out after {}ms", limit.as_millis()),
                hint,
            ));
        }
        None => {
            stop_child(&mut child).await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "server shutting down",
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

    let path = primary_filepath(&stdout);
    let media = match path {
        Some(path) if status.success() => path,
        _ => {
            let detail = if !stderr.trim().is_empty() {
                stderr.trim().to_string()
            } else {
                match status.code() {
                    Some(code) => format!("yt-dlp exited with code {code}"),
                    None => "yt-dlp was terminated by a signal".to_string(),
                }
            };
            return Err(ApiError::bad_gateway(detail, hint));
        }
    };

    let metadata = fs::metadata(&media)
        .await
        .map_err(|_| ApiError::bad_gateway("yt-dlp did not produce an output file", hint))?;
    if !metadata.is_file() {
        return Err(ApiError::bad_gateway(
            "yt-dlp did not produce an output file",
            hint,
        ));
    }

    let media = if compatibility == Compatibility::Quicktime {
        ensure_quicktime(state, &media).await?
    } else {
        media
    };
    let metadata = fs::metadata(&media)
        .await
        .map_err(|_| ApiError::internal("cannot stat the output file"))?;

    let file = fs::File::open(&media).await.map_err(|error| {
        ApiError::bad_gateway(format!("cannot read output file: {error}"), hint)
    })?;
    let name = media
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("video")
        .to_string();
    let mime = mime_guess::from_path(&media).first_or_octet_stream();

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref())
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

async fn root() -> Json<Value> {
    Json(json!({
        "message": "Hello!",
        "platforms": Platform::ALL.map(Platform::name),
    }))
}

async fn download_get(State(state): State<Arc<AppState>>, uri: Uri) -> ApiResult {
    let params: HashMap<String, String> =
        url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
            .into_owned()
            .collect();
    let url = params
        .get("url")
        .map(String::as_str)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| ApiError::bad_request("missing 'url' query parameter"))?;
    let platform = resolve_platform(url, params.get("platform").map(String::as_str))?;
    let embed = params
        .get("embed")
        .map(|value| EmbedOptions::parse(value))
        .transpose()?
        .unwrap_or_default();
    let sponsorblock = params
        .get("sponsorblock")
        .map(|value| SponsorBlock::parse(value))
        .transpose()?
        .unwrap_or_default();
    let compatibility = params
        .get("compatibility")
        .map(|value| Compatibility::parse(value))
        .transpose()?
        .unwrap_or_default();

    download(
        &state,
        DownloadRequest {
            url: url.to_string(),
            platform,
            cookies: params.get(&format!("{}_cookies", platform.name())).cloned(),
            format: params
                .get("format")
                .filter(|format| !format.is_empty())
                .cloned(),
            timeout: params.get("timeout_ms").and_then(|value| parse_ms(value)),
            embed,
            sponsorblock,
            compatibility,
        },
    )
    .await
}

async fn download_post(State(state): State<Arc<AppState>>, body: String) -> ApiResult {
    let parsed: Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => serde_json::from_str(&sanitize_json_escapes(&body)).map_err(|_| {
            ApiError::bad_request("invalid JSON body (check for stray escapes in the url)")
        })?,
    };
    let object = parsed.as_object().ok_or_else(|| {
        ApiError::bad_request("invalid JSON body (expected an object with a 'url' field)")
    })?;

    let url = object
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| ApiError::bad_request("missing 'url' in JSON body"))?;
    let platform = resolve_platform(url, object.get("platform").and_then(Value::as_str))?;

    let cookies = object
        .get("cookies")
        .and_then(Value::as_object)
        .and_then(|cookies| cookies.get(platform.name()))
        .and_then(Value::as_str)
        .or_else(|| {
            object
                .get(&format!("{}_cookies", platform.name()))
                .and_then(Value::as_str)
        })
        .map(str::to_string);

    download(
        &state,
        DownloadRequest {
            url: url.to_string(),
            platform,
            cookies,
            format: object
                .get("format")
                .and_then(Value::as_str)
                .filter(|format| !format.is_empty())
                .map(str::to_string),
            timeout: parse_ms_value(object.get("timeoutMs")),
            embed: EmbedOptions::from_json(object.get("embed"))?,
            sponsorblock: SponsorBlock::from_json(object.get("sponsorblock"))?,
            compatibility: Compatibility::from_json(object.get("compatibility"))?,
        },
    )
    .await
}

async fn healthz(State(state): State<Arc<AppState>>) -> Json<Value> {
    let cache_bytes = match &state.config.cache_dir {
        Some(dir) => dir_size(dir).await,
        None => None,
    };
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "ytdlp_version": ytdlp_version(&state.config).await,
        "active_jobs": state.jobs.load(Ordering::Relaxed),
        "cache_bytes": cache_bytes,
        "disk_free_bytes": disk_free_bytes(&state.config.tmp),
        "uptime_seconds": state.started.elapsed().as_secs(),
    }))
}

async fn ytdlp_version(config: &Config) -> Option<String> {
    let output = timeout(
        YTDLP_VERSION_TIMEOUT,
        Command::new(&config.ytdlp)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!version.is_empty()).then_some(version)
}

async fn dir_size(path: &Path) -> Option<u64> {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = fs::read_dir(&dir).await.ok()?;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(metadata) = entry.metadata().await else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Some(total)
}

#[cfg(unix)]
fn disk_free_bytes(path: &Path) -> Option<u64> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    let stats = unsafe { stats.assume_init() };
    // statvfs field widths differ between macOS and Linux.
    #[allow(clippy::unnecessary_cast)]
    let total = (stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64);
    Some(total)
}

#[cfg(not(unix))]
fn disk_free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Deletes `ixian-*` directories under `root` that are not active. `older_than: None` reaps every
/// orphan (boot), `Some(age)` only directories untouched for that long (hourly sweep).
async fn reap_orphans(
    root: &Path,
    active: &HashSet<PathBuf>,
    older_than: Option<Duration>,
) -> usize {
    let mut removed = 0;
    let Ok(mut entries) = fs::read_dir(root).await else {
        return 0;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(TEMP_PREFIX) {
            continue;
        }
        let path = entry.path();
        if active.contains(&path) {
            continue;
        }
        let Ok(file_type) = entry.file_type().await else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        if let Some(age) = older_than {
            let old = entry
                .metadata()
                .await
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|elapsed| elapsed >= age);
            if !old {
                continue;
            }
        }
        if fs::remove_dir_all(&path).await.is_ok() {
            removed += 1;
        }
    }
    removed
}

async fn boot_reap(state: &AppState) {
    let removed = reap_orphans(&state.config.tmp, &state.active_dirs(), None).await;
    if removed > 0 {
        info!(removed, "reaped orphan temp dirs at boot");
    }
}

fn spawn_reaper(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut ticker = interval(REAP_INTERVAL);
        ticker.tick().await; // the boot sweep already ran
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let removed = reap_orphans(
                        &state.config.tmp,
                        &state.active_dirs(),
                        Some(ORPHAN_AGE),
                    )
                    .await;
                    if removed > 0 {
                        info!(removed, "reaped orphan temp dirs");
                    }
                }
                _ = state.shutdown.cancelled() => break,
            }
        }
    });
}

async fn shutdown_signal() {
    let ctrl_c = async {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {}
            Err(error) => {
                warn!(%error, "cannot listen for ctrl-c");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                warn!(%error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not found")
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
}

fn app_with_state(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/healthz", get(healthz))
        .route("/download", get(download_get).post(download_post))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &Request<Body>| {
                    let request_id = request
                        .headers()
                        .get(REQUEST_ID_HEADER)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("-");
                    info_span!(
                        "http",
                        method = %request.method(),
                        path = %request.uri().path(),
                        request_id = %request_id
                    )
                })
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .with_state(state)
}

#[cfg(test)]
fn app(config: Arc<Config>) -> Router {
    app_with_state(Arc::new(AppState::new((*config).clone())))
}
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ixian=info,tower_http=info".into()),
        )
        .init();

    let state = Arc::new(AppState::new(Config::from_env()));
    let listener = TcpListener::bind((state.config.host.as_str(), state.config.port))
        .await
        .unwrap_or_else(|error| {
            panic!(
                "cannot listen on {}:{}: {error}",
                state.config.host, state.config.port
            )
        });
    boot_reap(&state).await;
    spawn_reaper(state.clone());
    info!(
        address = %format!("{}:{}", state.config.host, state.config.port),
        version = env!("CARGO_PKG_VERSION"),
        "ixian listening"
    );

    let draining = Arc::new(tokio::sync::Notify::new());
    let shutdown = {
        let state = state.clone();
        let draining = draining.clone();
        async move {
            shutdown_signal().await;
            state.begin_shutdown();
            draining.notify_one();
        }
    };
    let server =
        axum::serve(listener, app_with_state(state.clone())).with_graceful_shutdown(shutdown);
    let grace = state.config.shutdown_grace;
    tokio::select! {
        result = server => {
            if let Err(error) = result {
                panic!("server error: {error}");
            }
            info!("server stopped");
        }
        _ = async {
            draining.notified().await;
            tokio::time::sleep(grace).await;
        } => {
            warn!(grace_ms = grace.as_millis(), "shutdown grace period expired");
        }
    }
    state.cleanup_temp_dirs();
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
    fn picks_first_output_filepath() {
        assert_eq!(
            primary_filepath("/tmp/a.mp4\n/tmp/b.mp4\n"),
            Some(PathBuf::from("/tmp/a.mp4"))
        );
        assert_eq!(
            primary_filepath("\n  /tmp/a.mp4  \n"),
            Some(PathBuf::from("/tmp/a.mp4"))
        );
        assert_eq!(primary_filepath(" \n\n"), None);
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

    #[test]
    fn parses_embed_options() {
        assert_eq!(EmbedOptions::parse("").unwrap(), EmbedOptions::default());
        assert_eq!(
            EmbedOptions::parse("subs, thumbnail ,metadata").unwrap(),
            EmbedOptions {
                subs: true,
                thumbnail: true,
                metadata: true,
            }
        );
        assert_eq!(
            EmbedOptions::parse("metadata").unwrap(),
            EmbedOptions {
                metadata: true,
                ..EmbedOptions::default()
            }
        );
        let error = EmbedOptions::parse("chapters").unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(error.message.contains("unknown embed"));

        let json = json!(["subs", "metadata"]);
        assert_eq!(
            EmbedOptions::from_json(Some(&json)).unwrap(),
            EmbedOptions {
                subs: true,
                metadata: true,
                ..EmbedOptions::default()
            }
        );
        assert!(EmbedOptions::from_json(Some(&json!(true))).is_err());
    }

    #[test]
    fn parses_sponsorblock_and_compatibility() {
        assert_eq!(SponsorBlock::parse("").unwrap(), SponsorBlock::Off);
        assert_eq!(SponsorBlock::parse("off").unwrap(), SponsorBlock::Off);
        assert_eq!(SponsorBlock::parse("remove").unwrap(), SponsorBlock::Remove);
        assert_eq!(SponsorBlock::parse("mark").unwrap(), SponsorBlock::Mark);
        let error = SponsorBlock::parse("delete").unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(error.message.contains("unknown sponsorblock"));

        assert_eq!(
            Compatibility::parse("quicktime").unwrap(),
            Compatibility::Quicktime
        );
        assert_eq!(Compatibility::parse("none").unwrap(), Compatibility::Off);
        let error = Compatibility::parse("windows").unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(error.message.contains("unknown compatibility"));

        assert_eq!(
            SponsorBlock::from_json(Some(&json!("mark"))).unwrap(),
            SponsorBlock::Mark
        );
        assert_eq!(
            Compatibility::from_json(Some(&json!("quicktime"))).unwrap(),
            Compatibility::Quicktime
        );
        assert!(Compatibility::from_json(Some(&json!(1))).is_err());
    }

    #[test]
    fn names_quicktime_outputs() {
        assert_eq!(
            quicktime_output(Path::new("/tmp/clip [id].webm")),
            PathBuf::from("/tmp/clip [id].mp4")
        );
        assert_eq!(
            quicktime_output(Path::new("/tmp/clip [id].mp4")),
            PathBuf::from("/tmp/clip [id] [quicktime].mp4")
        );
    }

    #[tokio::test]
    async fn reaps_orphan_temp_dirs_only() {
        let root = tempfile::TempDir::new().unwrap();
        let orphan = root.path().join("ixian-orphan");
        let active = root.path().join("ixian-active");
        let other = root.path().join("downloads");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::create_dir_all(&active).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(orphan.join("leftover.part"), b"x").unwrap();
        std::fs::write(root.path().join("ixian-file"), b"x").unwrap();

        let active: HashSet<PathBuf> = HashSet::from([active.clone()]);
        assert_eq!(reap_orphans(root.path(), &active, None).await, 1);
        assert!(!orphan.exists());
        assert!(active.iter().all(|path| path.exists()));
        assert!(other.exists());
        assert!(root.path().join("ixian-file").exists());
    }

    #[tokio::test]
    async fn hourly_reap_spares_fresh_temp_dirs() {
        let root = tempfile::TempDir::new().unwrap();
        let old = root.path().join("ixian-old");
        let fresh = root.path().join("ixian-fresh");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&fresh).unwrap();
        let two_hours_ago = std::time::SystemTime::now() - Duration::from_secs(2 * 60 * 60);
        std::fs::File::open(&old)
            .unwrap()
            .set_modified(two_hours_ago)
            .unwrap();

        let removed = reap_orphans(root.path(), &HashSet::new(), Some(ORPHAN_AGE)).await;
        assert_eq!(removed, 1);
        assert!(!old.exists());
        assert!(fresh.exists());
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
if [ "$1" = "--version" ]; then
  printf 'v2026.10.26\n'
  exit 0
fi
LOG="{log_dir}"
out=""
prev=""
jar=""
for arg in "$@"; do
  case "$prev" in
    -o) out="$arg" ;;
    --cookies) jar="$arg" ;;
  esac
  prev="$arg"
done
printf '%s\n' "$@" > "$LOG/args.txt"
if [ -n "$jar" ]; then cp -p "$jar" "$LOG/jar.txt"; fi
{extra}
prefix="${{out%%\%*}}"
printf 'partial' > "${{prefix}}partial.part"
printf 'fake video data' > "${{prefix}}fake.mp4"
printf '%s\n' "${{prefix}}fake.mp4"
"#
                ),
            )
            .unwrap();
            stdfs::set_permissions(&script, stdfs::Permissions::from_mode(0o755)).unwrap();
            Self { tmp, logs, script }
        }

        fn config(&self, timeout: Duration) -> Arc<Config> {
            self.config_with_plugin_dir(timeout, None)
        }

        fn config_with_plugin_dir(
            &self,
            timeout: Duration,
            plugin_dir: Option<PathBuf>,
        ) -> Arc<Config> {
            Arc::new(Config {
                host: "127.0.0.1".into(),
                port: 0,
                ytdlp: self.script.to_string_lossy().into_owned(),
                plugin_dir,
                format: DEFAULT_FORMAT.into(),
                timeout,
                tmp: self.tmp.path().to_path_buf(),
                ffmpeg: "ffmpeg".into(),
                ffprobe: "ffprobe".into(),
                cache_dir: None,
                shutdown_grace: Duration::from_secs(1),
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

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        let error: Value = serde_json::from_slice(&body).unwrap();
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

        let (status, _, body) = request(app, Method::PUT, "/download", None).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"], "method not allowed");
    }

    #[tokio::test]
    async fn reports_health_with_ytdlp_version() {
        let harness = Harness::new("");
        let app = app(harness.config(Duration::from_secs(30)));

        let (status, headers, body) = request(app, Method::GET, "/healthz", None).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers[header::CONTENT_TYPE].to_str().unwrap(),
            "application/json"
        );
        assert!(headers.contains_key(REQUEST_ID_HEADER));
        let health: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(health["status"], "ok");
        assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(health["ytdlp_version"], "v2026.10.26");
        assert_eq!(health["active_jobs"], 0);
        assert!(health["cache_bytes"].is_null());
        assert!(health["disk_free_bytes"].is_u64() || health["disk_free_bytes"].is_null());
        assert!(health["uptime_seconds"].is_u64());
    }

    #[tokio::test]
    async fn passes_embed_and_sponsorblock_options_to_ytdlp() {
        let harness = Harness::new("");
        let app = app(harness.config(Duration::from_secs(30)));
        let payload = serde_json::json!({
            "url": "https://youtu.be/abc",
            "embed": ["subs", "thumbnail", "metadata"],
            "sponsorblock": "remove",
        })
        .to_string();

        let (status, _, _) = request(app, Method::POST, "/download", Some(payload)).await;

        assert_eq!(status, StatusCode::OK);
        let args = stdfs::read_to_string(harness.logs.path().join("args.txt")).unwrap();
        let lines: Vec<&str> = args.lines().collect();
        for flag in ["--embed-subs", "--embed-thumbnail", "--embed-metadata"] {
            assert!(lines.contains(&flag), "{args}");
        }
        let index = lines
            .iter()
            .position(|arg| *arg == "--sponsorblock-remove")
            .unwrap();
        assert_eq!(lines[index + 1], "all");
        assert!(!lines.contains(&"--sponsorblock-mark"));
    }
}
