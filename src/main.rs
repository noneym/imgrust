mod config;
mod error;
mod options;
mod storage;
mod vips;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use tokio::sync::Semaphore;

use config::Config;
use error::AppError;
use options::{Format, ImageRequest, Negotiated};
use storage::{Fetcher, S3Cache};

/// One finished variant, shared between all requests that want it.
struct Entry {
    data: Bytes,
    format: Format,
}

#[derive(Default)]
struct Metrics {
    requests: AtomicU64,
    errors: AtomicU64,
    not_modified: AtomicU64,
    mem_hits: AtomicU64,
    s3_hits: AtomicU64,
    misses: AtomicU64,
    processed_us: AtomicU64,
    bytes_out: AtomicU64,
}

struct App {
    cfg: Config,
    fetcher: Fetcher,
    s3_cache: Option<S3Cache>,
    mem: moka::future::Cache<String, Arc<Entry>>,
    workers: Arc<Semaphore>,
    metrics: Metrics,
}

const ORIGIN_MEMORY: u8 = 0;
const ORIGIN_S3: u8 = 1;
const ORIGIN_MISS: u8 = 2;

fn main() {
    if std::env::args().nth(1).as_deref() == Some("health") {
        std::process::exit(healthcheck());
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start runtime")
        .block_on(run());
}

async fn run() {
    let level = std::env::var("IMGRUST_LOG_LEVEL")
        .ok()
        .and_then(|l| l.parse::<tracing::Level>().ok())
        .unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt().with_max_level(level).with_ansi(false).init();

    let cfg = Config::from_env();
    vips::init(cfg.vips_concurrency);

    let source_client = storage::build_s3_client(&cfg.s3, true).await;
    let s3_cache = match &cfg.cache_bucket {
        Some(bucket) => {
            let client = storage::build_s3_client(&cfg.cache_s3, false).await;
            Some(S3Cache::new(client, bucket.clone(), cfg.cache_prefix.clone(), cfg.ttl))
        }
        None => None,
    };

    let mem = moka::future::Cache::builder()
        .max_capacity(cfg.mem_cache_bytes)
        .weigher(|k: &String, v: &Arc<Entry>| (k.len() + v.data.len()).min(u32::MAX as usize) as u32)
        .build();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        bind = %cfg.bind,
        workers = cfg.workers,
        signed_urls = !(cfg.key.is_empty() && cfg.thumbor_key.is_empty()),
        s3_cache = cfg.cache_bucket.as_deref().unwrap_or("disabled"),
        mem_cache_mb = cfg.mem_cache_bytes / 1024 / 1024,
        "imgrust starting"
    );
    if cfg.key.is_empty() && cfg.thumbor_key.is_empty() {
        tracing::warn!("no signing key is set: URLs are not signed and anyone can request any transformation");
    }

    let app = Arc::new(App {
        fetcher: Fetcher::new(&cfg, source_client),
        s3_cache,
        mem,
        workers: Arc::new(Semaphore::new(cfg.workers)),
        metrics: Metrics::default(),
        cfg,
    });

    let router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/version", get(|| async { env!("CARGO_PKG_VERSION") }))
        .route("/metrics", get(metrics))
        .fallback(image)
        .with_state(app.clone());

    let listener = tokio::net::TcpListener::bind(&app.cfg.bind).await.expect("failed to bind");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    tracing::info!("shutting down");
}

