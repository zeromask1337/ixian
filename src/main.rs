// ixian - minimal yt-dlp wrapper (youtube / x.com / instagram / tiktok) in Rust.
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path as UrlPath, State},
    http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    env,
    io::{self, Write},
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex as StdMutex, Weak,
    },
    task::{Context, Poll},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader, ReadBuf},
    net::TcpListener,
    process::Command,
    sync::{watch, Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore},
    time::{interval, timeout},
};
use tokio_util::{io::ReaderStream, sync::CancellationToken};
use tower::ServiceExt;
use tower_http::services::ServeFile;
use url::Url;
use uuid::Uuid;

const DEFAULT_FORMAT: &str = "bv*[vcodec^=avc1]+ba[acodec^=mp4a]/b[ext=mp4]/bv*+ba/b";
const COOKIE_HINT: &str =
    "cookies may be invalid or incomplete; public posts download without cookies";
const DEFAULT_INFO_TIMEOUT_MS: u64 = 15_000;
const COOKIE_DIR_MODE: u32 = 0o700;
const COOKIE_FILE_MODE: u32 = 0o600;
const MAX_STORED_COOKIE_BYTES: usize = 1024 * 1024;
const GET_COOKIE_HINT: &str = "send cookies in a POST /download JSON body instead";
const GET_COOKIE_PARAMS: [&str; 4] = [
    "youtube_cookies",
    "x_cookies",
    "instagram_cookies",
    "tiktok_cookies",
];
const STREAM_CHUNK: usize = 64 * 1024;
const DEFAULT_CACHE_MAX_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const DEFAULT_CACHE_TTL_SECS: u64 = 7 * 24 * 60 * 60;
const MAX_CONCURRENT: usize = 4;
const DEFAULT_MAX_QUEUE: usize = 32;
const DEFAULT_JOB_TTL_MS: u64 = 3_600_000;
const MAX_TIMEOUT_MS: u64 = 10 * 60 * 1000;
const MAX_COOKIE_BYTES: usize = 256 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_FILESIZE: u64 = 2 * 1024 * 1024 * 1024;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const SOCKET_TIMEOUT_SECS: u64 = 10;
const RETRIES: u64 = 3;
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const KILL_TIMEOUT: Duration = Duration::from_secs(2);
const PROGRESS_PREFIX: &str = "ixian-progress|";
const JOB_PROGRESS_TEMPLATE: &str = "download:ixian-progress|%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s|%(progress.eta)s|%(progress._percent_str)s";
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

    fn name(self) -> &'static str {
        match self {
            Mode::Video => "video",
            Mode::Audio => "audio",
        }
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

    fn name(self) -> &'static str {
        match self {
            Quality::Best => "best",
            Quality::P1080 => "1080",
            Quality::P720 => "720",
            Quality::P480 => "480",
            Quality::Data => "data",
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
    cache: Option<Cache>,
    cookie_dir: Option<PathBuf>,
    token: Option<String>,
    downloads: Arc<Semaphore>,
    max_filesize: u64,
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
            .unwrap_or_else(|| Duration::from_millis(MAX_TIMEOUT_MS))
            .min(Duration::from_millis(MAX_TIMEOUT_MS));
        let info_timeout = env::var("YTDLP_INFO_TIMEOUT_MS")
            .ok()
            .and_then(|v| parse_ms(&v))
            .unwrap_or_else(|| Duration::from_millis(DEFAULT_INFO_TIMEOUT_MS))
            .min(Duration::from_millis(MAX_TIMEOUT_MS));
        let max_concurrent = env::var("MAX_CONCURRENT")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(MAX_CONCURRENT);
        let max_queue = env::var("MAX_QUEUE")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_MAX_QUEUE);
        let job_ttl = env::var("JOB_TTL_MS")
            .ok()
            .and_then(|value| parse_ms(&value))
            .unwrap_or_else(|| Duration::from_millis(DEFAULT_JOB_TTL_MS));
        let tmp = non_empty(env::var("TMPDIR").ok())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let cache_dir_env = env::var("IXIAN_CACHE_DIR").ok();
        let cache_enabled = cache_dir_env
            .as_deref()
            .map(|value| {
                !matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "off" | "0" | "none" | "false"
                )
            })
            .unwrap_or(true);
        let cache = cache_enabled.then(|| {
            let dir = non_empty(cache_dir_env)
                .map(PathBuf::from)
                .unwrap_or_else(|| tmp.join("ixian-cache"));
            let ttl_secs = env::var("CACHE_TTL")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(DEFAULT_CACHE_TTL_SECS);
            let ttl = (ttl_secs > 0).then(|| Duration::from_secs(ttl_secs));
            let max_bytes = env::var("CACHE_MAX_BYTES")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(DEFAULT_CACHE_MAX_BYTES);
            let max_bytes = (max_bytes > 0).then_some(max_bytes);
            Cache::new(dir, ttl, max_bytes)
        });
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
            info_timeout,
            tmp,
            cache,
            cookie_dir,
            token,
            downloads: Arc::new(Semaphore::new(max_concurrent)),
            max_filesize: MAX_FILESIZE,
            max_queue,
            job_ttl,
        }
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn sha256_hex(data: &[u8]) -> String {
    hex_digest(&Sha256::digest(data))
}

fn cookie_fingerprint(cookies: Option<&str>) -> String {
    match cookies.filter(|value| !value.is_empty()) {
        Some(cookies) => sha256_hex(cookies.as_bytes()),
        None => "anonymous".to_string(),
    }
}

fn is_tracking_param(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("utm_")
        || matches!(
            name.as_str(),
            "fbclid"
                | "gclid"
                | "igshid"
                | "igsh"
                | "si"
                | "feature"
                | "ref"
                | "ref_src"
                | "ref_url"
                | "s"
                | "t"
                | "pp"
                | "ab_channel"
                | "twclid"
                | "mc_cid"
                | "mc_eid"
                | "is_from_webapp"
                | "sender_device"
                | "share_app_id"
                | "share_item_id"
                | "social_sharing"
                | "_r"
                | "_t"
        )
}

/// Canonical form used only for cache keys: tracking params dropped, queries
/// sorted, share hosts and short links folded onto one URL.
fn normalized_url(raw: &str) -> String {
    let cleaned = raw.replace('\\', "");
    let trimmed = cleaned.trim();
    let Ok(mut url) = Url::parse(trimmed) else {
        return trimmed.to_string();
    };
    url.set_fragment(None);

    let original_host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let host = original_host
        .strip_prefix("www.")
        .unwrap_or(&original_host)
        .to_string();
    let canonical_host = match host.as_str() {
        "youtu.be" => Some("youtube.com"),
        value if value == "youtube.com" || value.ends_with(".youtube.com") => Some("youtube.com"),
        value if value == "twitter.com" || value.ends_with(".twitter.com") => Some("x.com"),
        value if value == "x.com" || value.ends_with(".x.com") => Some("x.com"),
        value if value == "instagram.com" || value.ends_with(".instagram.com") => {
            Some("instagram.com")
        }
        value if value == "tiktok.com" || value.ends_with(".tiktok.com") => Some("tiktok.com"),
        _ => None,
    };
    if let Some(host) = canonical_host {
        let _ = url.set_host(Some(host));
        if url.scheme() == "http" {
            let _ = url.set_scheme("https");
        }
    }

    let mut path = percent_encoding::percent_decode_str(url.path())
        .decode_utf8_lossy()
        .into_owned();
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    if path.is_empty() {
        path.push('/');
    }

    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();

    let youtube_id = if original_host == "youtu.be" || original_host.ends_with(".youtu.be") {
        path.trim_start_matches('/')
            .split('/')
            .next()
            .filter(|id| !id.is_empty())
            .map(str::to_string)
    } else if canonical_host == Some("youtube.com") {
        let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        if segments.len() >= 2 && matches!(segments[0], "shorts" | "embed" | "live") {
            Some(segments[1].to_string())
        } else {
            None
        }
    } else {
        None
    };
    if let Some(id) = youtube_id {
        path = "/watch".to_string();
        pairs.retain(|(key, _)| key != "v");
        pairs.push(("v".to_string(), id));
    }

    url.set_path(&path);
    pairs.retain(|(key, _)| !is_tracking_param(key));
    pairs.sort();
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in &pairs {
        serializer.append_pair(key, value);
    }
    let query = serializer.finish();
    url.set_query((!query.is_empty()).then_some(query.as_str()));
    url.to_string()
}

