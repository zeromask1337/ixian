// ixian - minimal yt-dlp wrapper (youtube / x.com / instagram / tiktok) in Rust.
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path as UrlPath, State},
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, put},
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
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt, ReadBuf},
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
const COOKIE_DIR_MODE: u32 = 0o700;
const COOKIE_FILE_MODE: u32 = 0o600;
const MAX_STORED_COOKIE_BYTES: usize = 1024 * 1024;
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
    TikTok,
}

impl Platform {
    const ALL: [Platform; 4] = [
        Platform::YouTube,
        Platform::X,
        Platform::Instagram,
        Platform::TikTok,
    ];

    fn name(self) -> &'static str {
        match self {
            Platform::YouTube => "youtube",
            Platform::X => "x",
            Platform::Instagram => "instagram",
            Platform::TikTok => "tiktok",
        }
    }

    fn hosts(self) -> &'static [&'static str] {
        match self {
            Platform::YouTube => &["youtube.com", "youtu.be"],
            Platform::X => &["x.com", "twitter.com"],
            Platform::Instagram => &["instagram.com"],
            Platform::TikTok => &["tiktok.com", "vm.tiktok.com", "vt.tiktok.com"],
        }
    }

    fn domains(self) -> &'static [&'static str] {
        match self {
            Platform::YouTube => &[".youtube.com"],
            Platform::X => &[".x.com", ".twitter.com"],
            Platform::Instagram => &[".instagram.com"],
            Platform::TikTok => &[".tiktok.com"],
        }
    }

    fn from_name(name: &str) -> Option<Platform> {
        Platform::ALL.into_iter().find(|p| p.name() == name)
    }
}

fn supported_names() -> String {
    Platform::ALL
        .iter()
        .map(|platform| platform.name())
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_platform(name: &str) -> Result<Platform, ApiError> {
    Platform::from_name(name).ok_or_else(|| {
        ApiError::bad_request(format!(
            "unknown platform '{name}' (expected: {})",
            supported_names()
        ))
    })
}

struct Config {
    host: String,
    port: u16,
    ytdlp: String,
    plugin_dir: Option<PathBuf>,
    format: String,
    timeout: Duration,
    tmp: PathBuf,
    cookie_dir: Option<PathBuf>,
    token: Option<String>,
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
        let cookie_dir = non_empty(env::var("IXIAN_COOKIE_DIR").ok()).map(PathBuf::from);
        let token = env::var("IXIAN_TOKEN")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        Self {
            host,
            port,
            ytdlp,
            plugin_dir,
            format,
            timeout: timeout_ms,
            tmp,
            cookie_dir,
            token,
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
        return parse_platform(requested);
    }

    detect_platform(url).ok_or_else(|| {
        ApiError::bad_request(format!(
            "cannot detect platform (supported: {}); pass 'platform' to override",
            supported_names()
        ))
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
    if is_netscape_cookie_file(cookies) {
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

fn is_netscape_cookie_file(cookies: &str) -> bool {
    cookies.contains('\t')
}

fn strip_prefix_ignore_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let head = value.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &value[prefix.len()..])
}

fn cookie_path(dir: &Path, platform: Platform) -> PathBuf {
    dir.join(format!("{}.txt", platform.name()))
}

/// Normalizes a `PUT /cookies/{platform}` body (raw `Cookie:` header or Netscape file).
fn stored_cookie_contents(input: &str, platform: Platform) -> Result<String, ApiError> {
    if input.len() > MAX_STORED_COOKIE_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("cookie file too large (max {MAX_STORED_COOKIE_BYTES} bytes)"),
        ));
    }
    if input.trim().is_empty() {
        return Err(ApiError::bad_request("empty cookie body"));
    }

    let input = input.trim_end_matches(['\r', '\n']);
    if is_netscape_cookie_file(input) {
        let has_entry = input.lines().any(|line| {
            let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
            !line.trim_start().starts_with('#') && line.split('\t').count() >= 7
        });
        if !has_entry {
            return Err(ApiError::bad_request(
                "cookie file has no entries (expected a Netscape cookie file)",
            ));
        }
        return Ok(format!("{input}\n"));
    }

    let file = cookie_file(input, platform);
    if file.lines().count() <= 1 {
        return Err(ApiError::bad_request(
            "no cookies found (send a Cookie header or a Netscape cookie file)",
        ));
    }
    Ok(file)
}

