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
    env,
    io::{self, Write},
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
    sync::{OwnedSemaphorePermit, Semaphore},
    time::timeout,
};
use tokio_util::io::ReaderStream;
use url::Url;
use uuid::Uuid;

const DEFAULT_FORMAT: &str = "bv*[vcodec^=avc1]+ba[acodec^=mp4a]/b[ext=mp4]/bv*+ba/b";
const COOKIE_HINT: &str =
    "cookies may be invalid or incomplete; public posts download without cookies";
const GET_COOKIE_HINT: &str = "send cookies in a POST /download JSON body instead";
const GET_COOKIE_PARAMS: [&str; 3] = ["youtube_cookies", "x_cookies", "instagram_cookies"];
const STREAM_CHUNK: usize = 64 * 1024;
const MAX_CONCURRENT: usize = 4;
const MAX_TIMEOUT_MS: u64 = 10 * 60 * 1000;
const MAX_COOKIE_BYTES: usize = 256 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_FILESIZE: u64 = 2 * 1024 * 1024 * 1024;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const SOCKET_TIMEOUT_SECS: u64 = 10;
const RETRIES: u64 = 3;
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const KILL_TIMEOUT: Duration = Duration::from_secs(2);
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

struct Config {
    host: String,
    port: u16,
    ytdlp: String,
    plugin_dir: Option<PathBuf>,
    format: String,
    timeout: Duration,
    tmp: PathBuf,
    downloads: Arc<Semaphore>,
    max_filesize: u64,
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
            .unwrap_or_else(|| Duration::from_millis(MAX_TIMEOUT_MS))
            .min(Duration::from_millis(MAX_TIMEOUT_MS));
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
            tmp,
            downloads: Arc::new(Semaphore::new(MAX_CONCURRENT)),
            max_filesize: MAX_FILESIZE,
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

    fn bad_request_with_hint(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            hint: Some(hint.into()),
        }
    }

    fn too_large(message: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, message)
    }

    fn too_many_requests(message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, message)
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
        let mut body = Map::new();
        body.insert("error".into(), Value::String(self.message));
        if let Some(hint) = self.hint {
            body.insert("hint".into(), Value::String(hint));
        }
        (self.status, Json(Value::Object(body))).into_response()
    }
}

type ApiResult = Result<Response, ApiError>;

