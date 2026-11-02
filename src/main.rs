// ixian - minimal yt-dlp wrapper (youtube / x.com / instagram) in Rust.
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path as AxumPath, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, VecDeque},
    env, io,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncSeekExt, BufReader, ReadBuf, SeekFrom},
    net::TcpListener,
    process::{Child, Command},
    sync::{watch, Semaphore},
    time::{interval, timeout},
};
use tokio_util::{io::ReaderStream, sync::CancellationToken};
use url::Url;
use uuid::Uuid;

const DEFAULT_FORMAT: &str = "bv*[vcodec^=avc1]+ba[acodec^=mp4a]/b[ext=mp4]/bv*+ba/b";
const COOKIE_HINT: &str =
    "cookies may be invalid or incomplete; public posts download without cookies";
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

struct Config {
    host: String,
    port: u16,
    ytdlp: String,
    plugin_dir: Option<PathBuf>,
    format: String,
    timeout: Duration,
    tmp: PathBuf,
    max_concurrent: usize,
    max_queue: usize,
    job_ttl: Duration,
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
        let max_concurrent = env::var("MAX_CONCURRENT")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(2);
        let max_queue = env::var("MAX_QUEUE")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(32);
        let job_ttl = env::var("JOB_TTL_MS")
            .ok()
            .and_then(|value| parse_ms(&value))
            .unwrap_or_else(|| Duration::from_millis(3_600_000));
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
            max_concurrent,
            max_queue,
            job_ttl,
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

struct DownloadRequest {
    url: String,
    platform: Platform,
    cookies: Option<String>,
    format: Option<String>,
    timeout: Option<Duration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JobStatus {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl JobStatus {
    fn name(self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug)]
struct JobFailure {
    error: String,
    code: &'static str,
    hint: Option<String>,
}

impl JobFailure {
    fn new(error: impl Into<String>, code: &'static str, hint: Option<&str>) -> Self {
        Self {
            error: error.into(),
            code,
            hint: hint.map(str::to_string),
        }
    }
}

struct JobState {
    status: JobStatus,
    progress: f64,
    bytes: u64,
    total_bytes: Option<u64>,
    eta: Option<u64>,
    error: Option<JobFailure>,
    dir: Option<PathBuf>,
    path: Option<PathBuf>,
    finished_at: Option<Instant>,
}

impl JobState {
    fn new() -> Self {
        Self {
            status: JobStatus::Queued,
            progress: 0.0,
            bytes: 0,
            total_bytes: None,
            eta: None,
            error: None,
            dir: None,
            path: None,
            finished_at: None,
        }
    }
}

struct Job {
    id: Uuid,
    request: DownloadRequest,
    state: Mutex<JobState>,
    cancel: CancellationToken,
    finished: watch::Sender<bool>,
}

impl Job {
    fn new(id: Uuid, request: DownloadRequest) -> Self {
        let (finished, _) = watch::channel(false);
        Self {
            id,
            request,
            state: Mutex::new(JobState::new()),
            cancel: CancellationToken::new(),
            finished,
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, JobState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn json(&self, position: usize) -> Value {
        let state = self.lock_state();
        json!({
            "id": self.id.to_string(),
            "status": state.status.name(),
            "progress": (state.progress * 10.0).round() / 10.0,
            "bytes": state.bytes,
            "total_bytes": state.total_bytes,
            "eta": state.eta,
            "position": position,
            "error": state.error.as_ref().map(|failure| json!({
                "error": failure.error,
                "code": failure.code,
                "hint": failure.hint,
            })),
        })
    }
}

struct JobsInner {
    jobs: HashMap<Uuid, Arc<Job>>,
    queue: VecDeque<Uuid>,
}

struct Jobs {
    inner: Mutex<JobsInner>,
    semaphore: Semaphore,
    max_queue: usize,
    ttl: Duration,
}

impl Jobs {
    fn new(config: &Config) -> Self {
        Self {
            inner: Mutex::new(JobsInner {
                jobs: HashMap::new(),
                queue: VecDeque::new(),
            }),
            semaphore: Semaphore::new(config.max_concurrent.max(1)),
            max_queue: config.max_queue.max(1),
            ttl: config.job_ttl,
        }
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, JobsInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn enqueue(self: &Arc<Self>, request: DownloadRequest) -> Result<(Arc<Job>, usize), ApiError> {
        let mut inner = self.lock_inner();
        if inner.queue.len() >= self.max_queue {
            let mut error = ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                format!("job queue is full (MAX_QUEUE={})", self.max_queue),
            );
            error.hint = Some("wait for queued jobs to finish or raise MAX_QUEUE".into());
            return Err(error);
        }

        let job = Arc::new(Job::new(Uuid::new_v4(), request));
        inner.jobs.insert(job.id, Arc::clone(&job));
        inner.queue.push_back(job.id);
        Ok((job, inner.queue.len()))
    }

    fn start(self: &Arc<Self>, config: Arc<Config>, job: Arc<Job>) {
        let jobs = Arc::clone(self);
        tokio::spawn(async move {
            jobs.run(config, job).await;
        });
    }

    async fn run(self: Arc<Self>, config: Arc<Config>, job: Arc<Job>) {
        let permit = tokio::select! {
            permit = self.semaphore.acquire() => match permit {
                Ok(permit) => permit,
                Err(_) => return,
            },
            _ = job.cancel.cancelled() => {
                self.finish(&job, JobOutcome::Cancelled).await;
                return;
            }
        };

        let should_run = {
            let mut inner = self.lock_inner();
            inner.queue.retain(|queued| *queued != job.id);
            let mut state = job.lock_state();
            if state.status == JobStatus::Queued {
                state.status = JobStatus::Running;
                true
            } else {
                false
            }
        };

        let outcome = if should_run {
            run_job(&config, &job).await
        } else {
            JobOutcome::Cancelled
        };
        self.finish(&job, outcome).await;
        drop(permit);
    }

    async fn finish(&self, job: &Arc<Job>, outcome: JobOutcome) {
        let cleanup_dir = {
            let mut inner = self.lock_inner();
            inner.queue.retain(|queued| *queued != job.id);
            let mut state = job.lock_state();
            let cancelled = state.status == JobStatus::Cancelled;

            if cancelled || matches!(&outcome, JobOutcome::Cancelled) {
                state.status = JobStatus::Cancelled;
                state.eta = None;
                state.error = None;
                state.path = None;
                state.finished_at = Some(Instant::now());
                state.dir.take()
            } else {
                match outcome {
                    JobOutcome::Done { path, size } => {
                        state.status = JobStatus::Done;
                        state.progress = 100.0;
                        state.bytes = size;
                        state.total_bytes = Some(size);
                        state.eta = Some(0);
                        state.error = None;
                        state.path = Some(path);
                        state.finished_at = Some(Instant::now());
                        None
                    }
                    JobOutcome::Failed(failure) => {
                        state.status = JobStatus::Failed;
                        state.eta = None;
                        state.error = Some(failure);
                        state.path = None;
                        state.finished_at = Some(Instant::now());
                        state.dir.take()
                    }
                    JobOutcome::Cancelled => unreachable!(),
                }
            }
        };

        if let Some(dir) = cleanup_dir {
            let _ = fs::remove_dir_all(dir).await;
        }
        let _ = job.finished.send(true);
    }

    fn get(&self, id: Uuid) -> Option<Arc<Job>> {
        self.lock_inner().jobs.get(&id).cloned()
    }

    fn snapshot(&self, id: Uuid) -> Option<Value> {
        let inner = self.lock_inner();
        let job = inner.jobs.get(&id)?;
        let position = inner
            .queue
            .iter()
            .position(|queued| *queued == id)
            .map_or(0, |index| index + 1);
        Some(job.json(position))
    }

    async fn cancel(&self, id: Uuid) -> Result<Value, ApiError> {
        let job = {
            let mut inner = self.lock_inner();
            let job = inner.jobs.get(&id).cloned().ok_or_else(job_not_found)?;
            let status = job.lock_state().status;
            match status {
                JobStatus::Queued | JobStatus::Running => {
                    inner.queue.retain(|queued| *queued != id);
                    let mut state = job.lock_state();
                    state.status = JobStatus::Cancelled;
                    state.eta = None;
                    state.error = None;
                    state.finished_at = Some(Instant::now());
                    drop(state);
                    job.cancel.cancel();
                    job
                }
                JobStatus::Cancelled => job,
                JobStatus::Done | JobStatus::Failed => {
                    return Err(ApiError::new(
                        StatusCode::CONFLICT,
                        format!("job is already {}", status.name()),
                    ));
                }
            }
        };

        let mut finished = job.finished.subscribe();
        if !*finished.borrow() {
            let _ = timeout(Duration::from_secs(5), async {
                loop {
                    if *finished.borrow() || finished.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
        }

        self.snapshot(id).ok_or_else(job_not_found)
    }

    fn cleanup_expired(&self) {
        let now = Instant::now();
        let mut dirs = Vec::new();
        {
            let mut inner = self.lock_inner();
            let expired: Vec<Uuid> = inner
                .jobs
                .iter()
                .filter_map(|(id, job)| {
                    let state = job.lock_state();
                    state
                        .finished_at
                        .is_some_and(|finished| now.duration_since(finished) >= self.ttl)
                        .then_some(*id)
                })
                .collect();
            for id in expired {
                if let Some(job) = inner.jobs.remove(&id) {
                    inner.queue.retain(|queued| *queued != id);
                    if let Some(dir) = job.lock_state().dir.take() {
                        dirs.push(dir);
                    }
                }
            }
        }
        for dir in dirs {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn spawn_cleanup(self: Arc<Self>) {
        let period = (self.ttl / 4).clamp(Duration::from_millis(100), Duration::from_secs(60));
        tokio::spawn(async move {
            let mut ticker = interval(period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                self.cleanup_expired();
            }
        });
    }
}

enum JobOutcome {
    Done { path: PathBuf, size: u64 },
    Failed(JobFailure),
    Cancelled,
}

struct ProgressUpdate {
    bytes: u64,
    total: Option<u64>,
    eta: Option<u64>,
    percent: Option<f64>,
}

const PROGRESS_PREFIX: &str = "ixian-progress|";
const JOB_PROGRESS_TEMPLATE: &str = "download:ixian-progress|%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s|%(progress.eta)s|%(progress._percent_str)s";

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

async fn prepare_cookie_jar(
    dir: &Path,
    cookies: Option<&str>,
    platform: Platform,
) -> Result<Option<PathBuf>, ApiError> {
    let Some(cookies) = cookies.filter(|cookies| !cookies.is_empty()) else {
        return Ok(None);
    };
    let jar = dir.join("cookies.txt");
    let contents = cookie_file(cookies, platform);
    fs::write(&jar, contents)
        .await
        .map_err(|error| ApiError::internal(format!("cannot write cookie jar: {error}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&jar, std::fs::Permissions::from_mode(0o600)).await;
    }
    Ok(Some(jar))
}

fn ytdlp_command(
    config: &Config,
    request: &DownloadRequest,
    dir: &Path,
    jar: Option<&Path>,
) -> Command {
    let mut cmd = Command::new(&config.ytdlp);
    cmd.arg("--no-playlist")
        // `--no-playlist` does not collapse tweets that reference other tweets
        // (yt-dlp returns them as a playlist), so pin the first item explicitly.
        .arg("--playlist-items")
        .arg("1")
        .arg("--no-simulate")
        .arg("--merge-output-format")
        .arg("mp4")
        .arg("-f")
        .arg(request.format.as_deref().unwrap_or(&config.format))
        .arg("-o")
        .arg(dir.join("%(title)s [%(id)s].%(ext)s"));
    if let Some(plugin_dir) = &config.plugin_dir {
        cmd.arg("--plugin-dirs").arg(plugin_dir);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        // Put yt-dlp in its own process group so cancelling also stops ffmpeg/curl.
        cmd.process_group(0);
    }
    if let Some(jar) = jar {
        cmd.arg("--cookies").arg(jar);
    }
    cmd.arg(request.url.replace('\\', ""));
    cmd
}

async fn download(config: &Config, request: DownloadRequest) -> ApiResult {
    let platform = request.platform;
    let request_timeout = request.timeout;
    let has_cookies = request
        .cookies
        .as_deref()
        .is_some_and(|cookies| !cookies.is_empty());
    let hint = has_cookies.then_some(COOKIE_HINT);

    let dir = config.tmp.join(format!("ixian-{}", Uuid::new_v4()));
    fs::create_dir(&dir)
        .await
        .map_err(|error| ApiError::internal(format!("cannot create temp directory: {error}")))?;
    let cleanup = Cleanup::new(dir.clone());
    let jar = prepare_cookie_jar(&dir, request.cookies.as_deref(), platform).await?;

    let mut cmd = ytdlp_command(config, &request, &dir, jar.as_deref());
    cmd.arg("--no-progress")
        .arg("--print")
        .arg("after_move:filepath");

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
            kill_process_group(&mut child).await;
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

async fn run_job(config: &Config, job: &Arc<Job>) -> JobOutcome {
    let request = &job.request;
    let has_cookies = request
        .cookies
        .as_deref()
        .is_some_and(|cookies| !cookies.is_empty());
    let hint = has_cookies.then_some(COOKIE_HINT);

    let dir = config.tmp.join(format!("ixian-{}", Uuid::new_v4()));
    if let Err(error) = fs::create_dir(&dir).await {
        return JobOutcome::Failed(JobFailure::new(
            format!("cannot create temp directory: {error}"),
            "internal",
            hint,
        ));
    }
    job.lock_state().dir = Some(dir.clone());

    if job.cancel.is_cancelled() {
        return JobOutcome::Cancelled;
    }

    let jar = match prepare_cookie_jar(&dir, request.cookies.as_deref(), request.platform).await {
        Ok(jar) => jar,
        Err(error) => {
            return JobOutcome::Failed(JobFailure::new(error.message, "internal", hint));
        }
    };

    let path_file = dir.join("path.txt");
    let mut cmd = ytdlp_command(config, request, &dir, jar.as_deref());
    cmd.arg("--newline")
        .arg("--progress-template")
        .arg(JOB_PROGRESS_TEMPLATE)
        .arg("--print-to-file")
        .arg("after_move:filepath")
        .arg(&path_file);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            return JobOutcome::Failed(JobFailure::new(
                format!("failed to run yt-dlp: {error}"),
                "spawn_failed",
                hint,
            ));
        }
    };

    let Some(mut stdout_pipe) = child.stdout.take() else {
        kill_process_group(&mut child).await;
        return JobOutcome::Failed(JobFailure::new(
            "cannot capture yt-dlp stdout",
            "internal",
            hint,
        ));
    };
    let Some(stderr_pipe) = child.stderr.take() else {
        kill_process_group(&mut child).await;
        return JobOutcome::Failed(JobFailure::new(
            "cannot capture yt-dlp stderr",
            "internal",
            hint,
        ));
    };

    let mut stdout_task = tokio::spawn(async move {
        let mut sink = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut sink).await;
    });
    let progress_job = Arc::clone(job);
    let mut stderr_task = tokio::spawn(read_job_stderr(stderr_pipe, progress_job));

    let limit = request.timeout.unwrap_or(config.timeout);
    let waited = tokio::select! {
        _ = job.cancel.cancelled() => None,
        result = timeout(limit, child.wait()) => Some(result),
    };

    let status = match waited {
        None => {
            kill_process_group(&mut child).await;
            stdout_task.abort();
            stderr_task.abort();
            return JobOutcome::Cancelled;
        }
        Some(Err(_)) => {
            kill_process_group(&mut child).await;
            stdout_task.abort();
            stderr_task.abort();
            return JobOutcome::Failed(JobFailure::new(
                format!("yt-dlp timed out after {}ms", limit.as_millis()),
                "timeout",
                hint,
            ));
        }
        Some(Ok(Err(error))) => {
            kill_process_group(&mut child).await;
            stdout_task.abort();
            stderr_task.abort();
            return JobOutcome::Failed(JobFailure::new(
                format!("failed to wait for yt-dlp: {error}"),
                "internal",
                hint,
            ));
        }
        Some(Ok(Ok(status))) => status,
    };

    let stderr = match timeout(Duration::from_secs(5), &mut stderr_task).await {
        Ok(Ok(text)) => text,
        Ok(Err(_)) => String::new(),
        Err(_) => {
            stderr_task.abort();
            String::new()
        }
    };
    if timeout(Duration::from_secs(5), &mut stdout_task)
        .await
        .is_err()
    {
        stdout_task.abort();
    }

    if job.cancel.is_cancelled() {
        return JobOutcome::Cancelled;
    }

    if !status.success() {
        return JobOutcome::Failed(JobFailure::new(
            exit_detail(&status, &stderr),
            "download_failed",
            hint,
        ));
    }

    let text = fs::read_to_string(&path_file).await.unwrap_or_default();
    let Some(media) = primary_filepath(&text) else {
        return JobOutcome::Failed(JobFailure::new(
            "yt-dlp did not produce an output file",
            "no_output",
            hint,
        ));
    };
    let Ok(metadata) = fs::metadata(&media).await else {
        return JobOutcome::Failed(JobFailure::new(
            "yt-dlp did not produce an output file",
            "no_output",
            hint,
        ));
    };
    if !metadata.is_file() {
        return JobOutcome::Failed(JobFailure::new(
            "yt-dlp did not produce an output file",
            "no_output",
            hint,
        ));
    }

    JobOutcome::Done {
        path: media,
        size: metadata.len(),
    }
}

async fn read_job_stderr(pipe: tokio::process::ChildStderr, job: Arc<Job>) -> String {
    let mut lines = BufReader::new(pipe).lines();
    let mut text = String::new();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(update) = parse_progress_line(&line) {
            let mut state = job.lock_state();
            state.bytes = update.bytes;
            if let Some(total) = update.total {
                state.total_bytes = Some(total);
            }
            state.eta = update.eta;
            state.progress = update
                .percent
                .or_else(|| {
                    update
                        .total
                        .filter(|total| *total > 0)
                        .map(|total| (update.bytes as f64 / total as f64 * 100.0).clamp(0.0, 100.0))
                })
                .unwrap_or(state.progress);
        } else if text.len() < 64 * 1024 {
            text.push_str(line.trim_end());
            text.push('\n');
        }
    }
    text
}

fn parse_progress_line(line: &str) -> Option<ProgressUpdate> {
    let rest = line.trim().strip_prefix(PROGRESS_PREFIX)?;
    let mut parts = rest.split('|');
    let bytes = parse_amount(parts.next()?)?;
    let total_bytes = parts.next().unwrap_or("");
    let estimate_bytes = parts.next().unwrap_or("");
    let eta = parse_amount(parts.next().unwrap_or(""));
    let percent = parse_percent(parts.next().unwrap_or(""));
    let total = parse_amount(total_bytes).or_else(|| parse_amount(estimate_bytes));
    Some(ProgressUpdate {
        bytes,
        total: total.filter(|total| *total > 0),
        eta,
        percent,
    })
}

fn parse_amount(value: &str) -> Option<u64> {
    let value: f64 = value.trim().parse().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value as u64)
}

fn parse_percent(value: &str) -> Option<f64> {
    let value = value.trim().trim_end_matches('%').trim();
    let value: f64 = value.parse().ok()?;
    value.is_finite().then_some(value.clamp(0.0, 100.0))
}

fn exit_detail(status: &std::process::ExitStatus, stderr: &str) -> String {
    if !stderr.trim().is_empty() {
        stderr.trim().to_string()
    } else {
        match status.code() {
            Some(code) => format!("yt-dlp exited with code {code}"),
            None => "yt-dlp was terminated by a signal".to_string(),
        }
    }
}

async fn kill_process_group(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // Negative pid targets the process group created with `process_group(0)`.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn root() -> Json<Value> {
    Json(json!({
        "message": "Hello!",
        "platforms": Platform::ALL.map(Platform::name),
    }))
}

fn parse_download_get(uri: &Uri) -> Result<DownloadRequest, ApiError> {
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

    Ok(DownloadRequest {
        url: url.to_string(),
        platform,
        cookies: params.get(&format!("{}_cookies", platform.name())).cloned(),
        format: params
            .get("format")
            .filter(|format| !format.is_empty())
            .cloned(),
        timeout: params.get("timeout_ms").and_then(|value| parse_ms(value)),
    })
}

fn parse_download_json(body: &str) -> Result<DownloadRequest, ApiError> {
    let parsed: Value = match serde_json::from_str(body) {
        Ok(value) => value,
        Err(_) => serde_json::from_str(&sanitize_json_escapes(body)).map_err(|_| {
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

    Ok(DownloadRequest {
        url: url.to_string(),
        platform,
        cookies,
        format: object
            .get("format")
            .and_then(Value::as_str)
            .filter(|format| !format.is_empty())
            .map(str::to_string),
        timeout: parse_ms_value(object.get("timeoutMs")),
    })
}

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    jobs: Arc<Jobs>,
}

async fn download_get(State(state): State<AppState>, uri: Uri) -> ApiResult {
    let request = parse_download_get(&uri)?;
    download(&state.config, request).await
}

async fn download_post(State(state): State<AppState>, body: String) -> ApiResult {
    let request = parse_download_json(&body)?;
    download(&state.config, request).await
}

async fn jobs_post(State(state): State<AppState>, body: String) -> ApiResult {
    let request = parse_download_json(&body)?;
    let (job, position) = state.jobs.enqueue(request)?;
    let response = json!({
        "id": job.id.to_string(),
        "status": job.lock_state().status.name(),
        "position": position,
    });
    state.jobs.start(Arc::clone(&state.config), job);
    Ok((StatusCode::ACCEPTED, Json(response)).into_response())
}

async fn job_get(State(state): State<AppState>, AxumPath(id): AxumPath<String>) -> ApiResult {
    let id = parse_job_id(&id)?;
    let snapshot = state.jobs.snapshot(id).ok_or_else(job_not_found)?;
    Ok(Json(snapshot).into_response())
}

async fn job_delete(State(state): State<AppState>, AxumPath(id): AxumPath<String>) -> ApiResult {
    let id = parse_job_id(&id)?;
    let snapshot = state.jobs.cancel(id).await?;
    Ok(Json(snapshot).into_response())
}

async fn job_file(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    method: Method,
    headers: HeaderMap,
) -> ApiResult {
    let id = parse_job_id(&id)?;
    let job = state.jobs.get(id).ok_or_else(job_not_found)?;
    let (status, path) = {
        let state = job.lock_state();
        (state.status, state.path.clone())
    };
    if status != JobStatus::Done {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("job is {}", status.name()),
        ));
    }
    let path = path.ok_or_else(|| ApiError::internal("job file is missing"))?;
    serve_file(&path, &headers, &method, job.request.platform).await
}

async fn serve_file(
    path: &Path,
    headers: &HeaderMap,
    method: &Method,
    platform: Platform,
) -> ApiResult {
    let metadata = fs::metadata(path)
        .await
        .map_err(|error| ApiError::internal(format!("cannot read job file: {error}")))?;
    let total = metadata.len();
    let range = match headers.get(header::RANGE) {
        Some(value) => match value
            .to_str()
            .ok()
            .and_then(|value| parse_range(value, total))
        {
            Some(range) => Some(range),
            None => return Ok(range_not_satisfiable(total)),
        },
        None => None,
    };

    let mut file = fs::File::open(path)
        .await
        .map_err(|error| ApiError::internal(format!("cannot open job file: {error}")))?;
    let status = if range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let length = range.map_or(total, |(start, end)| end - start + 1);
    let body = if method == Method::HEAD {
        Body::empty()
    } else if let Some((start, end)) = range {
        file.seek(SeekFrom::Start(start))
            .await
            .map_err(|error| ApiError::internal(format!("cannot seek job file: {error}")))?;
        Body::from_stream(ReaderStream::with_capacity(
            file.take(end - start + 1),
            STREAM_CHUNK,
        ))
    } else {
        Body::from_stream(ReaderStream::with_capacity(file, STREAM_CHUNK))
    };

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("video");
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let output = response.headers_mut();
    output.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref())
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    output.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    output.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    output.insert(
        "x-ixian-platform",
        HeaderValue::from_static(platform.name()),
    );
    if let Some((start, end)) = range {
        if let Ok(value) = HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")) {
            output.insert(header::CONTENT_RANGE, value);
        }
    }
    let disposition = format!(
        "attachment; filename*=UTF-8''{}",
        percent_encoding::utf8_percent_encode(name, FILENAME_ENCODE)
    );
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        output.insert(header::CONTENT_DISPOSITION, value);
    }
    Ok(response)
}

fn parse_range(value: &str, total: u64) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?.trim();
    if value.contains(',') || total == 0 {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    let start = start.trim();
    let end = end.trim();
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?;
        if suffix == 0 {
            return None;
        }
        Some((total.saturating_sub(suffix), total - 1))
    } else {
        let start = start.parse::<u64>().ok()?;
        if start >= total {
            return None;
        }
        let end = if end.is_empty() {
            total - 1
        } else {
            end.parse::<u64>().ok()?.min(total - 1)
        };
        (end >= start).then_some((start, end))
    }
}

fn range_not_satisfiable(total: u64) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
    let headers = response.headers_mut();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(value) = HeaderValue::from_str(&format!("bytes */{total}")) {
        headers.insert(header::CONTENT_RANGE, value);
    }
    response
}

fn parse_job_id(raw: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(raw).map_err(|_| ApiError::bad_request("invalid job id"))
}

fn job_not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "job not found")
}

async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not found")
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
}