/// `imgrust health`: used as the container HEALTHCHECK so the image needs no curl.
fn healthcheck() -> i32 {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let bind = std::env::var("IMGRUST_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let port = bind.rsplit(':').next().unwrap_or("8080");
    let Ok(addr) = format!("127.0.0.1:{port}").parse() else { return 1 };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else { return 1 };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    if stream.write_all(b"GET /health HTTP/1.0\r\n\r\n").is_err() {
        return 1;
    }
    let mut buf = [0u8; 16];
    match stream.read(&mut buf) {
        Ok(n) if buf[..n].windows(3).any(|w| w == b"200") => 0,
        _ => 1,
    }
}

async fn metrics(State(app): State<Arc<App>>) -> impl IntoResponse {
    let m = &app.metrics;
    let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let body = format!(
        "# TYPE imgrust_requests_total counter\nimgrust_requests_total {}\n\
         # TYPE imgrust_errors_total counter\nimgrust_errors_total {}\n\
         # TYPE imgrust_not_modified_total counter\nimgrust_not_modified_total {}\n\
         # TYPE imgrust_cache_hits_total counter\nimgrust_cache_hits_total{{layer=\"memory\"}} {}\nimgrust_cache_hits_total{{layer=\"s3\"}} {}\n\
         # TYPE imgrust_cache_misses_total counter\nimgrust_cache_misses_total {}\n\
         # TYPE imgrust_processing_seconds_total counter\nimgrust_processing_seconds_total {:.6}\n\
         # TYPE imgrust_response_bytes_total counter\nimgrust_response_bytes_total {}\n\
         # TYPE imgrust_memory_cache_bytes gauge\nimgrust_memory_cache_bytes {}\n\
         # TYPE imgrust_workers_available gauge\nimgrust_workers_available {}\n",
        g(&m.requests),
        g(&m.errors),
        g(&m.not_modified),
        g(&m.mem_hits),
        g(&m.s3_hits),
        g(&m.misses),
        g(&m.processed_us) as f64 / 1e6,
        g(&m.bytes_out),
        app.mem.weighted_size(),
        app.workers.available_permits(),
    );
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body)
}

async fn image(State(app): State<Arc<App>>, method: Method, uri: Uri, headers: HeaderMap) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    match uri.path() {
        "/" => return concat!("imgrust ", env!("CARGO_PKG_VERSION"), "\nhttps://imgrust.com\n").into_response(),
        "/favicon.ico" | "/robots.txt" => return StatusCode::NOT_FOUND.into_response(),
        _ => {}
    }

    app.metrics.requests.fetch_add(1, Ordering::Relaxed);
    match serve(&app, &method, &uri, &headers).await {
        Ok(res) => res,
        Err(e) => {
            app.metrics.errors.fetch_add(1, Ordering::Relaxed);
            if e.status.is_server_error() {
                tracing::warn!(path = uri.path(), status = e.status.as_u16(), error = %e.msg, "request failed");
            } else {
                tracing::debug!(path = uri.path(), status = e.status.as_u16(), error = %e.msg, "request rejected");
            }
            e.into_response()
        }
    }
}

async fn serve(app: &Arc<App>, method: &Method, uri: &Uri, headers: &HeaderMap) -> Result<Response, AppError> {
    let req = options::parse(uri.path(), &app.cfg)?;

    // Format negotiation only matters when the URL does not pin a format.
    let auto = req.options.format.is_none();
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).unwrap_or("");
    let thumbor_cache = req.thumbor_path.is_some() && app.cfg.thumbor_cache_root.is_some();
    let neg = if thumbor_cache {
        // Sharing thumbor's result storage: follow its AUTO_WEBP rule only.
        Negotiated { webp: auto && app.cfg.thumbor_auto_webp && accept.contains("image/webp"), avif: false }
    } else {
        Negotiated {
            webp: auto && app.cfg.auto_webp && accept.contains("image/webp"),
            avif: auto && app.cfg.auto_avif && accept.contains("image/avif"),
        }
    };

    let key = req.cache_key(neg, &app.cfg);
    let etag = format!("\"{}\"", &key[..32]);

    // Where this variant lives in the S3 cache.
    let object = app.s3_cache.as_ref().map(|c| match (&req.thumbor_path, &app.cfg.thumbor_cache_root) {
        (Some(path), Some(root)) => format!("{root}{}{path}", if neg.webp { "auto_webp/" } else { "" }),
        _ => c.hashed_key(&key),
    });

    // The key covers source URL, options, negotiated format and encoder
    // settings, so a matching ETag can be answered without touching any cache.
    if let Some(inm) = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) {
        if inm.split(',').any(|t| t.trim().trim_start_matches("W/") == etag) {
            app.metrics.not_modified.fetch_add(1, Ordering::Relaxed);
            let mut res = StatusCode::NOT_MODIFIED.into_response();
            common_headers(res.headers_mut(), app, &etag, auto);
            return Ok(res);
        }
    }

    // try_get_with coalesces concurrent requests for the same variant into a
    // single computation (no thundering herd on a cold cache). The work runs
    // in its own task so a client disconnect does not throw it away.
    let origin = Arc::new(AtomicU8::new(ORIGIN_MEMORY));
    let entry = {
        let (app2, origin2) = (app.clone(), origin.clone());
        app.mem
            .try_get_with(key, async move {
                tokio::spawn(async move { produce(app2, object, req, neg, origin2).await })
                    .await
                    .unwrap_or_else(|e| Err(AppError::internal(format!("processing task failed: {e}"))))
            })
            .await
            .map_err(|e| (*e).clone())?
    };

    let (label, counter) = match origin.load(Ordering::Relaxed) {
        ORIGIN_S3 => ("HIT-S3", &app.metrics.s3_hits),
        ORIGIN_MISS => ("MISS", &app.metrics.misses),
        _ => ("HIT", &app.metrics.mem_hits),
    };
    counter.fetch_add(1, Ordering::Relaxed);

    let len = entry.data.len();
    let body = if method == Method::HEAD { Body::empty() } else { Body::from(entry.data.clone()) };
    let mut res = Response::new(body);
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(entry.format.mime()));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert("x-cache", HeaderValue::from_static(label));
    common_headers(h, app, &etag, auto);
    if method != Method::HEAD {
        app.metrics.bytes_out.fetch_add(len as u64, Ordering::Relaxed);
    }
    Ok(res)
}