/// Parses and validates a request URL, returning the cleaned URL (stray
/// backslashes removed) and the platform it belongs to. A `platform` override
/// still has to match the URL host: it never widens the allowlist.
fn validate_url(raw_url: &str, requested: Option<&str>) -> Result<(String, Platform), ApiError> {
    let platform = match requested.filter(|name| !name.is_empty()) {
        Some(requested) => Platform::from_name(requested).ok_or_else(|| {
            ApiError::bad_request(format!(
                "unknown platform '{requested}' (expected: youtube, x, instagram)"
            ))
        })?,
        None => detect_platform(raw_url).ok_or_else(|| {
            ApiError::bad_request(
                "cannot detect platform (supported: youtube, x, instagram); pass 'platform' to override",
            )
        })?,
    };

    let cleaned = raw_url.replace('\\', "");
    let url = Url::parse(&cleaned)
        .map_err(|_| ApiError::bad_request("invalid url (expected an absolute http(s) URL)"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ApiError::bad_request("url must use http or https"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| ApiError::bad_request("url has no host"))?;
    if !host_allowed(platform, host) {
        return Err(ApiError::bad_request(format!(
            "url host '{host}' is not allowed for platform '{}'",
            platform.name()
        )));
    }

    Ok((cleaned, platform))
}

fn detect_platform(raw_url: &str) -> Option<Platform> {
    let url = Url::parse(&raw_url.replace('\\', "")).ok()?;
    let host = url.host_str()?;
    Platform::ALL
        .into_iter()
        .find(|platform| host_allowed(*platform, host))
}

fn host_allowed(platform: Platform, host: &str) -> bool {
    let host = host.strip_prefix("www.").unwrap_or(host);
    platform
        .hosts()
        .iter()
        .any(|candidate| host == *candidate || host.ends_with(&format!(".{candidate}")))
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

/// `timeout_ms` is canonical; `timeoutMs` is accepted as an alias.
fn checked_timeout(duration: Duration) -> Result<Duration, ApiError> {
    if duration.as_millis() > u128::from(MAX_TIMEOUT_MS) {
        return Err(ApiError::bad_request(format!(
            "'timeout_ms' must be at most {MAX_TIMEOUT_MS} milliseconds"
        )));
    }
    Ok(duration)
}

fn query_timeout(params: &HashMap<String, String>) -> Result<Option<Duration>, ApiError> {
    let Some(value) = params.get("timeout_ms").or_else(|| params.get("timeoutMs")) else {
        return Ok(None);
    };
    let duration = parse_ms(value).ok_or_else(|| {
        ApiError::bad_request("'timeout_ms' must be a positive number of milliseconds")
    })?;
    checked_timeout(duration).map(Some)
}

fn json_timeout(object: &Map<String, Value>) -> Result<Option<Duration>, ApiError> {
    let Some(value) = object.get("timeout_ms").or_else(|| object.get("timeoutMs")) else {
        return Ok(None);
    };
    let duration = parse_ms_value(Some(value)).ok_or_else(|| {
        ApiError::bad_request("'timeout_ms' must be a positive number of milliseconds")
    })?;
    checked_timeout(duration).map(Some)
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

struct DownloadRequest {
    url: String,
    platform: Platform,
    cookies: Option<String>,
    format: Option<String>,
    timeout: Option<Duration>,
}

/// Creates the per-download directory with `0700` from the first syscall.
fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Creates the cookie jar with `0600` atomically (`create_new`), so it is never
/// readable by other users, not even for an instant.
fn write_cookie_jar(path: &Path, contents: &str) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())
}

/// Kills the whole process group led by `pid` (yt-dlp and its ffmpeg children).
#[cfg(unix)]
fn signal_process_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: a negative pid signals the process group led by `pid`.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn signal_process_group(_pid: u32) {}

/// Kills the child's process group if the download future is dropped (client
/// disconnect) or if it was not disarmed after a normal exit.
struct ProcessGroup {
    pid: u32,
    armed: bool,
}

impl ProcessGroup {
    fn new(pid: Option<u32>) -> Self {
        Self {
            pid: pid.unwrap_or(0),
            armed: pid.is_some_and(|pid| pid > 0),
        }
    }

    fn kill(&self) {
        if self.armed {
            signal_process_group(self.pid);
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Keeps lossy UTF-8 conversion from growing past the byte cap.
fn truncate_to_bytes(mut text: String, max_bytes: usize) -> String {
    if text.len() > max_bytes {
        let mut end = max_bytes;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// Waits for a pipe reader, but never longer than [`PIPE_DRAIN_TIMEOUT`] after
/// the child exited: a grandchild can keep the pipe open indefinitely.
async fn drain_pipe(handle: tokio::task::JoinHandle<io::Result<String>>) -> (Option<String>, bool) {
    let mut handle = handle;
    tokio::select! {
        joined = &mut handle => match joined {
            Ok(Ok(text)) => (Some(text), false),
            Ok(Err(_)) | Err(_) => (None, false),
        },
        _ = tokio::time::sleep(PIPE_DRAIN_TIMEOUT) => {
            handle.abort();
            (None, true)
        }
    }
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
    _permit: OwnedSemaphorePermit,
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

async fn download(config: &Config, request: DownloadRequest) -> ApiResult {
    let DownloadRequest {
        url,
        platform,
        cookies,
        format,
        timeout: request_timeout,
    } = request;
    let cookies = cookies.filter(|cookies| !cookies.is_empty());
    let has_cookies = cookies.is_some();
    let hint = has_cookies.then_some(COOKIE_HINT);
    if cookies
        .as_deref()
        .is_some_and(|cookies| cookies.len() > MAX_COOKIE_BYTES)
    {
        return Err(ApiError::too_large(format!(
            "cookies exceed the {MAX_COOKIE_BYTES} byte limit"
        )));
    }
    let permit = config.downloads.clone().try_acquire_owned().map_err(|_| {
        ApiError::too_many_requests(format!(
            "too many concurrent downloads (limit: {MAX_CONCURRENT})"
        ))
    })?;

    let dir = config.tmp.join(format!("ixian-{}", Uuid::new_v4()));
    create_private_dir(&dir)
        .map_err(|error| ApiError::internal(format!("cannot create temp directory: {error}")))?;
    let jar = dir.join("cookies.txt");
    let cleanup = Cleanup::new(dir.clone());
    if let Some(cookies) = cookies.as_deref() {
        let contents = cookie_file(cookies, platform);
        if contents.len() > MAX_COOKIE_BYTES {
            return Err(ApiError::too_large(format!(
                "cookies exceed the {MAX_COOKIE_BYTES} byte limit"
            )));
        }
        write_cookie_jar(&jar, &contents)
            .map_err(|error| ApiError::internal(format!("cannot write cookie jar: {error}")))?;
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
        .arg("--socket-timeout")
        .arg(SOCKET_TIMEOUT_SECS.to_string())
        .arg("--retries")
        .arg(RETRIES.to_string())
        .arg("--max-filesize")
        .arg(config.max_filesize.to_string())
        .arg("-f")
        .arg(format.as_deref().unwrap_or(&config.format))
        .arg("-o")
        .arg(dir.join("%(title)s [%(id)s].%(ext)s"));
    if let Some(plugin_dir) = &config.plugin_dir {
        cmd.arg("--plugin-dirs").arg(plugin_dir);
    }
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if has_cookies {
        cmd.arg("--cookies").arg(&jar);
    }
    // `--` terminates option parsing so a leading dash can never become a flag.
    cmd.arg("--").arg(&url);

    let mut child = cmd
        .spawn()
        .map_err(|error| ApiError::bad_gateway(format!("failed to run yt-dlp: {error}"), None))?;
    let mut group = ProcessGroup::new(child.id());

    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| ApiError::internal("cannot capture yt-dlp stdout"))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| ApiError::internal("cannot capture yt-dlp stderr"))?;
    let stdout_task = tokio::spawn(async move {
        let mut text = String::new();
        stdout_pipe.read_to_string(&mut text).await.map(|_| text)
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut limited = stderr_pipe.take(MAX_STDERR_BYTES as u64);
        limited.read_to_end(&mut buf).await.map(|_| {
            let text = String::from_utf8_lossy(&buf);
            truncate_to_bytes(text.trim_end().to_string(), MAX_STDERR_BYTES)
        })
    });

    let limit = request_timeout.unwrap_or(config.timeout);
    let status = match timeout(limit, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            group.kill();
            return Err(ApiError::bad_gateway(
                format!("failed to wait for yt-dlp: {error}"),
                hint,
            ));
        }
        Err(_) => {
            group.kill();
            if timeout(KILL_TIMEOUT, child.wait()).await.is_ok() {
                group.disarm();
            }
            // Do not wait on the pipes: yt-dlp's children (ffmpeg, curl) can keep them open.
            stdout_task.abort();
            stderr_task.abort();
            return Err(ApiError::bad_gateway(
                format!("yt-dlp timed out after {}ms", limit.as_millis()),
                hint,
            ));
        }
    };

    // yt-dlp exited, but a grandchild may still hold the pipes open.
    let ((stdout, stdout_stalled), (stderr, stderr_stalled)) =
        tokio::join!(drain_pipe(stdout_task), drain_pipe(stderr_task));
    if stdout_stalled || stderr_stalled {
        group.kill();
    }
    group.disarm();
    let stdout = stdout.unwrap_or_default();
    let stderr = stderr.unwrap_or_default();

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
    if metadata.len() > config.max_filesize {
        return Err(ApiError::bad_gateway(
            format!(
                "output file is {} bytes, over the {} byte limit",
                metadata.len(),
                config.max_filesize
            ),
            hint,
        ));
    }

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
        _permit: permit,
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

async fn download_get(State(config): State<Arc<Config>>, uri: Uri) -> ApiResult {
    let params: HashMap<String, String> =
        url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
            .into_owned()
            .collect();
    if let Some((name, _)) = params
        .iter()
        .find(|(name, _)| GET_COOKIE_PARAMS.contains(&name.as_str()))
    {
        return Err(ApiError::bad_request_with_hint(
            format!("'{name}' is not accepted in GET query strings"),
            GET_COOKIE_HINT,
        ));
    }

    let url = params
        .get("url")
        .map(String::as_str)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| ApiError::bad_request("missing 'url' query parameter"))?;
    let (url, platform) = validate_url(url, params.get("platform").map(String::as_str))?;
    let timeout = query_timeout(&params)?;

    download(
        &config,
        DownloadRequest {
            url,
            platform,
            cookies: None,
            format: params
                .get("format")
                .filter(|format| !format.is_empty())
                .cloned(),
            timeout,
        },
    )
    .await
}

async fn download_post(State(config): State<Arc<Config>>, body: String) -> ApiResult {
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
    let (url, platform) = validate_url(url, object.get("platform").and_then(Value::as_str))?;
    let timeout = json_timeout(object)?;

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
        &config,
        DownloadRequest {
            url,
            platform,
            cookies,
            format: object
                .get("format")
                .and_then(Value::as_str)
                .filter(|format| !format.is_empty())
                .map(str::to_string),
            timeout,
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
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
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
    fn platform_override_still_enforces_the_host_allowlist() {
        assert_eq!(
            validate_url("https://www.instagram.com/reel/abc/", Some("instagram"))
                .unwrap()
                .1,
            Platform::Instagram
        );
        assert_eq!(
            validate_url("http://127.0.0.1/video", Some("youtube"))
                .unwrap_err()
                .status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            validate_url("https://x.com/user/status/1", Some("youtube"))
                .unwrap_err()
                .status,
            StatusCode::BAD_REQUEST
        );
        assert!(validate_url("https://example.com/v", Some("tiktok")).is_err());
        assert_eq!(
            validate_url("https://example.com/v", Some(""))
                .unwrap_err()
                .status,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn rejects_non_http_urls_and_cleans_backslashes() {
        for url in [
            "file:///etc/passwd",
            "ftp://youtu.be/abc",
            "javascript:alert(1)",
            "--exec=rm -rf /",
        ] {
            assert_eq!(
                validate_url(url, Some("youtube")).unwrap_err().status,
                StatusCode::BAD_REQUEST,
                "{url}"
            );
        }
        assert_eq!(
            validate_url(r"https://www.youtube.com/watch\?v\=abc", None)
                .unwrap()
                .0,
            "https://www.youtube.com/watch?v=abc"
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
    fn creates_temp_dir_0700_and_cookie_jar_0600_atomically() {
        use std::os::unix::fs::PermissionsExt;

        let parent = tempfile::TempDir::new().unwrap();
        let dir = parent.path().join("ixian-test");
        create_private_dir(&dir).unwrap();
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);

        let jar = dir.join("cookies.txt");
        write_cookie_jar(&jar, "cookie data").unwrap();
        let jar_mode = std::fs::metadata(&jar).unwrap().permissions().mode() & 0o777;
        assert_eq!(jar_mode, 0o600);
        // `create_new` refuses to reuse an existing file.
        assert!(write_cookie_jar(&jar, "other").is_err());
        assert_eq!(std::fs::read_to_string(&jar).unwrap(), "cookie data");
    }

    #[test]
    fn caps_timeouts() {
        assert_eq!(
            checked_timeout(Duration::from_millis(MAX_TIMEOUT_MS)).unwrap(),
            Duration::from_millis(MAX_TIMEOUT_MS)
        );
        assert!(checked_timeout(Duration::from_millis(MAX_TIMEOUT_MS + 1)).is_err());

        let params = HashMap::from([("timeoutMs".to_string(), "1500".to_string())]);
        assert_eq!(
            query_timeout(&params).unwrap(),
            Some(Duration::from_millis(1500))
        );
        let params = HashMap::from([("timeout_ms".to_string(), "nope".to_string())]);
        assert!(query_timeout(&params).is_err());
    }

    #[test]
    fn truncates_stderr_on_a_char_boundary() {
        assert_eq!(truncate_to_bytes("abcdef".into(), 3), "abc");
        assert_eq!(truncate_to_bytes("é".into(), 1), "");
        assert_eq!(truncate_to_bytes("ok".into(), 10), "ok");
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
dir="${{prefix%/}}"
mode=$(stat -c %a "$dir" 2>/dev/null || stat -f %Lp "$dir")
printf '%s\n' "$mode" > "$LOG/dirmode.txt"
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
            self.config_with_limits(plugin_dir, timeout, MAX_CONCURRENT, MAX_FILESIZE)
        }

        fn config_with_limits(
            &self,
            plugin_dir: Option<PathBuf>,
            timeout: Duration,
            concurrency: usize,
            max_filesize: u64,
        ) -> Arc<Config> {
            Arc::new(Config {
                host: "127.0.0.1".into(),
                port: 0,
                ytdlp: self.script.to_string_lossy().into_owned(),
                plugin_dir,
                format: DEFAULT_FORMAT.into(),
                timeout,
                tmp: self.tmp.path().to_path_buf(),
                downloads: Arc::new(Semaphore::new(concurrency)),
                max_filesize,
            })
        }
    }

    fn query(params: &[(&str, &str)]) -> String {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in params {
            serializer.append_pair(name, value);
        }
        format!("/download?{}", serializer.finish())
    }

    fn process_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    async fn assert_process_dies(pid: u32) {
        for _ in 0..100 {
            if !process_alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("process {pid} survived");
    }

    async fn wait_for_file(path: &Path) {
        for _ in 0..100 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{} was not created", path.display());
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
        let dir_mode = stdfs::read_to_string(harness.logs.path().join("dirmode.txt")).unwrap();
        assert_eq!(dir_mode.trim(), "700");
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
    async fn passes_hardening_flags_and_double_dash() {
        let harness = Harness::new("");
        let app = app(harness.config(Duration::from_secs(30)));
        let payload =
            serde_json::json!({ "url": "https://www.youtube.com/watch?v=abc" }).to_string();

        let (status, _, _) = request(app, Method::POST, "/download", Some(payload)).await;

        assert_eq!(status, StatusCode::OK);
        let args = stdfs::read_to_string(harness.logs.path().join("args.txt")).unwrap();
        let lines: Vec<&str> = args.lines().collect();
        assert_eq!(lines[lines.len() - 2], "--");
        assert_eq!(
            lines[lines.len() - 1],
            "https://www.youtube.com/watch?v=abc"
        );
        let value_of = |flag: &str| {
            let index = lines.iter().position(|arg| *arg == flag).unwrap();
            lines[index + 1]
        };
        assert_eq!(
            value_of("--socket-timeout"),
            SOCKET_TIMEOUT_SECS.to_string()
        );
        assert_eq!(value_of("--retries"), RETRIES.to_string());
        assert_eq!(value_of("--max-filesize"), MAX_FILESIZE.to_string());
    }

    #[tokio::test]
    async fn rejects_cookies_on_get() {
        let harness = Harness::new("");
        let app = app(harness.config(Duration::from_secs(30)));
        let uri = query(&[
            ("url", "https://x.com/user/status/1"),
            ("x_cookies", "auth_token=secret"),
        ]);

        let (status, _, body) = request(app, Method::GET, &uri, None).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert!(error["hint"].as_str().unwrap().contains("POST"), "{error}");
        assert_eq!(stdfs::read_dir(harness.tmp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn rejects_urls_outside_the_platform_allowlist() {
        let harness = Harness::new("");
        let app = app(harness.config(Duration::from_secs(30)));

        let uri = query(&[("url", "http://127.0.0.1/video"), ("platform", "youtube")]);
        let (status, _, body) = request(app.clone(), Method::GET, &uri, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            error["error"].as_str().unwrap().contains("not allowed"),
            "{error}"
        );

        for (url, platform) in [
            ("https://x.com/user/status/1", "youtube"),
            ("file:///etc/passwd", "youtube"),
            ("ftp://youtu.be/abc", "youtube"),
            ("--exec=rm -rf /", "youtube"),
        ] {
            let payload = serde_json::json!({ "url": url, "platform": platform }).to_string();
            let (status, _, _) =
                request(app.clone(), Method::POST, "/download", Some(payload)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{url}");
        }
        assert_eq!(stdfs::read_dir(harness.tmp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn accepts_both_timeout_names() {
        let harness = Harness::new("sleep 5");
        let app = app(harness.config(Duration::from_secs(30)));

        for name in ["timeout_ms", "timeoutMs"] {
            let uri = query(&[("url", "https://youtu.be/abc"), (name, "100")]);
            let started = std::time::Instant::now();
            let (status, _, _) = request(app.clone(), Method::GET, &uri, None).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "GET {name}");
            assert!(started.elapsed() < Duration::from_secs(2));
        }

        for name in ["timeout_ms", "timeoutMs"] {
            let payload = if name == "timeout_ms" {
                serde_json::json!({ "url": "https://youtu.be/abc", "timeout_ms": 100 }).to_string()
            } else {
                serde_json::json!({ "url": "https://youtu.be/abc", "timeoutMs": 100 }).to_string()
            };
            let started = std::time::Instant::now();
            let (status, _, _) =
                request(app.clone(), Method::POST, "/download", Some(payload)).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "POST {name}");
            assert!(started.elapsed() < Duration::from_secs(2));
        }
    }

    #[tokio::test]
    async fn rejects_invalid_and_oversized_timeouts() {
        let harness = Harness::new("");
        let app = app(harness.config(Duration::from_secs(30)));

        for payload in [
            serde_json::json!({ "url": "https://youtu.be/abc", "timeout_ms": "nope" }),
            serde_json::json!({ "url": "https://youtu.be/abc", "timeoutMs": 0 }),
            serde_json::json!({ "url": "https://youtu.be/abc", "timeout_ms": MAX_TIMEOUT_MS + 1 }),
        ] {
            let (status, _, _) = request(
                app.clone(),
                Method::POST,
                "/download",
                Some(payload.to_string()),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{payload}");
        }
        assert_eq!(stdfs::read_dir(harness.tmp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn rejects_oversized_cookies() {
        let harness = Harness::new("");
        let app = app(harness.config(Duration::from_secs(30)));
        let payload = serde_json::json!({
            "url": "https://x.com/user/status/1",
            "cookies": { "x": "a".repeat(MAX_COOKIE_BYTES + 1) },
        })
        .to_string();

        let (status, _, body) = request(app, Method::POST, "/download", Some(payload)).await;

        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            error["error"].as_str().unwrap().contains("cookie"),
            "{error}"
        );
        assert_eq!(stdfs::read_dir(harness.tmp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn bounds_stderr_capture() {
        let harness = Harness::new("yes x | head -c 100000 >&2; exit 1");
        let app = app(harness.config(Duration::from_secs(30)));
        let payload = serde_json::json!({ "url": "https://youtu.be/abc" }).to_string();

        let (status, _, body) = request(app, Method::POST, "/download", Some(payload)).await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let error: Value = serde_json::from_slice(&body).unwrap();
        let message = error["error"].as_str().unwrap();
        assert!(!message.is_empty());
        assert!(
            message.len() <= MAX_STDERR_BYTES,
            "stderr was {} bytes",
            message.len()
        );
    }

    #[tokio::test]
    async fn rejects_output_over_max_filesize() {
        let harness = Harness::new("");
        let app = app(harness.config_with_limits(None, Duration::from_secs(30), MAX_CONCURRENT, 4));
        let payload = serde_json::json!({ "url": "https://youtu.be/abc" }).to_string();

        let (status, _, body) = request(app, Method::POST, "/download", Some(payload)).await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            error["error"].as_str().unwrap().contains("limit"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn returns_429_when_concurrency_is_exhausted() {
        let harness = Harness::new("sleep 30");
        let app = app(harness.config_with_limits(None, Duration::from_secs(60), 1, MAX_FILESIZE));
        let payload = serde_json::json!({ "url": "https://youtu.be/abc" }).to_string();
        let first = tokio::spawn(request(
            app.clone(),
            Method::POST,
            "/download",
            Some(payload.clone()),
        ));
        wait_for_file(&harness.logs.path().join("args.txt")).await;

        let (status, _, body) = request(app, Method::POST, "/download", Some(payload)).await;

        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            error["error"].as_str().unwrap().contains("concurrent"),
            "{error}"
        );
        first.abort();
        let _ = first.await;
    }

    #[tokio::test]
    async fn kills_process_group_on_timeout() {
        let harness = Harness::new("sleep 30 & echo $! > \"$LOG/grandchild.pid\"; sleep 30");
        let app = app(harness.config(Duration::from_secs(30)));
        let payload =
            serde_json::json!({ "url": "https://youtu.be/abc", "timeout_ms": 1000 }).to_string();

        let started = std::time::Instant::now();
        let (status, _, body) = request(app, Method::POST, "/download", Some(payload)).await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(started.elapsed() < Duration::from_secs(3));
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            error["error"].as_str().unwrap().contains("timed out"),
            "{error}"
        );
        let pid = stdfs::read_to_string(harness.logs.path().join("grandchild.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_process_dies(pid).await;
    }

    #[tokio::test]
    async fn kills_process_group_when_request_is_cancelled() {
        let harness = Harness::new("sleep 30 & echo $! > \"$LOG/grandchild.pid\"; sleep 30");
        let app = app(harness.config(Duration::from_secs(60)));
        let payload = serde_json::json!({ "url": "https://youtu.be/abc" }).to_string();
        let task = tokio::spawn(request(app, Method::POST, "/download", Some(payload)));
        wait_for_file(&harness.logs.path().join("grandchild.pid")).await;

        task.abort();
        let _ = task.await;

        let pid = stdfs::read_to_string(harness.logs.path().join("grandchild.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_process_dies(pid).await;
    }

    #[tokio::test]
    async fn bounds_post_exit_pipe_drain() {
        let harness = Harness::new("sleep 30 1>/dev/null & echo $! > \"$LOG/grandchild.pid\"");
        let app = app(harness.config(Duration::from_secs(30)));
        let payload = serde_json::json!({ "url": "https://youtu.be/abc" }).to_string();

        let started = std::time::Instant::now();
        let (status, _, body) = request(app, Method::POST, "/download", Some(payload)).await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(body, b"fake video data");
        let pid = stdfs::read_to_string(harness.logs.path().join("grandchild.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_process_dies(pid).await;
    }
}