async fn stored_cookies(config: &Config, platform: Platform) -> Result<Option<String>, ApiError> {
    let Some(dir) = &config.cookie_dir else {
        return Ok(None);
    };
    match fs::read_to_string(cookie_path(dir, platform)).await {
        Ok(contents) if !contents.trim().is_empty() => Ok(Some(contents)),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ApiError::internal(format!(
            "cannot read stored cookies for {}: {error}",
            platform.name()
        ))),
    }
}

async fn ensure_cookie_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, std::fs::Permissions::from_mode(COOKIE_DIR_MODE)).await?;
    }
    Ok(())
}

async fn write_cookie_file(path: &Path, contents: &str) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "cookie path has no parent directory",
        )
    })?;
    ensure_cookie_dir(dir).await?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("cookies");
    let tmp = dir.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    let result = write_cookie_tmp(&tmp, path, contents).await;
    if result.is_err() {
        let _ = fs::remove_file(&tmp).await;
    }
    result
}

async fn write_cookie_tmp(tmp: &Path, path: &Path, contents: &str) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(COOKIE_FILE_MODE);
    let mut file = options.open(tmp).await?;
    file.write_all(contents.as_bytes()).await?;
    file.sync_all().await?;
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(tmp, std::fs::Permissions::from_mode(COOKIE_FILE_MODE)).await?;
    }
    fs::rename(tmp, path).await
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn authorize(config: &Config, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(token) = config.token.as_deref() else {
        return Ok(());
    };
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| strip_prefix_ignore_case(value.trim(), "bearer "))
        .map(str::trim);
    let authorized =
        provided.is_some_and(|provided| constant_time_eq(provided.as_bytes(), token.as_bytes()));
    if authorized {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "missing or invalid bearer token (IXIAN_TOKEN)",
        ))
    }
}