fn common_headers(h: &mut HeaderMap, app: &App, etag: &str, auto: bool) {
    if let Ok(v) = HeaderValue::from_str(&format!("public, max-age={}", app.cfg.ttl)) {
        h.insert(header::CACHE_CONTROL, v);
    }
    if let Ok(v) = HeaderValue::from_str(etag) {
        h.insert(header::ETAG, v);
    }
    if auto && (app.cfg.auto_webp || app.cfg.auto_avif) {
        h.insert(header::VARY, HeaderValue::from_static("Accept"));
    }
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert(header::SERVER, HeaderValue::from_static(concat!("imgrust/", env!("CARGO_PKG_VERSION"))));
}

/// Cold path: S3 cache lookup, then fetch + process + write back.
async fn produce(
    app: Arc<App>,
    object: Option<String>,
    req: ImageRequest,
    neg: Negotiated,
    origin: Arc<AtomicU8>,
) -> Result<Arc<Entry>, AppError> {
    if let (Some(cache), Some(object)) = (&app.s3_cache, &object) {
        match cache.get(object).await {
            Ok(Some((data, format))) => {
                origin.store(ORIGIN_S3, Ordering::Relaxed);
                return Ok(Arc::new(Entry { data, format }));
            }
            Ok(None) => {}
            // A broken cache must never take the service down with it.
            Err(e) => tracing::warn!(error = %e, "S3 cache read failed"),
        }
    }
    origin.store(ORIGIN_MISS, Ordering::Relaxed);

    let source = app.fetcher.fetch(&req.source).await?;

    // Bounded CPU concurrency with a bounded wait: under overload we shed
    // requests with 503 instead of piling up memory.
    let permit = tokio::time::timeout(app.cfg.queue_timeout, app.workers.clone().acquire_owned())
        .await
        .map_err(|_| AppError::unavailable("server is busy, try again"))?
        .map_err(|_| AppError::unavailable("server is shutting down"))?;

    let started = Instant::now();
    let app2 = app.clone();
    let options = req.options.clone();
    let out = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        vips::process(&source, &options, neg, &app2.cfg)
    })
    .await
    .map_err(|e| AppError::internal(format!("processing task failed: {e}")))?
    .map_err(|e| match e {
        vips::ProcError::Unsupported => {
            AppError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "source is not a supported image format")
        }
        vips::ProcError::TooLarge(msg) => AppError::unprocessable(msg),
        vips::ProcError::Vips(msg) => {
            tracing::warn!(source = %req.source, error = %msg, "image processing failed");
            AppError::unprocessable("failed to process image")
        }
    })?;
    app.metrics.processed_us.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);

    if let (Some(cache), Some(object)) = (app.s3_cache.clone(), object) {
        let (data, format) = (out.data.clone(), out.format);
        // Write-behind: the client never waits for the cache upload.
        tokio::spawn(async move {
            if let Err(e) = cache.put(&object, data, format).await {
                tracing::warn!(error = %e, "S3 cache write failed");
            }
        });
    }

    Ok(Arc::new(Entry { data: out.data, format: out.format }))
}