fn hash_component(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn cache_key(
    platform: Platform,
    url: &str,
    format: &str,
    variant: &str,
    cookies: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hash_component(&mut hasher, b"ixian-cache-v1");
    hash_component(&mut hasher, platform.name().as_bytes());
    hash_component(&mut hasher, normalized_url(url).as_bytes());
    hash_component(&mut hasher, format.as_bytes());
    // Output modes share a URL but not a file: mode/quality/container/item
    // must not collide in the cache.
    hash_component(&mut hasher, b"download");
    hash_component(&mut hasher, variant.as_bytes());
    hash_component(&mut hasher, cookie_fingerprint(cookies).as_bytes());
    hex_digest(&hasher.finalize())
}

fn parse_bool(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    value.is_empty() || matches!(value.as_str(), "1" | "true" | "yes" | "on")
}

fn parse_bool_value(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(value)) => *value,
        Some(Value::String(value)) => parse_bool(value),
        Some(Value::Number(value)) => value.as_u64() == Some(1),
        _ => false,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheMeta {
    key: String,
    file: String,
    name: String,
    size: u64,
    content_type: String,
    platform: String,
    created: u64,
    last_used: u64,
}

struct CacheEntry {
    path: PathBuf,
    meta: CacheMeta,
}

#[derive(Debug, Serialize)]
struct CacheStats {
    enabled: bool,
    entries: usize,
    bytes: u64,
    max_bytes: Option<u64>,
    ttl_seconds: Option<u64>,
    hits: u64,
    misses: u64,
    evictions: u64,
}

struct Cache {
    dir: PathBuf,
    ttl: Option<Duration>,
    max_bytes: Option<u64>,
    locks: StdMutex<HashMap<String, Weak<AsyncMutex<()>>>>,
    store: AsyncMutex<()>,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

impl Cache {
    fn new(dir: PathBuf, ttl: Option<Duration>, max_bytes: Option<u64>) -> Self {
        Self {
            dir,
            ttl,
            max_bytes,
            locks: StdMutex::new(HashMap::new()),
            store: AsyncMutex::new(()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    fn objects_dir(&self) -> PathBuf {
        self.dir.join("objects")
    }

    fn key_lock(&self, key: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
            return lock;
        }
        if locks.len() > 1024 {
            locks.retain(|_, lock| lock.strong_count() > 0);
        }
        let lock = Arc::new(AsyncMutex::new(()));
        locks.insert(key.to_string(), Arc::downgrade(&lock));
        lock
    }

    fn has_live_lock(&self, key: &str) -> bool {
        self.locks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(key)
            .and_then(Weak::upgrade)
            .is_some()
    }

    async fn ensure_dirs(&self) -> io::Result<()> {
        fs::create_dir_all(self.objects_dir()).await?;
        fs::create_dir_all(self.dir.join("staging")).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700)).await;
        }
        Ok(())
    }

    fn expired(&self, meta: &CacheMeta) -> bool {
        self.ttl
            .is_some_and(|ttl| now_secs().saturating_sub(meta.created) > ttl.as_secs())
    }

    async fn write_meta(dir: &Path, meta: &CacheMeta) -> io::Result<()> {
        let bytes = serde_json::to_vec(meta).map_err(io::Error::other)?;
        let staging = dir.join("meta.json.tmp");
        fs::write(&staging, bytes).await?;
        fs::rename(staging, dir.join("meta.json")).await
    }

    async fn lookup(&self, key: &str) -> Option<CacheEntry> {
        let _store = self.store.lock().await;
        let dir = self.objects_dir().join(key);
        let bytes = fs::read(dir.join("meta.json")).await.ok()?;
        let Ok(mut meta) = serde_json::from_slice::<CacheMeta>(&bytes) else {
            let _ = fs::remove_dir_all(&dir).await;
            return None;
        };
        if meta.key != key || !safe_cache_file_name(&meta.file) {
            let _ = fs::remove_dir_all(&dir).await;
            return None;
        }
        if self.expired(&meta) {
            let _ = fs::remove_dir_all(&dir).await;
            return None;
        }
        let path = dir.join(&meta.file);
        let Ok(file_meta) = fs::symlink_metadata(&path).await else {
            let _ = fs::remove_dir_all(&dir).await;
            return None;
        };
        if !file_meta.file_type().is_file() || file_meta.len() != meta.size {
            let _ = fs::remove_dir_all(&dir).await;
            return None;
        }
        meta.last_used = now_secs();
        let _ = Self::write_meta(&dir, &meta).await;
        Some(CacheEntry { path, meta })
    }

    async fn insert(
        &self,
        key: &str,
        path: &Path,
        name: &str,
        mime: &str,
        size: u64,
        platform: Platform,
    ) -> Option<CacheEntry> {
        if self.max_bytes.is_some_and(|max_bytes| size > max_bytes) {
            return None;
        }
        self.ensure_dirs().await.ok()?;

        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .filter(|extension| {
                !extension.is_empty()
                    && extension.len() <= 16
                    && extension.chars().all(|ch| ch.is_ascii_alphanumeric())
            })
            .unwrap_or("bin");
        let file = format!("data.{extension}");
        let now = now_secs();
        let meta = CacheMeta {
            key: key.to_string(),
            file: file.clone(),
            name: name.to_string(),
            size,
            content_type: mime.to_string(),
            platform: platform.name().to_string(),
            created: now,
            last_used: now,
        };

        let staging = self
            .dir
            .join("staging")
            .join(format!("ixian-{}", Uuid::new_v4()));
        fs::create_dir_all(&staging).await.ok()?;
        let staged = staging.join(&file);
        if fs::hard_link(path, &staged).await.is_err() && fs::copy(path, &staged).await.is_err() {
            let _ = fs::remove_dir_all(&staging).await;
            return None;
        }
        let Ok(meta_bytes) = serde_json::to_vec(&meta) else {
            let _ = fs::remove_dir_all(&staging).await;
            return None;
        };
        if fs::write(staging.join("meta.json"), meta_bytes)
            .await
            .is_err()
        {
            let _ = fs::remove_dir_all(&staging).await;
            return None;
        }

        let _store = self.store.lock().await;
        let objects = self.objects_dir();
        if fs::create_dir_all(&objects).await.is_err() {
            let _ = fs::remove_dir_all(&staging).await;
            return None;
        }
        let target = objects.join(key);
        if fs::symlink_metadata(&target).await.is_ok() {
            let _ = fs::remove_dir_all(&target).await;
        }
        if fs::rename(&staging, &target).await.is_err() {
            let _ = fs::remove_dir_all(&staging).await;
            return None;
        }
        self.enforce_limit_locked(key).await;
        Some(CacheEntry {
            path: target.join(file),
            meta,
        })
    }

    async fn entries_locked(&self) -> Vec<CacheMeta> {
        let mut entries = Vec::new();
        let Ok(mut read) = fs::read_dir(self.objects_dir()).await else {
            return entries;
        };
        while let Ok(Some(entry)) = read.next_entry().await {
            if !entry
                .file_type()
                .await
                .map(|kind| kind.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let Ok(bytes) = fs::read(entry.path().join("meta.json")).await else {
                continue;
            };
            let Ok(meta) = serde_json::from_slice::<CacheMeta>(&bytes) else {
                continue;
            };
            if entry.file_name().to_string_lossy() == meta.key {
                entries.push(meta);
            }
        }
        entries
    }

    async fn enforce_limit_locked(&self, keep: &str) {
        let Some(max_bytes) = self.max_bytes else {
            return;
        };
        let mut entries = self.entries_locked().await;
        let mut total: u64 = entries.iter().map(|meta| meta.size).sum();
        if total <= max_bytes {
            return;
        }
        entries.sort_by_key(|meta| meta.last_used);
        for meta in entries {
            if total <= max_bytes {
                break;
            }
            if meta.key == keep || self.has_live_lock(&meta.key) {
                continue;
            }
            let dir = self.objects_dir().join(&meta.key);
            if fs::remove_dir_all(&dir).await.is_ok() {
                total = total.saturating_sub(meta.size);
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    async fn stats(&self) -> CacheStats {
        let _store = self.store.lock().await;
        let entries = self.entries_locked().await;
        CacheStats {
            enabled: true,
            entries: entries.len(),
            bytes: entries.iter().map(|meta| meta.size).sum(),
            max_bytes: self.max_bytes,
            ttl_seconds: self.ttl.map(|ttl| ttl.as_secs()),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
        }
    }

    async fn clear(&self) -> (usize, u64) {
        let _store = self.store.lock().await;
        let Ok(mut read) = fs::read_dir(self.objects_dir()).await else {
            return (0, 0);
        };
        let mut cleared = 0;
        let mut bytes = 0;
        while let Ok(Some(entry)) = read.next_entry().await {
            if !entry
                .file_type()
                .await
                .map(|kind| kind.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let size = fs::read(entry.path().join("meta.json"))
                .await
                .ok()
                .and_then(|bytes| serde_json::from_slice::<CacheMeta>(&bytes).ok())
                .map(|meta| meta.size)
                .unwrap_or(0);
            if fs::remove_dir_all(entry.path()).await.is_ok() {
                cleared += 1;
                bytes += size;
            }
        }
        (cleared, bytes)
    }
}

fn safe_cache_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "meta.json"
        && !name.contains('/')
        && !name.contains('\\')
        && Path::new(name).file_name().and_then(|value| value.to_str()) == Some(name)
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

    fn classified(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            hint: Some(hint_for(status).to_string()),
        }
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
        body.insert("code".into(), json!(self.status.as_u16()));
        if let Some(hint) = self.hint {
            body.insert("hint".into(), Value::String(hint));
        }
        (self.status, Json(Value::Object(body))).into_response()
    }
}

type ApiResult = Result<Response, ApiError>;

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

/// Parses and validates a request URL, returning the cleaned URL (stray
/// backslashes removed) and the platform it belongs to. A `platform` override
/// still has to match the URL host: it never widens the allowlist.
fn validate_url(raw_url: &str, requested: Option<&str>) -> Result<(String, Platform), ApiError> {
    let platform = match requested.filter(|name| !name.is_empty()) {
        Some(requested) => parse_platform(requested)?,
        None => detect_platform(raw_url).ok_or_else(|| {
            ApiError::bad_request(format!(
                "cannot detect platform (supported: {}); pass 'platform' to override",
                supported_names()
            ))
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
    let contents = if is_netscape_cookie_file(input) {
        let has_entry = input.lines().any(|line| {
            let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
            !line.trim_start().starts_with('#') && line.split('\t').count() >= 7
        });
        if !has_entry {
            return Err(ApiError::bad_request(
                "cookie file has no entries (expected a Netscape cookie file)",
            ));
        }
        format!("{input}\n")
    } else {
        let file = cookie_file(input, platform);
        if file.lines().count() <= 1 {
            return Err(ApiError::bad_request(
                "no cookies found (send a Cookie header or a Netscape cookie file)",
            ));
        }
        file
    };
    // Expansion of a raw header (one line per domain) can outgrow the input.
    if contents.len() > MAX_STORED_COOKIE_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("cookie file too large (max {MAX_STORED_COOKIE_BYTES} bytes)"),
        ));
    }
    Ok(contents)
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
        .map(str::trim)
        .or_else(|| {
            headers
                .get("x-ixian-token")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
        });
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
    mode: Mode,
    quality: Quality,
    container: Container,
    item: usize,
    timeout: Option<Duration>,
    inline: bool,
}

impl DownloadRequest {
    /// Cache partition for the output shape. `format` is hashed separately.
    fn cache_variant(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.mode.name(),
            self.quality.name(),
            self.container.name(),
            self.item
        )
    }
}

struct InfoRequest {
    url: String,
    platform: Platform,
    cookies: Option<String>,
    timeout: Option<Duration>,
}

/// A completed yt-dlp download, still inside its per-download temp directory.
struct DownloadedMedia {
    path: PathBuf,
    name: String,
    mime: String,
    size: u64,
    platform: Platform,
    hint: Option<&'static str>,
    cleanup: Cleanup,
    permit: OwnedSemaphorePermit,
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

struct YtdlpOutput {
    success: bool,
    status_code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Runs yt-dlp in its own process group with bounded pipe capture and a hard
/// timeout. A non-zero exit is returned as data so callers can classify stderr.
async fn run_ytdlp(mut command: Command, limit: Duration) -> Result<YtdlpOutput, ApiError> {
    #[cfg(unix)]
    command.process_group(0);
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

    let status = match timeout(limit, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            group.kill();
            return Err(ApiError::bad_gateway(
                format!("failed to wait for yt-dlp: {error}"),
                Some(hint_for(StatusCode::BAD_GATEWAY)),
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
            return Err(ApiError::classified(
                StatusCode::GATEWAY_TIMEOUT,
                format!("yt-dlp timed out after {}ms", limit.as_millis()),
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

    Ok(YtdlpOutput {
        success: status.success(),
        status_code: status.code(),
        stdout: stdout.unwrap_or_default(),
        stderr: stderr.unwrap_or_default(),
    })
}

struct PreparedDownload {
    jar: Option<PathBuf>,
    selector: String,
    hint: Option<&'static str>,
}

/// Resolves cookies (request or stored) and writes the private cookie jar, then
/// builds the format selector shared by synchronous and asynchronous downloads.
async fn prepare_download(
    config: &Config,
    request: &DownloadRequest,
    dir: &Path,
) -> Result<PreparedDownload, ApiError> {
    let platform = request.platform;
    let cookies = request
        .cookies
        .clone()
        .filter(|cookies| !cookies.trim().is_empty());
    if cookies
        .as_deref()
        .is_some_and(|cookies| cookies.len() > MAX_COOKIE_BYTES)
    {
        return Err(ApiError::too_large(format!(
            "cookies exceed the {MAX_COOKIE_BYTES} byte limit"
        )));
    }
    let cookies = match cookies {
        Some(cookies) => Some(cookies),
        None => stored_cookies(config, platform).await?,
    };
    let has_cookies = cookies.is_some();
    let hint = has_cookies.then_some(COOKIE_HINT);
    let jar = has_cookies.then(|| dir.join("cookies.txt"));
    if let (Some(jar), Some(cookies)) = (jar.as_deref(), cookies.as_deref()) {
        let contents = cookie_file(cookies, platform);
        // Stored cookie files may be larger than a request cookie; both are
        // bounded well below the body limit.
        if contents.len() > MAX_STORED_COOKIE_BYTES {
            return Err(ApiError::too_large(format!(
                "cookies exceed the {MAX_STORED_COOKIE_BYTES} byte limit"
            )));
        }
        write_cookie_jar(jar, &contents)
            .map_err(|error| ApiError::internal(format!("cannot write cookie jar: {error}")))?;
    }

    let selector = request.format.clone().unwrap_or_else(|| {
        if request.mode.is_audio() {
            "ba/b".to_string()
        } else {
            request.quality.selector(request.container, &config.format)
        }
    });

    Ok(PreparedDownload {
        jar,
        selector,
        hint,
    })
}

fn download_command(
    config: &Config,
    request: &DownloadRequest,
    prepared: &PreparedDownload,
    dir: &Path,
    progress: bool,
) -> Command {
    let mut cmd = Command::new(&config.ytdlp);
    cmd.arg("--no-playlist")
        // `--no-playlist` does not collapse tweets that reference other tweets
        // (yt-dlp returns them as a playlist), so pin the requested item explicitly.
        .arg("--playlist-items")
        .arg(request.item.to_string());
    if progress {
        // Jobs read progress from stderr and the final path from `--print-to-file`.
        cmd.arg("--newline")
            .arg("--progress-template")
            .arg(JOB_PROGRESS_TEMPLATE);
    } else {
        cmd.arg("--no-progress");
    }
    cmd.arg("--no-simulate");
    if !progress {
        cmd.arg("--print").arg("after_move:filepath");
    }
    cmd.arg("--socket-timeout")
        .arg(SOCKET_TIMEOUT_SECS.to_string())
        .arg("--retries")
        .arg(RETRIES.to_string())
        .arg("--max-filesize")
        .arg(config.max_filesize.to_string())
        .arg("-f")
        .arg(&prepared.selector)
        .arg("-o")
        .arg(dir.join("%(title)s [%(id)s].%(ext)s"));
    if request.mode.is_audio() {
        cmd.arg("-x")
            .arg("--audio-format")
            .arg("m4a")
            .arg("--audio-quality")
            .arg("0");
    } else {
        cmd.arg("--merge-output-format")
            .arg(request.container.name());
    }
    if let Some(plugin_dir) = &config.plugin_dir {
        cmd.arg("--plugin-dirs").arg(plugin_dir);
    }
    if let Some(jar) = &prepared.jar {
        cmd.arg("--cookies").arg(jar);
    }
    // `--` terminates option parsing so a leading dash can never become a flag.
    cmd.arg("--").arg(request.url.replace('\\', ""));
    cmd
}

async fn fetch(config: &Config, request: &DownloadRequest) -> Result<DownloadedMedia, ApiError> {
    let platform = request.platform;
    let permit = config.downloads.clone().try_acquire_owned().map_err(|_| {
        ApiError::too_many_requests(format!(
            "too many concurrent downloads (limit: {MAX_CONCURRENT})"
        ))
    })?;

    let dir = config.tmp.join(format!("ixian-{}", Uuid::new_v4()));
    create_private_dir(&dir)
        .map_err(|error| ApiError::internal(format!("cannot create temp directory: {error}")))?;
    let cleanup = Cleanup::new(dir.clone());
    let prepared = prepare_download(config, request, &dir).await?;
    let hint = prepared.hint;

    let cmd = download_command(config, request, &prepared, &dir, false);

    let limit = request.timeout.unwrap_or(config.timeout);
    let output = run_ytdlp(cmd, limit).await?;
    if !output.success {
        return Err(ytdlp_failure(&output.stderr, output.status_code, hint));
    }

    // `--playlist-items N` normally yields one path (the selected item). Some
    // extractors still print several, so prefer the Nth and fall back to the
    // only path yt-dlp produced.
    let media = nth_filepath(&output.stdout, request.item)
        .or_else(|| nth_filepath(&output.stdout, 1))
        .ok_or_else(|| {
            ApiError::bad_gateway(
                format!("yt-dlp did not produce item {}", request.item),
                hint,
            )
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

    let name = media
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(if request.mode.is_audio() {
            "audio"
        } else {
            "video"
        })
        .to_string();
    let mime = if request.mode.is_audio() {
        "audio/mp4".to_string()
    } else {
        mime_guess::from_path(&media)
            .first_or_octet_stream()
            .to_string()
    };

    Ok(DownloadedMedia {
        path: media,
        name,
        mime,
        size: metadata.len(),
        platform,
        hint,
        cleanup,
        permit,
    })
}

async fn info(config: &Config, request: InfoRequest) -> Result<Json<Value>, ApiError> {
    let InfoRequest {
        url,
        platform,
        cookies,
        timeout: request_timeout,
    } = request;
    let url = url.replace('\\', "");
    let cookies = cookies.filter(|cookies| !cookies.trim().is_empty());
    if cookies
        .as_deref()
        .is_some_and(|cookies| cookies.len() > MAX_COOKIE_BYTES)
    {
        return Err(ApiError::too_large(format!(
            "cookies exceed the {MAX_COOKIE_BYTES} byte limit"
        )));
    }
    let cookies = match cookies {
        Some(cookies) => Some(cookies),
        None => stored_cookies(config, platform).await?,
    };
    let has_cookies = cookies.is_some();
    let hint = has_cookies.then_some(COOKIE_HINT);
    let _permit = config.downloads.clone().try_acquire_owned().map_err(|_| {
        ApiError::too_many_requests(format!(
            "too many concurrent downloads (limit: {MAX_CONCURRENT})"
        ))
    })?;

    let dir = config.tmp.join(format!("ixian-{}", Uuid::new_v4()));
    create_private_dir(&dir)
        .map_err(|error| ApiError::internal(format!("cannot create temp directory: {error}")))?;
    let jar = dir.join("cookies.txt");
    let _cleanup = Cleanup::new(dir.clone());
    if let Some(cookies) = cookies.as_deref() {
        let contents = cookie_file(cookies, platform);
        if contents.len() > MAX_STORED_COOKIE_BYTES {
            return Err(ApiError::too_large(format!(
                "cookies exceed the {MAX_STORED_COOKIE_BYTES} byte limit"
            )));
        }
        write_cookie_jar(&jar, &contents)
            .map_err(|error| ApiError::internal(format!("cannot write cookie jar: {error}")))?;
    }

    let mut cmd = Command::new(&config.ytdlp);
    cmd.arg("-J")
        .arg("--no-playlist")
        .arg("--playlist-items")
        .arg("1")
        .arg("--no-progress")
        .arg("--socket-timeout")
        .arg(SOCKET_TIMEOUT_SECS.to_string())
        .arg("--retries")
        .arg(RETRIES.to_string());
    if let Some(plugin_dir) = &config.plugin_dir {
        cmd.arg("--plugin-dirs").arg(plugin_dir);
    }
    if has_cookies {
        cmd.arg("--cookies").arg(&jar);
    }
    // `--` terminates option parsing so a leading dash can never become a flag.
    cmd.arg("--").arg(&url);

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

async fn download(
    config: &Config,
    mut request: DownloadRequest,
    method: Method,
    headers: HeaderMap,
) -> ApiResult {
    request.url = request.url.replace('\\', "");
    let Some(cache) = &config.cache else {
        let media = fetch(config, &request).await?;
        return stream_media(media, request.inline).await;
    };

    // Stored cookies change the response, so they are part of the cache key too.
    let key = download_cache_key(config, &request).await?;
    let key_lock = cache.key_lock(&key);
    let _guard = key_lock.lock().await;

    if let Some(entry) = cache.lookup(&key).await {
        cache.hits.fetch_add(1, Ordering::Relaxed);
        return Ok(serve_entry(entry, &request, method, headers).await);
    }
    cache.misses.fetch_add(1, Ordering::Relaxed);

    let media = fetch(config, &request).await?;
    let Some(entry) = cache
        .insert(
            &key,
            &media.path,
            &media.name,
            &media.mime,
            media.size,
            request.platform,
        )
        .await
    else {
        return stream_media(media, request.inline).await;
    };
    drop(media);
    Ok(serve_entry(entry, &request, method, headers).await)
}

/// Cache key for a download, including cookies that were auto-loaded from the
/// store. Stored cookies change the response, so they partition the cache.
async fn download_cache_key(
    config: &Config,
    request: &DownloadRequest,
) -> Result<String, ApiError> {
    let effective_cookies = match request.cookies.as_deref().map(str::trim) {
        Some(cookies) if !cookies.is_empty() => request.cookies.clone(),
        _ => stored_cookies(config, request.platform).await?,
    };
    let format = request.format.as_deref().unwrap_or(&config.format);
    Ok(cache_key(
        request.platform,
        &request.url.replace('\\', ""),
        format,
        &request.cache_variant(),
        effective_cookies.as_deref(),
    ))
}

async fn stream_media(media: DownloadedMedia, inline: bool) -> ApiResult {
    let DownloadedMedia {
        path,
        name,
        mime,
        size,
        platform,
        hint,
        cleanup,
        permit,
    } = media;
    let file = fs::File::open(&path).await.map_err(|error| {
        ApiError::bad_gateway(format!("cannot read output file: {error}"), hint)
    })?;

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&size.to_string()).unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    add_media_headers(&mut headers, &name, platform.name(), inline);

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

async fn serve_entry(
    entry: CacheEntry,
    request: &DownloadRequest,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let CacheEntry { path, meta } = entry;
    serve_file(
        &path,
        &meta.name,
        &meta.content_type,
        &meta.platform,
        request.inline,
        method,
        headers,
    )
    .await
}

/// Serves a finished file with `Range`/`HEAD`/conditional support. Used for
/// both disk-cache hits and finished async jobs.
async fn serve_file(
    path: &Path,
    name: &str,
    mime: &str,
    platform: &str,
    inline: bool,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let method = if method == Method::GET || method == Method::HEAD {
        method
    } else {
        Method::GET
    };
    let mut builder = Request::builder().method(method).uri("/");
    for name in [
        header::RANGE.as_str(),
        header::IF_NONE_MATCH.as_str(),
        header::IF_MODIFIED_SINCE.as_str(),
        header::IF_MATCH.as_str(),
        header::IF_UNMODIFIED_SINCE.as_str(),
    ] {
        if let Some(value) = headers.get(name) {
            builder = builder.header(name, value);
        }
    }
    let request = match builder.body(Body::empty()) {
        Ok(request) => request,
        Err(error) => return ApiError::internal(error.to_string()).into_response(),
    };
    let mut response = match ServeFile::new(path).oneshot(request).await {
        Ok(response) => response.map(Body::new),
        Err(never) => match never {},
    };
    add_media_headers(response.headers_mut(), name, platform, inline);
    if response.status() != StatusCode::NOT_MODIFIED {
        if let Ok(value) = HeaderValue::from_str(mime) {
            response.headers_mut().insert(header::CONTENT_TYPE, value);
        }
    }
    response
}

fn add_media_headers(headers: &mut HeaderMap, name: &str, platform: &str, inline: bool) {
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private"));
    let kind = if inline { "inline" } else { "attachment" };
    let disposition = format!(
        "{kind}; filename*=UTF-8''{}",
        percent_encoding::utf8_percent_encode(name, FILENAME_ENCODE)
    );
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    if let Ok(value) = HeaderValue::from_str(platform) {
        headers.insert("x-ixian-platform", value);
    }
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

    fn from_api(error: ApiError, code: &'static str) -> Self {
        Self {
            error: error.message,
            code,
            hint: error.hint,
        }
    }
}

/// A finished download owned by a job. The file stays in the job's private
/// directory until `JOB_TTL_MS` expires.
struct JobMedia {
    path: PathBuf,
    name: String,
    mime: String,
    size: u64,
}

struct JobState {
    status: JobStatus,
    progress: f64,
    bytes: u64,
    total_bytes: Option<u64>,
    eta: Option<u64>,
    error: Option<JobFailure>,
    dir: Option<PathBuf>,
    media: Option<JobMedia>,
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
            media: None,
            finished_at: None,
        }
    }
}

struct Job {
    id: Uuid,
    request: DownloadRequest,
    state: StdMutex<JobState>,
    cancel: CancellationToken,
    finished: watch::Sender<bool>,
}

impl Job {
    fn new(id: Uuid, request: DownloadRequest) -> Self {
        let (finished, _) = watch::channel(false);
        Self {
            id,
            request,
            state: StdMutex::new(JobState::new()),
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

/// In-memory job registry. Jobs share the download semaphore with synchronous
/// requests, so `MAX_CONCURRENT` bounds both together; extra jobs wait in
/// `queue` instead of being rejected, up to `MAX_QUEUE`.
struct Jobs {
    inner: StdMutex<JobsInner>,
    max_queue: usize,
    ttl: Duration,
}

impl Jobs {
    fn new(config: &Config) -> Self {
        Self {
            inner: StdMutex::new(JobsInner {
                jobs: HashMap::new(),
                queue: VecDeque::new(),
            }),
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
            let mut error = ApiError::too_many_requests(format!(
                "job queue is full (MAX_QUEUE={})",
                self.max_queue
            ));
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
            permit = config.downloads.clone().acquire_owned() => match permit {
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
        let mut done_dir = None;
        let cleanup_dir = {
            let mut inner = self.lock_inner();
            inner.queue.retain(|queued| *queued != job.id);
            let mut state = job.lock_state();
            let cancelled = state.status == JobStatus::Cancelled;

            if cancelled || matches!(&outcome, JobOutcome::Cancelled) {
                state.status = JobStatus::Cancelled;
                state.eta = None;
                state.error = None;
                state.media = None;
                state.finished_at = Some(Instant::now());
                state.dir.take()
            } else {
                match outcome {
                    JobOutcome::Done(media) => {
                        state.status = JobStatus::Done;
                        state.progress = 100.0;
                        state.bytes = media.size;
                        state.total_bytes = Some(media.size);
                        state.eta = Some(0);
                        state.error = None;
                        state.media = Some(media);
                        state.finished_at = Some(Instant::now());
                        done_dir = state.dir.clone();
                        None
                    }
                    JobOutcome::Failed(failure) => {
                        state.status = JobStatus::Failed;
                        state.eta = None;
                        state.error = Some(failure);
                        state.media = None;
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
        if let Some(dir) = done_dir {
            // Keep the media for `JOB_TTL_MS`, but never leave the cookie jar behind.
            let keep = job
                .lock_state()
                .media
                .as_ref()
                .is_some_and(|media| media.name == "cookies.txt");
            if !keep {
                let _ = fs::remove_file(dir.join("cookies.txt")).await;
            }
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
                    state.media = None;
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
    Done(JobMedia),
    Failed(JobFailure),
    Cancelled,
}

struct ProgressUpdate {
    bytes: u64,
    total: Option<u64>,
    eta: Option<u64>,
    percent: Option<f64>,
}

/// Runs one job's yt-dlp process, reporting progress on the job and stopping it
/// on cancellation or timeout. The job owns its temp directory, so the media
/// survives `fetch`-style cleanup until the job expires.
async fn run_job(config: &Config, job: &Arc<Job>) -> JobOutcome {
    let request = &job.request;
    let dir = config.tmp.join(format!("ixian-{}", Uuid::new_v4()));
    if let Err(error) = create_private_dir(&dir) {
        return JobOutcome::Failed(JobFailure::new(
            format!("cannot create temp directory: {error}"),
            "internal",
            None,
        ));
    }
    job.lock_state().dir = Some(dir.clone());
    if job.cancel.is_cancelled() {
        return JobOutcome::Cancelled;
    }

    let prepared = match prepare_download(config, request, &dir).await {
        Ok(prepared) => prepared,
        Err(error) => return JobOutcome::Failed(JobFailure::from_api(error, "internal")),
    };
    let hint = prepared.hint;

    // A job reuses the disk cache like a synchronous download: a hit is
    // hard-linked into the job directory, a miss is stored after it completes.
    let cache_key = if config.cache.is_some() {
        match download_cache_key(config, request).await {
            Ok(key) => Some(key),
            Err(error) => return JobOutcome::Failed(JobFailure::from_api(error, "internal")),
        }
    } else {
        None
    };
    if let (Some(cache), Some(key)) = (&config.cache, cache_key.as_deref()) {
        if let Some(entry) = cache.lookup(key).await {
            cache.hits.fetch_add(1, Ordering::Relaxed);
            if let Some(media) = adopt_cache_entry(&entry, &dir).await {
                return JobOutcome::Done(media);
            }
        } else {
            cache.misses.fetch_add(1, Ordering::Relaxed);
        }
    }

    let path_file = dir.join("path.txt");
    let mut cmd = download_command(config, request, &prepared, &dir, true);
    cmd.arg("--print-to-file")
        .arg("after_move:filepath")
        .arg(&path_file);
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

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
    let mut group = ProcessGroup::new(child.id());

    let Some(mut stdout_pipe) = child.stdout.take() else {
        group.kill();
        return JobOutcome::Failed(JobFailure::new(
            "cannot capture yt-dlp stdout",
            "internal",
            hint,
        ));
    };
    let Some(stderr_pipe) = child.stderr.take() else {
        group.kill();
        return JobOutcome::Failed(JobFailure::new(
            "cannot capture yt-dlp stderr",
            "internal",
            hint,
        ));
    };

    let stdout_task = tokio::spawn(async move {
        let mut sink = String::new();
        stdout_pipe.read_to_string(&mut sink).await.map(|_| sink)
    });
    let progress_job = Arc::clone(job);
    let stderr_task = tokio::spawn(read_job_stderr(stderr_pipe, progress_job));

    let limit = request.timeout.unwrap_or(config.timeout);
    let status = tokio::select! {
        _ = job.cancel.cancelled() => {
            group.kill();
            if timeout(KILL_TIMEOUT, child.wait()).await.is_ok() {
                group.disarm();
            }
            stdout_task.abort();
            stderr_task.abort();
            return JobOutcome::Cancelled;
        }
        waited = timeout(limit, child.wait()) => match waited {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                group.kill();
                if timeout(KILL_TIMEOUT, child.wait()).await.is_ok() {
                    group.disarm();
                }
                stdout_task.abort();
                stderr_task.abort();
                return JobOutcome::Failed(JobFailure::new(
                    format!("failed to wait for yt-dlp: {error}"),
                    "internal",
                    hint,
                ));
            }
            Err(_) => {
                group.kill();
                if timeout(KILL_TIMEOUT, child.wait()).await.is_ok() {
                    group.disarm();
                }
                stdout_task.abort();
                stderr_task.abort();
                return JobOutcome::Failed(JobFailure::new(
                    format!("yt-dlp timed out after {}ms", limit.as_millis()),
                    "timeout",
                    hint,
                ));
            }
        }
    };

    // yt-dlp exited, but a grandchild may still hold the pipes open.
    let ((stderr, stderr_stalled), _stdout) =
        tokio::join!(drain_pipe(stderr_task), drain_pipe(stdout_task));
    if stderr_stalled {
        group.kill();
    }
    group.disarm();

    if job.cancel.is_cancelled() {
        return JobOutcome::Cancelled;
    }

    let stderr = stderr.unwrap_or_default();
    if !status.success() {
        let failure = ytdlp_failure(&stderr, status.code(), hint);
        return JobOutcome::Failed(JobFailure::from_api(failure, "download_failed"));
    }

    let produced = fs::read_to_string(&path_file).await.unwrap_or_default();
    let media = nth_filepath(&produced, request.item).or_else(|| nth_filepath(&produced, 1));
    let Some(path) = media else {
        return JobOutcome::Failed(JobFailure::new(
            format!("yt-dlp did not produce item {}", request.item),
            "no_output",
            hint,
        ));
    };
    let metadata = match fs::metadata(&path).await {
        Ok(metadata) => metadata,
        Err(_) => {
            return JobOutcome::Failed(JobFailure::new(
                "yt-dlp did not produce an output file",
                "no_output",
                hint,
            ));
        }
    };
    if !metadata.is_file() {
        return JobOutcome::Failed(JobFailure::new(
            "yt-dlp did not produce an output file",
            "no_output",
            hint,
        ));
    }
    if metadata.len() > config.max_filesize {
        return JobOutcome::Failed(JobFailure::new(
            format!(
                "output file is {} bytes, over the {} byte limit",
                metadata.len(),
                config.max_filesize
            ),
            "download_failed",
            hint,
        ));
    }

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(if request.mode.is_audio() {
            "audio"
        } else {
            "video"
        })
        .to_string();
    let mime = if request.mode.is_audio() {
        "audio/mp4".to_string()
    } else {
        mime_guess::from_path(&path)
            .first_or_octet_stream()
            .to_string()
    };

    if let (Some(cache), Some(key)) = (&config.cache, cache_key.as_deref()) {
        let _ = cache
            .insert(key, &path, &name, &mime, metadata.len(), request.platform)
            .await;
    }

    JobOutcome::Done(JobMedia {
        path,
        name,
        mime,
        size: metadata.len(),
    })
}

/// Reads progress lines from yt-dlp's stderr and keeps the remaining text for
/// error classification, bounded like synchronous downloads.
async fn read_job_stderr(pipe: tokio::process::ChildStderr, job: Arc<Job>) -> io::Result<String> {
    let mut lines = BufReader::new(pipe).lines();
    let mut text = String::new();
    while let Some(line) = lines.next_line().await? {
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
        } else if text.len() < MAX_STDERR_BYTES {
            text.push_str(line.trim_end());
            text.push('\n');
        }
    }
    Ok(truncate_to_bytes(text, MAX_STDERR_BYTES))
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

/// Hard-links (or copies) a cache hit into the job directory so the job keeps
/// its own file even if the cache entry is evicted before `JOB_TTL_MS`.
async fn adopt_cache_entry(entry: &CacheEntry, dir: &Path) -> Option<JobMedia> {
    let name = entry.meta.name.clone();
    if Path::new(&name).file_name().and_then(|file| file.to_str()) != Some(name.as_str()) {
        return None;
    }
    let target = dir.join(&name);
    if fs::hard_link(&entry.path, &target).await.is_err()
        && fs::copy(&entry.path, &target).await.is_err()
    {
        return None;
    }
    let metadata = fs::metadata(&target).await.ok()?;
    Some(JobMedia {
        path: target,
        name,
        mime: entry.meta.content_type.clone(),
        size: metadata.len(),
    })
}

fn parse_job_id(raw: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(raw).map_err(|_| ApiError::bad_request("invalid job id"))
}

fn job_not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "job not found")
}

async fn root() -> Json<Value> {
    Json(json!({
        "message": "Hello!",
        "platforms": Platform::ALL.map(Platform::name),
    }))
}

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    jobs: Arc<Jobs>,
}

fn download_request_from_query(uri: &Uri) -> Result<DownloadRequest, ApiError> {
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

    Ok(DownloadRequest {
        url,
        platform,
        cookies: None,
        format: params
            .get("format")
            .filter(|format| !format.is_empty())
            .cloned(),
        mode: Mode::parse(params.get("mode").map(String::as_str))?,
        quality: Quality::parse(params.get("quality").map(String::as_str))?,
        container: Container::parse(params.get("container").map(String::as_str))?,
        item: parse_item(params.get("item").map(String::as_str))?,
        timeout,
        inline: params.get("inline").is_some_and(|value| parse_bool(value)),
    })
}

fn download_request_from_json(body: &str) -> Result<DownloadRequest, ApiError> {
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

    Ok(DownloadRequest {
        url,
        platform,
        cookies,
        format: object
            .get("format")
            .and_then(Value::as_str)
            .filter(|format| !format.is_empty())
            .map(str::to_string),
        mode: Mode::parse(object.get("mode").and_then(Value::as_str))?,
        quality: Quality::parse(object.get("quality").and_then(Value::as_str))?,
        container: Container::parse(object.get("container").and_then(Value::as_str))?,
        item: item_from_json(object.get("item"))?,
        timeout,
        inline: parse_bool_value(object.get("inline")),
    })
}

async fn download_get(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    let request = download_request_from_query(&uri)?;
    download(&state.config, request, method, headers).await
}

async fn download_post(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    body: String,
) -> ApiResult {
    let request = download_request_from_json(&body)?;
    download(&state.config, request, method, headers).await
}

async fn jobs_post(State(state): State<AppState>, body: String) -> ApiResult {
    let request = download_request_from_json(&body)?;
    let (job, position) = state.jobs.enqueue(request)?;
    let response = json!({
        "id": job.id.to_string(),
        "status": job.lock_state().status.name(),
        "position": position,
    });
    state.jobs.start(Arc::clone(&state.config), job);
    Ok((StatusCode::ACCEPTED, Json(response)).into_response())
}

async fn job_get(State(state): State<AppState>, UrlPath(id): UrlPath<String>) -> ApiResult {
    let id = parse_job_id(&id)?;
    let snapshot = state.jobs.snapshot(id).ok_or_else(job_not_found)?;
    Ok(Json(snapshot).into_response())
}

async fn job_delete(State(state): State<AppState>, UrlPath(id): UrlPath<String>) -> ApiResult {
    let id = parse_job_id(&id)?;
    let snapshot = state.jobs.cancel(id).await?;
    Ok(Json(snapshot).into_response())
}

async fn job_file(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    method: Method,
    headers: HeaderMap,
) -> ApiResult {
    let id = parse_job_id(&id)?;
    let job = state.jobs.get(id).ok_or_else(job_not_found)?;
    let (status, media, inline) = {
        let state = job.lock_state();
        (
            state.status,
            state
                .media
                .as_ref()
                .map(|media| (media.path.clone(), media.name.clone(), media.mime.clone())),
            job.request.inline,
        )
    };
    if status != JobStatus::Done {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("job is {}", status.name()),
        ));
    }
    let Some((path, name, mime)) = media else {
        return Err(ApiError::internal("job file is missing"));
    };
    Ok(serve_file(
        &path,
        &name,
        &mime,
        job.request.platform.name(),
        inline,
        method,
        headers,
    )
    .await)
}

async fn info_get(State(state): State<AppState>, uri: Uri) -> Result<Json<Value>, ApiError> {
    let config = &state.config;
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
            "send cookies in a POST /info JSON body instead",
        ));
    }

    let url = params
        .get("url")
        .map(String::as_str)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| ApiError::bad_request("missing 'url' query parameter"))?;
    let (url, platform) = validate_url(url, params.get("platform").map(String::as_str))?;
    let timeout = query_timeout(&params)?;

    info(
        config,
        InfoRequest {
            url,
            platform,
            cookies: None,
            timeout,
        },
    )
    .await
}

async fn info_post(State(state): State<AppState>, body: String) -> Result<Json<Value>, ApiError> {
    let config = &state.config;
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

    info(
        config,
        InfoRequest {
            url,
            platform,
            cookies,
            timeout,
        },
    )
    .await
}

async fn cache_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let config = &state.config;
    authorize(config, &headers)?;
    match &config.cache {
        Some(cache) => Ok(Json(cache.stats().await).into_response()),
        None => Ok(Json(json!({ "enabled": false })).into_response()),
    }
}

async fn cache_clear(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let config = &state.config;
    authorize(config, &headers)?;
    match &config.cache {
        Some(cache) => {
            let (cleared, bytes) = cache.clear().await;
            Ok(Json(json!({
                "enabled": true,
                "cleared": cleared,
                "bytes": bytes,
            }))
            .into_response())
        }
        None => Ok(Json(json!({ "enabled": false, "cleared": 0 })).into_response()),
    }
}

fn cookie_store_dir(config: &Config) -> Result<&Path, ApiError> {
    config
        .cookie_dir
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("cookie store disabled; set IXIAN_COOKIE_DIR"))
}

async fn list_cookies(State(state): State<AppState>) -> Json<Value> {
    let config = &state.config;
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
    State(state): State<AppState>,
    UrlPath(platform): UrlPath<String>,
    headers: HeaderMap,
    body: String,
) -> ApiResult {
    let config = &state.config;
    authorize(config, &headers)?;
    let platform = parse_platform(&platform)?;
    let dir = cookie_store_dir(config)?;
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
    State(state): State<AppState>,
    UrlPath(platform): UrlPath<String>,
    headers: HeaderMap,
) -> ApiResult {
    let config = &state.config;
    authorize(config, &headers)?;
    let platform = parse_platform(&platform)?;
    let dir = cookie_store_dir(config)?;
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

fn app_with_jobs(config: Arc<Config>, jobs: Arc<Jobs>) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/download", get(download_get).post(download_post))
        .route("/jobs", post(jobs_post))
        .route("/jobs/{id}", get(job_get).delete(job_delete))
        .route("/jobs/{id}/file", get(job_file))
        .route("/info", get(info_get).post(info_post))
        .route("/cache/stats", get(cache_stats))
        .route("/cache", delete(cache_clear))
        .route("/cookies", get(list_cookies))
        .route("/cookies/{platform}", put(put_cookie).delete(delete_cookie))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
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
    fn platform_override_still_enforces_the_host_allowlist() {
        assert_eq!(
            validate_url("https://www.instagram.com/reel/abc/", Some("instagram"))
                .unwrap()
                .1,
            Platform::Instagram
        );
        assert_eq!(
            validate_url("https://vm.tiktok.com/ZMabc/", Some("tiktok"))
                .unwrap()
                .1,
            Platform::TikTok
        );
        assert!(validate_url("https://example.com/v", Some("vimeo")).is_err());
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
    #[test]
    fn normalizes_urls_for_cache_keys() {
        assert_eq!(
            normalized_url("https://youtu.be/abc?si=share"),
            "https://youtube.com/watch?v=abc"
        );
        assert_eq!(
            normalized_url("https://www.youtube.com/shorts/abc/?utm_source=x"),
            "https://youtube.com/watch?v=abc"
        );
        assert_eq!(
            normalized_url("https://twitter.com/user/status/1?s=20"),
            "https://x.com/user/status/1"
        );
        assert_eq!(
            normalized_url("https://www.instagram.com/reel/abc/?igsh=x"),
            "https://instagram.com/reel/abc"
        );
        assert_eq!(
            normalized_url("https://example.com/v?b=2&a=1"),
            "https://example.com/v?a=1&b=2"
        );
        assert_eq!(
            normalized_url("https://vm.tiktok.com/ZMabc/?is_from_webapp=1&sender_device=pc"),
            "https://tiktok.com/ZMabc"
        );
    }

    #[test]
    fn cache_keys_cover_cookies_format_and_variant() {
        let base = cache_key(
            Platform::YouTube,
            "https://youtu.be/abc",
            DEFAULT_FORMAT,
            "video:best:mp4:1",
            None,
        );
        assert_eq!(
            base,
            cache_key(
                Platform::YouTube,
                "https://www.youtube.com/watch?v=abc&utm_source=share",
                DEFAULT_FORMAT,
                "video:best:mp4:1",
                Some("")
            )
        );
        assert_ne!(
            base,
            cache_key(
                Platform::YouTube,
                "https://youtu.be/abc",
                DEFAULT_FORMAT,
                "video:best:mp4:1",
                Some("auth_token=secret")
            )
        );
        assert_ne!(
            base,
            cache_key(
                Platform::YouTube,
                "https://youtu.be/abc",
                "bv*+ba/b",
                "video:best:mp4:1",
                None
            )
        );
        for variant in [
            "audio:best:mp4:1",
            "video:720:mp4:1",
            "video:best:webm:1",
            "video:best:mp4:2",
        ] {
            assert_ne!(
                base,
                cache_key(
                    Platform::YouTube,
                    "https://youtu.be/abc",
                    DEFAULT_FORMAT,
                    variant,
                    None
                ),
                "{variant}"
            );
        }
    }

    #[test]
    fn parses_inline_flags() {
        assert!(parse_bool("1"));
        assert!(parse_bool("true"));
        assert!(parse_bool("yes"));
        assert!(!parse_bool("0"));
        assert!(!parse_bool("false"));
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
ext="mp4"
print_file=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    --cookies) jar="$2"; shift 2 ;;
    --merge-output-format) ext="$2"; shift 2 ;;
    --audio-format) ext="$2"; shift 2 ;;
    --print-to-file) print_file="$3"; shift 3 ;;
    *) shift ;;
  esac
done
printf 'run\n' >> "$LOG/runs.txt"
if [ -n "$jar" ]; then cp -p "$jar" "$LOG/jar.txt"; fi
{extra}
prefix="${{out%%\%*}}"
dir="${{prefix%/}}"
mode=$(stat -c %a "$dir" 2>/dev/null || stat -f %Lp "$dir")
printf '%s\n' "$mode" > "$LOG/dirmode.txt"
printf 'partial' > "${{prefix}}partial.part"
if [ -f "$LOG/items" ]; then
  n=$(cat "$LOG/items")
  i=1
  while [ "$i" -le "$n" ]; do
    printf 'item %s' "$i" > "${{prefix}}item${{i}}.${{ext}}"
    if [ -n "$print_file" ]; then printf '%s\n' "${{prefix}}item${{i}}.${{ext}}" >> "$print_file"; fi
    printf '%s\n' "${{prefix}}item${{i}}.${{ext}}"
    i=$((i+1))
  done
  exit 0
fi
printf 'fake video data' > "${{prefix}}fake.${{ext}}"
if [ -n "$print_file" ]; then printf '%s\n' "${{prefix}}fake.${{ext}}" > "$print_file"; fi
printf '%s\n' "${{prefix}}fake.${{ext}}"
"#
                ),
            )
            .unwrap();
            stdfs::set_permissions(&script, stdfs::Permissions::from_mode(0o755)).unwrap();
            Self { tmp, logs, script }
        }

        fn config(&self, timeout: Duration) -> Arc<Config> {
            self.config_full(
                timeout,
                timeout,
                None,
                None,
                None,
                None,
                MAX_CONCURRENT,
                MAX_FILESIZE,
            )
        }

        fn config_with_info_timeout(
            &self,
            timeout: Duration,
            info_timeout: Duration,
        ) -> Arc<Config> {
            self.config_full(
                timeout,
                info_timeout,
                None,
                None,
                None,
                None,
                MAX_CONCURRENT,
                MAX_FILESIZE,
            )
        }

        fn config_with_plugin_dir(
            &self,
            timeout: Duration,
            plugin_dir: Option<PathBuf>,
        ) -> Arc<Config> {
            self.config_full(
                timeout,
                timeout,
                plugin_dir,
                None,
                None,
                None,
                MAX_CONCURRENT,
                MAX_FILESIZE,
            )
        }

        fn config_with_cookie_store(
            &self,
            timeout: Duration,
            cookie_dir: PathBuf,
            token: Option<&str>,
        ) -> Arc<Config> {
            self.config_full(
                timeout,
                timeout,
                None,
                Some(cookie_dir),
                None,
                token.map(str::to_string),
                MAX_CONCURRENT,
                MAX_FILESIZE,
            )
        }

        fn config_with_limits(
            &self,
            plugin_dir: Option<PathBuf>,
            timeout: Duration,
            concurrency: usize,
            max_filesize: u64,
        ) -> Arc<Config> {
            self.config_full(
                timeout,
                timeout,
                plugin_dir,
                None,
                None,
                None,
                concurrency,
                max_filesize,
            )
        }

        fn config_with_cache(&self, cache: Cache) -> Arc<Config> {
            self.config_full(
                Duration::from_secs(30),
                Duration::from_secs(30),
                None,
                None,
                Some(cache),
                None,
                MAX_CONCURRENT,
                MAX_FILESIZE,
            )
        }

        fn config_with(
            &self,
            timeout: Duration,
            plugin_dir: Option<PathBuf>,
            cache: Option<Cache>,
            token: Option<String>,
        ) -> Arc<Config> {
            self.config_full(
                timeout,
                timeout,
                plugin_dir,
                None,
                cache,
                token,
                MAX_CONCURRENT,
                MAX_FILESIZE,
            )
        }

        #[allow(clippy::too_many_arguments)]
        fn config_full(
            &self,
            timeout: Duration,
            info_timeout: Duration,
            plugin_dir: Option<PathBuf>,
            cookie_dir: Option<PathBuf>,
            cache: Option<Cache>,
            token: Option<String>,
            concurrency: usize,
            max_filesize: u64,
        ) -> Arc<Config> {
            self.config_full_jobs(
                timeout,
                info_timeout,
                plugin_dir,
                cookie_dir,
                cache,
                token,
                concurrency,
                max_filesize,
                DEFAULT_MAX_QUEUE,
                Duration::from_millis(DEFAULT_JOB_TTL_MS),
            )
        }

        fn config_with_job_limits(
            &self,
            timeout: Duration,
            plugin_dir: Option<PathBuf>,
            concurrency: usize,
            max_queue: usize,
            job_ttl: Duration,
        ) -> Arc<Config> {
            self.config_full_jobs(
                timeout,
                timeout,
                plugin_dir,
                None,
                None,
                None,
                concurrency,
                MAX_FILESIZE,
                max_queue,
                job_ttl,
            )
        }

        #[allow(clippy::too_many_arguments)]
        fn config_full_jobs(
            &self,
            timeout: Duration,
            info_timeout: Duration,
            plugin_dir: Option<PathBuf>,
            cookie_dir: Option<PathBuf>,
            cache: Option<Cache>,
            token: Option<String>,
            concurrency: usize,
            max_filesize: u64,
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
                info_timeout,
                tmp: self.tmp.path().to_path_buf(),
                cache,
                cookie_dir,
                token,
                downloads: Arc::new(Semaphore::new(concurrency)),
                max_filesize,
                max_queue,
                job_ttl,
            })
        }

        fn runs(&self) -> usize {
            stdfs::read_to_string(self.logs.path().join("runs.txt"))
                .map(|runs| runs.lines().count())
                .unwrap_or(0)
        }
    }

    fn query(params: &[(&str, &str)]) -> String {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in params {
            serializer.append_pair(name, value);
        }
        format!("/download?{}", serializer.finish())
    }

    fn arg_value(args: &str, flag: &str) -> String {
        let lines: Vec<&str> = args.lines().collect();
        let index = lines
            .iter()
            .position(|arg| *arg == flag)
            .unwrap_or_else(|| panic!("missing {flag} in {args}"));
        lines[index + 1].to_string()
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
    async fn rejects_invalid_presets() {
        let harness = Harness::new("");
        let app = app(harness.config(Duration::from_secs(30)));

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
            assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "GET {name}");
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
            assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "POST {name}");
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

        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert!(started.elapsed() < Duration::from_secs(3));
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["code"], 504);
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

    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    fn disk_cache(dir: &TempDir, ttl: Option<Duration>, max_bytes: Option<u64>) -> Cache {
        Cache::new(dir.path().to_path_buf(), ttl, max_bytes)
    }

    #[tokio::test]
    async fn caches_downloads_and_normalizes_urls() {
        let harness = Harness::new("");
        let cache_dir = TempDir::new().unwrap();
        let cache = disk_cache(
            &cache_dir,
            Some(Duration::from_secs(3600)),
            Some(1024 * 1024),
        );
        let app = app(harness.config_with_cache(cache));

        let first = serde_json::json!({
            "url": "https://www.youtube.com/watch?v=abc&utm_source=share",
        })
        .to_string();
        let (status, headers, body) =
            request(app.clone(), Method::POST, "/download", Some(first)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"fake video data");
        assert_eq!(headers[header::CACHE_CONTROL], "private");
        assert!(headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment;"));

        let second = serde_json::json!({ "url": "https://youtu.be/abc" }).to_string();
        let (status, _, body) = request(app.clone(), Method::POST, "/download", Some(second)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"fake video data");
        assert_eq!(harness.runs(), 1, "second request should be a cache hit");

        let (status, headers, body) = request(
            app.clone(),
            Method::GET,
            "/download?url=https://youtu.be/abc&inline=1",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"fake video data");
        assert_eq!(headers[header::CACHE_CONTROL], "private");
        assert!(headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("inline;"));
        assert_eq!(harness.runs(), 1);

        let (status, _, body) = request(app, Method::GET, "/cache/stats", None).await;
        assert_eq!(status, StatusCode::OK);
        let stats: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(stats["entries"], 1);
        assert_eq!(stats["misses"], 1);
        assert_eq!(stats["hits"], 2);
    }

    #[tokio::test]
    async fn serves_range_head_and_conditional_requests_from_cache() {
        let harness = Harness::new("");
        let cache_dir = TempDir::new().unwrap();
        let cache = disk_cache(&cache_dir, None, Some(1024 * 1024));
        let app = app(harness.config_with_cache(cache));
        let uri = "/download?url=https://youtu.be/range";

        let (status, headers, body) = request_with_headers(
            app.clone(),
            Method::GET,
            uri,
            None,
            header_map(&[(header::RANGE.as_str(), "bytes=0-3")]),
        )
        .await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(body, b"fake");
        assert_eq!(headers[header::CONTENT_RANGE], "bytes 0-3/15");
        assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
        assert_eq!(harness.runs(), 1);

        let (status, headers, body) = request_with_headers(
            app.clone(),
            Method::GET,
            uri,
            None,
            header_map(&[(header::RANGE.as_str(), "bytes=5-9")]),
        )
        .await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(body, b"video");
        assert_eq!(headers[header::CONTENT_RANGE], "bytes 5-9/15");
        assert_eq!(harness.runs(), 1);

        let (status, headers, body) = request(app.clone(), Method::HEAD, uri, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.is_empty());
        assert_eq!(headers[header::CONTENT_LENGTH], "15");
        assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
        assert_eq!(harness.runs(), 1);
        let etag = headers[header::ETAG].to_str().unwrap().to_string();

        let (status, _, body) = request_with_headers(
            app,
            Method::GET,
            uri,
            None,
            header_map(&[(header::IF_NONE_MATCH.as_str(), etag.as_str())]),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert!(body.is_empty());
        assert_eq!(harness.runs(), 1);
    }

    #[tokio::test]
    async fn dedupes_concurrent_same_key_downloads() {
        let harness = Harness::new("sleep 0.4");
        let cache_dir = TempDir::new().unwrap();
        let cache = disk_cache(&cache_dir, None, Some(1024 * 1024));
        let app = app(harness.config_with_cache(cache));
        let payload = serde_json::json!({ "url": "https://youtu.be/concurrent" }).to_string();

        let (first, second) = tokio::join!(
            request(
                app.clone(),
                Method::POST,
                "/download",
                Some(payload.clone())
            ),
            request(app, Method::POST, "/download", Some(payload))
        );

        assert_eq!(first.0, StatusCode::OK);
        assert_eq!(second.0, StatusCode::OK);
        assert_eq!(first.2, b"fake video data");
        assert_eq!(second.2, b"fake video data");
        assert_eq!(
            harness.runs(),
            1,
            "same-key requests must share one yt-dlp run"
        );
    }

    #[tokio::test]
    async fn evicts_least_recently_used_entries() {
        let harness = Harness::new("");
        let cache_dir = TempDir::new().unwrap();
        let cache = disk_cache(&cache_dir, None, Some(20));
        let app = app(harness.config_with_cache(cache));

        for url in ["https://youtu.be/first", "https://youtu.be/second"] {
            let payload = serde_json::json!({ "url": url }).to_string();
            let (status, _, body) =
                request(app.clone(), Method::POST, "/download", Some(payload)).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, b"fake video data");
        }

        assert_eq!(harness.runs(), 2);
        let (status, _, body) = request(app, Method::GET, "/cache/stats", None).await;
        assert_eq!(status, StatusCode::OK);
        let stats: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(stats["entries"], 1);
        assert_eq!(stats["bytes"], 15);
        assert_eq!(stats["evictions"], 1);
        assert_eq!(
            stdfs::read_dir(cache_dir.path().join("objects"))
                .unwrap()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn protects_cache_admin_routes_and_clears_entries() {
        let harness = Harness::new("");
        let cache_dir = TempDir::new().unwrap();
        let cache = disk_cache(&cache_dir, None, Some(1024 * 1024));
        let app = app(harness.config_with(
            Duration::from_secs(30),
            None,
            Some(cache),
            Some("secret".into()),
        ));

        let payload = serde_json::json!({ "url": "https://youtu.be/protected" }).to_string();
        let (status, _, _) = request(app.clone(), Method::POST, "/download", Some(payload)).await;
        assert_eq!(status, StatusCode::OK);

        let (status, _, _) = request(app.clone(), Method::GET, "/cache/stats", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let auth = header_map(&[(header::AUTHORIZATION.as_str(), "Bearer secret")]);
        let (status, _, body) =
            request_with_headers(app.clone(), Method::GET, "/cache/stats", None, auth.clone())
                .await;
        assert_eq!(status, StatusCode::OK);
        let stats: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(stats["entries"], 1);

        let (status, _, _) = request(app.clone(), Method::DELETE, "/cache", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _, body) =
            request_with_headers(app.clone(), Method::DELETE, "/cache", None, auth.clone()).await;
        assert_eq!(status, StatusCode::OK);
        let cleared: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(cleared["cleared"], 1);

        let (status, _, body) =
            request_with_headers(app, Method::GET, "/cache/stats", None, auth).await;
        assert_eq!(status, StatusCode::OK);
        let stats: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(stats["entries"], 0);
        assert_eq!(harness.runs(), 1);
    }

    #[tokio::test]
    async fn stored_cookies_partition_the_cache() {
        let harness = Harness::new("");
        let cookies = TempDir::new().unwrap();
        let dir = cookies.path().join("store");
        stdfs::create_dir(&dir).unwrap();
        let cache_dir = TempDir::new().unwrap();
        let cache = disk_cache(&cache_dir, None, Some(1024 * 1024));
        let app = app(harness.config_full(
            Duration::from_secs(30),
            Duration::from_secs(30),
            None,
            Some(dir.clone()),
            Some(cache),
            None,
            MAX_CONCURRENT,
            MAX_FILESIZE,
        ));

        stdfs::write(
            dir.join("youtube.txt"),
            "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tSID\tone\n",
        )
        .unwrap();
        let payload = serde_json::json!({ "url": "https://youtu.be/stored" }).to_string();
        let (status, _, _) = request(
            app.clone(),
            Method::POST,
            "/download",
            Some(payload.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(harness.runs(), 1);

        stdfs::write(
            dir.join("youtube.txt"),
            "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tSID\ttwo\n",
        )
        .unwrap();
        let (status, _, _) = request(app, Method::POST, "/download", Some(payload)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            harness.runs(),
            2,
            "changed stored cookies must not reuse the cache entry"
        );
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
            None,
            header_map(&[(header::RANGE.as_str(), "bytes=5-9")]),
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
            None,
            header_map(&[(header::RANGE.as_str(), "bytes=-4")]),
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
            None,
            header_map(&[(header::RANGE.as_str(), "bytes=99-")]),
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
        let config = harness.config_with_job_limits(
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
        let config = harness.config_with_job_limits(
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

    #[tokio::test]
    async fn jobs_use_the_disk_cache() {
        let harness = Harness::new("");
        let cache_dir = TempDir::new().unwrap();
        let cache = disk_cache(&cache_dir, None, Some(1024 * 1024));
        let app = app(harness.config_with_cache(cache));
        let payload = serde_json::json!({ "url": "https://youtu.be/job-cache" }).to_string();

        let (status, _, body) =
            request(app.clone(), Method::POST, "/jobs", Some(payload.clone())).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let created: Value = serde_json::from_slice(&body).unwrap();
        let id = created["id"].as_str().unwrap().to_string();
        wait_for_job(&app, &id, |job| job["status"] == "done").await;
        assert_eq!(harness.runs(), 1);

        let (status, _, body) = request(app, Method::POST, "/download", Some(payload)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"fake video data");
        assert_eq!(
            harness.runs(),
            1,
            "the job result should populate the cache"
        );
    }
}