fn app_with_jobs(config: Arc<Config>, jobs: Arc<Jobs>) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/download", get(download_get).post(download_post))
        .route("/jobs", post(jobs_post))
        .route("/jobs/{id}", get(job_get).delete(job_delete))
        .route("/jobs/{id}/file", get(job_file))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(AppState { config, jobs })
}

fn app(config: Arc<Config>) -> Router {
    let jobs = Arc::new(Jobs::new(&config));
    jobs.clone().spawn_cleanup();
    app_with_jobs(config, jobs)
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
printf '%s\n' "$@" > "$LOG/args.txt"
out=""
jar=""
print_file=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    --cookies) jar="$2"; shift 2 ;;
    --print-to-file) print_file="$3"; shift 3 ;;
    *) shift ;;
  esac
done
if [ -n "$jar" ]; then cp -p "$jar" "$LOG/jar.txt"; fi
{extra}
prefix="${{out%%\%*}}"
printf 'partial' > "${{prefix}}partial.part"
printf 'fake video data' > "${{prefix}}fake.mp4"
if [ -n "$print_file" ]; then printf '%s\n' "${{prefix}}fake.mp4" > "$print_file"; fi
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
            self.config_with_limits(timeout, plugin_dir, 2, 32, Duration::from_secs(3_600))
        }

        fn config_with_limits(
            &self,
            timeout: Duration,
            plugin_dir: Option<PathBuf>,
            max_concurrent: usize,
            max_queue: usize,
            job_ttl: Duration,
        ) -> Arc<Config> {
            Arc::new(Config {
                host: "127.0.0.1".into(),
                port: 0,
                ytdlp: self.script.to_string_lossy().into_owned(),
                plugin_dir,
                format: DEFAULT_FORMAT.into(),
                timeout,
                tmp: self.tmp.path().to_path_buf(),
                max_concurrent,
                max_queue,
                job_ttl,
            })
        }
    }

    async fn request(
        app: Router,
        method: Method,
        uri: &str,
        body: Option<String>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        request_with_headers(app, method, uri, &[], body).await
    }

    async fn request_with_headers(
        app: Router,
        method: Method,
        uri: &str,
        extra: &[(&str, &str)],
        body: Option<String>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in extra {
            builder = builder.header(*name, *value);
        }
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

    async fn job(app: &Router, id: &str) -> Value {
        let (status, _, body) =
            request(app.clone(), Method::GET, &format!("/jobs/{id}"), None).await;
        assert_eq!(status, StatusCode::OK);
        serde_json::from_slice(&body).unwrap()
    }

    async fn wait_for_job(app: &Router, id: &str, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let value = job(app, id).await;
            if predicate(&value) {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "job {id} did not reach expected state: {value}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
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
    async fn runs_jobs_reports_progress_and_serves_ranges() {
        let harness = Harness::new(
            r#"printf 'ixian-progress|5|10|NA|2|50.0%%\n' >&2
sleep 1
printf 'ixian-progress|10|10|10|0|100.0%%\n' >&2"#,
        );
        let app = app(harness.config(Duration::from_secs(30)));
        let payload = serde_json::json!({
            "url": "https://www.youtube.com/watch?v=abc",
            "format": "b[ext=mp4]",
        })
        .to_string();

        let (status, _, body) = request(app.clone(), Method::POST, "/jobs", Some(payload)).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let created: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(created["status"], "queued");
        assert_eq!(created["position"], 1);
        let id = created["id"].as_str().unwrap().to_string();

        let running = wait_for_job(&app, &id, |job| job["bytes"].as_u64().unwrap_or(0) >= 5).await;
        assert_eq!(running["status"], "running");
        assert_eq!(running["progress"], 50.0);
        assert_eq!(running["bytes"], 5);
        assert_eq!(running["total_bytes"], 10);
        assert_eq!(running["eta"], 2);
        assert!(running["error"].is_null());

        let done = wait_for_job(&app, &id, |job| job["status"] == "done").await;
        assert_eq!(done["progress"], 100.0);
        assert_eq!(done["bytes"], 15);
        assert_eq!(done["total_bytes"], 15);
        assert_eq!(done["eta"], 0);
        assert!(done["error"].is_null());

        let args = stdfs::read_to_string(harness.logs.path().join("args.txt")).unwrap();
        let lines: Vec<&str> = args.lines().collect();
        assert!(lines.contains(&"--newline"), "{args}");
        assert!(lines.contains(&"--progress-template"), "{args}");
        assert!(!lines.contains(&"--print"), "{args}");
        let index = lines
            .iter()
            .position(|arg| *arg == "--print-to-file")
            .unwrap();
        assert_eq!(lines[index + 1], "after_move:filepath");
        assert!(args.contains("ixian-progress|%(progress.downloaded_bytes)s"));

        let path = format!("/jobs/{id}/file");
        let (status, headers, body) = request(app.clone(), Method::GET, &path, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"fake video data");
        assert_eq!(headers[header::CONTENT_LENGTH], "15");
        assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
        assert_eq!(headers["x-ixian-platform"], "youtube");

        let (status, headers, body) = request_with_headers(
            app.clone(),
            Method::GET,
            &path,
            &[(header::RANGE.as_str(), "bytes=5-9")],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(headers[header::CONTENT_RANGE], "bytes 5-9/15");
        assert_eq!(headers[header::CONTENT_LENGTH], "5");
        assert_eq!(body, b"video");

        let (status, headers, body) = request_with_headers(
            app.clone(),
            Method::GET,
            &path,
            &[(header::RANGE.as_str(), "bytes=-4")],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(headers[header::CONTENT_RANGE], "bytes 11-14/15");
        assert_eq!(body, b"data");

        let (status, headers, body) = request(app.clone(), Method::HEAD, &path, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_LENGTH], "15");
        assert!(body.is_empty());

        let (status, headers, body) = request_with_headers(
            app.clone(),
            Method::GET,
            &path,
            &[(header::RANGE.as_str(), "bytes=99-")],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(headers[header::CONTENT_RANGE], "bytes */15");
        assert!(body.is_empty());

        let (status, _, _) =
            request(app, Method::GET, &format!("/jobs/{}", Uuid::new_v4()), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cancels_queued_and_running_jobs_and_bounds_the_queue() {
        let harness = Harness::new("sleep 5");
        let config = harness.config_with_limits(
            Duration::from_secs(30),
            None,
            1,
            1,
            Duration::from_secs(3_600),
        );
        let app = app(config);
        let payload = serde_json::json!({ "url": "https://youtu.be/abc" }).to_string();

        let (status, _, body) =
            request(app.clone(), Method::POST, "/jobs", Some(payload.clone())).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let first: Value = serde_json::from_slice(&body).unwrap();
        let first_id = first["id"].as_str().unwrap().to_string();
        wait_for_job(&app, &first_id, |job| job["status"] == "running").await;

        let (status, _, body) =
            request(app.clone(), Method::POST, "/jobs", Some(payload.clone())).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let second: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(second["status"], "queued");
        assert_eq!(second["position"], 1);
        let second_id = second["id"].as_str().unwrap().to_string();

        let (status, _, body) = request(app.clone(), Method::POST, "/jobs", Some(payload)).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert!(error["error"].as_str().unwrap().contains("queue is full"));

        let (status, _, body) = request(
            app.clone(),
            Method::DELETE,
            &format!("/jobs/{second_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let cancelled: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(cancelled["status"], "cancelled");
        assert!(cancelled["error"].is_null());

        let second = wait_for_job(&app, &second_id, |job| job["status"] == "cancelled").await;
        assert_eq!(second["position"], 0);

        let (status, _, body) = request(
            app.clone(),
            Method::DELETE,
            &format!("/jobs/{first_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let cancelled: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(stdfs::read_dir(harness.tmp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn cancel_kills_the_whole_process_group() {
        let harness = Harness::new(
            r#"( sleep 1; printf alive > "$LOG/alive" ) &
echo $! > "$LOG/child.pid"
wait"#,
        );
        let app = app(harness.config(Duration::from_secs(30)));
        let payload = serde_json::json!({ "url": "https://youtu.be/abc" }).to_string();

        let (status, _, body) = request(app.clone(), Method::POST, "/jobs", Some(payload)).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let id: Value = serde_json::from_slice(&body).unwrap();
        let id = id["id"].as_str().unwrap().to_string();
        wait_for_job(&app, &id, |job| job["status"] == "running").await;

        let child_file = harness.logs.path().join("child.pid");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !child_file.exists() {
            assert!(Instant::now() < deadline, "stub did not start its child");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let (status, _, body) =
            request(app.clone(), Method::DELETE, &format!("/jobs/{id}"), None).await;
        assert_eq!(status, StatusCode::OK);
        let cancelled: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(cancelled["status"], "cancelled");

        tokio::time::sleep(Duration::from_millis(1_300)).await;
        assert!(
            !harness.logs.path().join("alive").exists(),
            "child survived job cancellation"
        );
    }

    #[tokio::test]
    async fn times_out_jobs_and_cleans_their_files() {
        let harness = Harness::new("sleep 5");
        let app = app(harness.config(Duration::from_secs(30)));
        let payload = serde_json::json!({
            "url": "https://youtu.be/abc",
            "timeoutMs": 100,
        })
        .to_string();

        let (status, _, body) = request(app.clone(), Method::POST, "/jobs", Some(payload)).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let created: Value = serde_json::from_slice(&body).unwrap();
        let id = created["id"].as_str().unwrap().to_string();

        let failed = wait_for_job(&app, &id, |job| job["status"] == "failed").await;
        assert!(failed["error"]["error"]
            .as_str()
            .unwrap()
            .contains("timed out"));
        assert_eq!(failed["error"]["code"], "timeout");
        assert!(failed["error"]["hint"].is_null());
        assert!(failed["eta"].is_null());
        assert_eq!(stdfs::read_dir(harness.tmp.path()).unwrap().count(), 0);

        let (status, _, _) = request(app, Method::GET, &format!("/jobs/{id}/file"), None).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn cleans_up_expired_jobs() {
        let harness = Harness::new("");
        let config = harness.config_with_limits(
            Duration::from_secs(30),
            None,
            1,
            4,
            Duration::from_millis(0),
        );
        let jobs = Arc::new(Jobs::new(&config));
        let app = app_with_jobs(config, Arc::clone(&jobs));
        let payload = serde_json::json!({ "url": "https://youtu.be/abc" }).to_string();

        let (status, _, body) = request(app.clone(), Method::POST, "/jobs", Some(payload)).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let created: Value = serde_json::from_slice(&body).unwrap();
        let id = created["id"].as_str().unwrap().to_string();
        wait_for_job(&app, &id, |job| job["status"] == "done").await;
        assert_eq!(stdfs::read_dir(harness.tmp.path()).unwrap().count(), 1);

        jobs.cleanup_expired();

        let (status, _, _) = request(app, Method::GET, &format!("/jobs/{id}"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(stdfs::read_dir(harness.tmp.path()).unwrap().count(), 0);
    }
}