fn system_time_secs(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
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

struct DownloadRequest {
    url: String,
    platform: Platform,
    cookies: Option<String>,
    format: Option<String>,
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

async fn download(config: &Config, request: DownloadRequest) -> ApiResult {
    let DownloadRequest {
        url,
        platform,
        cookies,
        format,
        timeout: request_timeout,
    } = request;
    let url = url.replace('\\', "");
    let cookies = match cookies.filter(|cookies| !cookies.trim().is_empty()) {
        Some(cookies) => Some(cookies),
        None => stored_cookies(config, platform).await?,
    };
    let has_cookies = cookies
        .as_deref()
        .is_some_and(|cookies| !cookies.trim().is_empty());
    let hint = has_cookies.then_some(COOKIE_HINT);

    let dir = config.tmp.join(format!("ixian-{}", Uuid::new_v4()));
    fs::create_dir(&dir)
        .await
        .map_err(|error| ApiError::internal(format!("cannot create temp directory: {error}")))?;
    let jar = dir.join("cookies.txt");
    let cleanup = Cleanup::new(dir.clone());
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
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if has_cookies {
        cmd.arg("--cookies").arg(&jar);
    }
    cmd.arg(&url);

    let mut child = cmd
        .spawn()
        .map_err(|error| ApiError::bad_gateway(format!("failed to run yt-dlp: {error}"), None))?;

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
    let status = match timeout(limit, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            return Err(ApiError::bad_gateway(
                format!("failed to wait for yt-dlp: {error}"),
                hint,
            ));
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            // Do not wait on the pipes: yt-dlp's children (ffmpeg, curl) can keep them open.
            stdout_task.abort();
            stderr_task.abort();
            return Err(ApiError::bad_gateway(
                format!("yt-dlp timed out after {}ms", limit.as_millis()),
                hint,
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

async fn download_get(State(config): State<Arc<Config>>, uri: Uri) -> ApiResult {
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

    download(
        &config,
        DownloadRequest {
            url: url.to_string(),
            platform,
            cookies: params.get(&format!("{}_cookies", platform.name())).cloned(),
            format: params
                .get("format")
                .filter(|format| !format.is_empty())
                .cloned(),
            timeout: params.get("timeout_ms").and_then(|value| parse_ms(value)),
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
        &config,
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
        },
    )
    .await
}

fn cookie_store_dir(config: &Config) -> Result<&Path, ApiError> {
    config
        .cookie_dir
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("cookie store disabled; set IXIAN_COOKIE_DIR"))
}

async fn list_cookies(State(config): State<Arc<Config>>) -> Json<Value> {
    let mut cookies = Vec::with_capacity(Platform::ALL.len());
    for platform in Platform::ALL {
        let metadata = match &config.cookie_dir {
            Some(dir) => fs::metadata(cookie_path(dir, platform))
                .await
                .ok()
                .filter(|metadata| metadata.is_file()),
            None => None,
        };
        let mut entry = json!({
            "platform": platform.name(),
            "present": metadata.is_some(),
        });
        let mtime = metadata
            .as_ref()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(system_time_secs);
        if let Some(mtime) = mtime {
            entry["mtime"] = json!(mtime);
        }
        cookies.push(entry);
    }

    Json(json!({
        "enabled": config.cookie_dir.is_some(),
        "cookies": cookies,
    }))
}

async fn put_cookie(
    State(config): State<Arc<Config>>,
    UrlPath(platform): UrlPath<String>,
    headers: HeaderMap,
    body: String,
) -> ApiResult {
    authorize(&config, &headers)?;
    let platform = parse_platform(&platform)?;
    let dir = cookie_store_dir(&config)?;
    let contents = stored_cookie_contents(&body, platform)?;
    let path = cookie_path(dir, platform);
    write_cookie_file(&path, &contents)
        .await
        .map_err(|error| ApiError::internal(format!("cannot store cookies: {error}")))?;
    let mtime = fs::metadata(&path)
        .await
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(system_time_secs);

    Ok(Json(json!({
        "platform": platform.name(),
        "stored": true,
        "mtime": mtime,
    }))
    .into_response())
}

async fn delete_cookie(
    State(config): State<Arc<Config>>,
    UrlPath(platform): UrlPath<String>,
    headers: HeaderMap,
) -> ApiResult {
    authorize(&config, &headers)?;
    let platform = parse_platform(&platform)?;
    let dir = cookie_store_dir(&config)?;
    let deleted = match fs::remove_file(cookie_path(dir, platform)).await {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(ApiError::internal(format!(
                "cannot delete stored cookies: {error}"
            )));
        }
    };

    Ok(Json(json!({
        "platform": platform.name(),
        "deleted": deleted,
    }))
    .into_response())
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
        .route("/cookies", get(list_cookies))
        .route("/cookies/{platform}", put(put_cookie).delete(delete_cookie))
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
        for url in [
            "https://www.tiktok.com/@user/video/123",
            "https://tiktok.com/@user/video/123",
            "https://vm.tiktok.com/ZMabc/",
            "https://vt.tiktok.com/ZMabc/",
        ] {
            assert_eq!(detect_platform(url), Some(Platform::TikTok), "{url}");
        }
        assert_eq!(detect_platform("https://example.com/video"), None);
        assert_eq!(detect_platform("not a url"), None);
    }

    #[test]
    fn platform_override_wins_and_validates() {
        assert_eq!(
            resolve_platform("https://example.com/v", Some("instagram")).unwrap(),
            Platform::Instagram
        );
        assert_eq!(
            resolve_platform("https://example.com/v", Some("tiktok")).unwrap(),
            Platform::TikTok
        );
        assert!(resolve_platform("https://example.com/v", Some("vimeo")).is_err());
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
    fn builds_tiktok_cookie_file_from_header() {
        let file = cookie_file(
            "Cookie: sessionid=abc; tt_chain_token=d=ef",
            Platform::TikTok,
        );
        assert!(file.contains(".tiktok.com\tTRUE\t/\tTRUE\t2147483647\tsessionid\tabc"));
        assert!(file.contains(".tiktok.com\tTRUE\t/\tTRUE\t2147483647\ttt_chain_token\td=ef"));
        assert!(!file.contains(".instagram.com"));
    }

    #[test]
    fn normalizes_cookie_store_inputs() {
        let file = stored_cookie_contents("Cookie: sessionid=abc", Platform::TikTok).unwrap();
        assert!(file.contains(".tiktok.com\tTRUE\t/\tTRUE\t2147483647\tsessionid\tabc"));

        let netscape = "# Netscape HTTP Cookie File\n\
                        #HttpOnly_.tiktok.com\tTRUE\t/\tTRUE\t0\tsessionid\tx\n";
        assert_eq!(
            stored_cookie_contents(netscape, Platform::TikTok).unwrap(),
            netscape
        );
        assert_eq!(
            stored_cookie_contents(netscape.trim_end(), Platform::TikTok).unwrap(),
            netscape
        );
        assert!(stored_cookie_contents("  \n", Platform::TikTok).is_err());
        assert!(stored_cookie_contents("# Netscape HTTP Cookie File\n", Platform::TikTok).is_err());
        assert!(
            stored_cookie_contents(&"a".repeat(MAX_STORED_COOKIE_BYTES + 1), Platform::TikTok)
                .is_err()
        );
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
            self.config_full(timeout, None, None, None)
        }

        fn config_with_plugin_dir(
            &self,
            timeout: Duration,
            plugin_dir: Option<PathBuf>,
        ) -> Arc<Config> {
            self.config_full(timeout, plugin_dir, None, None)
        }

        fn config_with_cookie_store(
            &self,
            timeout: Duration,
            cookie_dir: PathBuf,
            token: Option<&str>,
        ) -> Arc<Config> {
            self.config_full(timeout, None, Some(cookie_dir), token.map(str::to_string))
        }

        fn config_full(
            &self,
            timeout: Duration,
            plugin_dir: Option<PathBuf>,
            cookie_dir: Option<PathBuf>,
            token: Option<String>,
        ) -> Arc<Config> {
            Arc::new(Config {
                host: "127.0.0.1".into(),
                port: 0,
                ytdlp: self.script.to_string_lossy().into_owned(),
                plugin_dir,
                format: DEFAULT_FORMAT.into(),
                timeout,
                tmp: self.tmp.path().to_path_buf(),
                cookie_dir,
                token,
            })
        }
    }

    async fn request(
        app: Router,
        method: Method,
        uri: &str,
        body: Option<String>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        request_with_headers(app, method, uri, body, HeaderMap::new()).await
    }

    async fn request_with_headers(
        app: Router,
        method: Method,
        uri: &str,
        body: Option<String>,
        headers: HeaderMap,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut builder = Request::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        for (name, value) in &headers {
            builder = builder.header(name, value);
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

        let payload = r#"{"url":"https://example.com/video","platform":"vimeo"}"#.to_string();
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
    async fn downloads_tiktok_with_default_format() {
        let harness = Harness::new("");
        let app = app(harness.config(Duration::from_secs(30)));
        let payload = serde_json::json!({ "url": "https://vm.tiktok.com/ZMabc/" }).to_string();

        let (status, headers, _) = request(app, Method::POST, "/download", Some(payload)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["x-ixian-platform"].to_str().unwrap(), "tiktok");
        let args = stdfs::read_to_string(harness.logs.path().join("args.txt")).unwrap();
        let lines: Vec<&str> = args.lines().collect();
        let index = lines.iter().position(|arg| *arg == "-f").unwrap();
        assert_eq!(lines[index + 1], DEFAULT_FORMAT);
    }

    #[tokio::test]
    async fn auto_loads_stored_cookies_and_request_cookies_win() {
        let harness = Harness::new("");
        let cookies = TempDir::new().unwrap();
        let dir = cookies.path().join("store");
        stdfs::create_dir(&dir).unwrap();
        stdfs::write(
            dir.join("youtube.txt"),
            "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tSID\tfrom-file\n",
        )
        .unwrap();
        let app = app(harness.config_with_cookie_store(Duration::from_secs(30), dir, None));

        let payload = serde_json::json!({ "url": "https://youtu.be/abc" }).to_string();
        let (status, _, _) = request(app.clone(), Method::POST, "/download", Some(payload)).await;
        assert_eq!(status, StatusCode::OK);
        let jar = stdfs::read_to_string(harness.logs.path().join("jar.txt")).unwrap();
        assert!(jar.contains("SID\tfrom-file"), "{jar}");

        let payload = serde_json::json!({
            "url": "https://youtu.be/abc",
            "youtube_cookies": "SID=from-request",
        })
        .to_string();
        let (status, _, _) = request(app, Method::POST, "/download", Some(payload)).await;
        assert_eq!(status, StatusCode::OK);
        let jar = stdfs::read_to_string(harness.logs.path().join("jar.txt")).unwrap();
        assert!(jar.contains("SID\tfrom-request"), "{jar}");
        assert!(!jar.contains("from-file"), "{jar}");
    }

    #[tokio::test]
    async fn stores_lists_and_deletes_cookies_without_returning_values() {
        let harness = Harness::new("");
        let cookies = TempDir::new().unwrap();
        let dir = cookies.path().join("store");
        let app = app(harness.config_with_cookie_store(Duration::from_secs(30), dir.clone(), None));

        let (status, _, _) = request(
            app.clone(),
            Method::PUT,
            "/cookies/tiktok",
            Some("# Netscape HTTP Cookie File\n".into()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!dir.exists());

        let (status, _, body) = request(
            app.clone(),
            Method::PUT,
            "/cookies/tiktok",
            Some("Cookie: sessionid=secret-value; tt_chain_token=chain".into()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let stored: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(stored["platform"], "tiktok");
        assert_eq!(stored["stored"], true);
        assert!(stored["mtime"].is_u64());

        let file = dir.join("tiktok.txt");
        let contents = stdfs::read_to_string(&file).unwrap();
        assert!(
            contents.contains(".tiktok.com\tTRUE\t/\tTRUE\t2147483647\tsessionid\tsecret-value")
        );
        assert_eq!(
            stdfs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            stdfs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(stdfs::read_dir(&dir).unwrap().count(), 1);

        let (status, _, body) = request(app.clone(), Method::GET, "/cookies", None).await;
        assert_eq!(status, StatusCode::OK);
        let text = String::from_utf8(body.clone()).unwrap();
        assert!(!text.contains("secret-value"), "{text}");
        assert!(!text.contains("chain"), "{text}");
        let listed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(listed["enabled"], true);
        assert_eq!(
            listed["cookies"].as_array().unwrap().len(),
            Platform::ALL.len()
        );
        let tiktok = listed["cookies"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["platform"] == "tiktok")
            .unwrap();
        assert_eq!(tiktok["present"], true);
        assert!(tiktok["mtime"].is_u64());
        assert_eq!(
            listed["cookies"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["platform"] == "youtube")
                .unwrap()["present"],
            false
        );

        let (status, _, body) = request(app.clone(), Method::DELETE, "/cookies/tiktok", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["deleted"],
            true
        );
        assert!(!file.exists());

        let (status, _, body) = request(app.clone(), Method::DELETE, "/cookies/tiktok", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["deleted"],
            false
        );

        let (status, _, body) =
            request(app, Method::PUT, "/cookies/vimeo", Some("a=b".into())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body).contains("unknown platform"));
    }

    #[tokio::test]
    async fn requires_bearer_token_for_cookie_writes_when_configured() {
        let harness = Harness::new("");
        let cookies = TempDir::new().unwrap();
        let dir = cookies.path().join("store");
        let app = app(harness.config_with_cookie_store(
            Duration::from_secs(30),
            dir.clone(),
            Some("s3cret"),
        ));

        let (status, _, _) = request(
            app.clone(),
            Method::PUT,
            "/cookies/x",
            Some("auth_token=abc".into()),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong"),
        );
        let (status, _, _) = request_with_headers(
            app.clone(),
            Method::PUT,
            "/cookies/x",
            Some("auth_token=abc".into()),
            headers,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer s3cret"),
        );
        let (status, _, _) = request_with_headers(
            app.clone(),
            Method::PUT,
            "/cookies/x",
            Some("auth_token=abc".into()),
            headers,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(dir.join("x.txt").exists());

        let (status, _, _) = request(app.clone(), Method::GET, "/cookies", None).await;
        assert_eq!(status, StatusCode::OK);

        let (status, _, _) = request(app, Method::DELETE, "/cookies/x", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(dir.join("x.txt").exists());
    }

    #[tokio::test]
    async fn cookie_writes_require_a_cookie_dir() {
        let harness = Harness::new("");
        let app = app(harness.config(Duration::from_secs(30)));

        let (status, _, body) = request(
            app.clone(),
            Method::PUT,
            "/cookies/youtube",
            Some("SID=a".into()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body).contains("IXIAN_COOKIE_DIR"));

        let (status, _, body) = request(app.clone(), Method::GET, "/cookies", None).await;
        assert_eq!(status, StatusCode::OK);
        let listed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(listed["enabled"], false);
        assert!(listed["cookies"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["present"] == false));

        let (status, _, _) = request(app, Method::DELETE, "/cookies/youtube", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
