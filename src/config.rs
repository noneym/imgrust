use std::env;
use std::time::Duration;

/// Connection settings for one S3 (or S3-compatible) endpoint.
#[derive(Debug, Clone, Default)]
pub struct S3Params {
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub force_path_style: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: String,

    // URL signing (imgproxy compatible). Empty key = signature not checked.
    pub key: Vec<u8>,
    pub salt: Vec<u8>,
    /// thumbor-style URLs: plain-text security key and base for relative paths.
    pub thumbor_key: Vec<u8>,
    pub thumbor_base_url: String,
    /// When set, thumbor-style results use thumbor_aws' result-storage layout
    /// (`{root}/[auto_webp/]{request path}`) so both can share one cache.
    pub thumbor_cache_root: Option<String>,
    pub thumbor_auto_webp: bool,
    /// Like thumbor's ALLOW_UNSAFE_URL: accept unsigned URLs even when a key is set.
    pub allow_unsafe: bool,
    /// Answer unsigned URLs (`/insecure/...`, `/unsafe/...`) with a 301 to their
    /// signed form instead of rejecting them.
    pub unsafe_redirect: bool,
    /// When non-empty, only these option strings (the part between the
    /// signature and the source) are eligible for that redirect.
    pub unsafe_redirect_options: Vec<String>,

    pub base_url: String,
    pub allowed_sources: Vec<String>,

    // Concurrency
    pub workers: usize,
    pub vips_concurrency: i32,
    pub queue_timeout: Duration,
    pub download_timeout: Duration,

    // Limits
    pub max_src_bytes: usize,
    pub max_src_pixels: u64,
    pub max_animation_frames: u32,
    pub max_animation_pixels: u64,
    pub max_dimension: u32,

    // Encoding
    pub quality: u8,
    pub avif_quality: u8,
    pub webp_effort: i32,
    pub avif_effort: i32,
    pub auto_webp: bool,
    pub auto_avif: bool,

    // Caching
    pub ttl: u64,
    pub mem_cache_bytes: u64,
    pub cache_bucket: Option<String>,
    pub cache_prefix: String,

    pub s3: S3Params,
    pub cache_s3: S3Params,
}

fn var(name: &str) -> Option<String> {
    env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn num<T: std::str::FromStr>(name: &str, default: T) -> T {
    match var(name) {
        Some(v) => v.parse().unwrap_or_else(|_| panic!("invalid value for {name}: {v}")),
        None => default,
    }
}

fn flag(name: &str) -> Option<bool> {
    var(name).map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

fn hex_var(name: &str) -> Vec<u8> {
    match var(name) {
        Some(v) => hex::decode(&v).unwrap_or_else(|_| panic!("{name} must be hex encoded")),
        None => Vec::new(),
    }
}

impl Config {
    pub fn from_env() -> Self {
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);

        let s3 = S3Params {
            endpoint: var("IMGRUST_S3_ENDPOINT"),
            region: var("IMGRUST_S3_REGION"),
            access_key: var("IMGRUST_S3_ACCESS_KEY"),
            secret_key: var("IMGRUST_S3_SECRET_KEY"),
            force_path_style: flag("IMGRUST_S3_FORCE_PATH_STYLE"),
        };
        // The cache bucket may live on a different provider; every field falls
        // back to the source settings when not overridden.
        let cache_s3 = S3Params {
            endpoint: var("IMGRUST_CACHE_S3_ENDPOINT").or_else(|| s3.endpoint.clone()),
            region: var("IMGRUST_CACHE_S3_REGION").or_else(|| s3.region.clone()),
            access_key: var("IMGRUST_CACHE_S3_ACCESS_KEY").or_else(|| s3.access_key.clone()),
            secret_key: var("IMGRUST_CACHE_S3_SECRET_KEY").or_else(|| s3.secret_key.clone()),
            force_path_style: flag("IMGRUST_CACHE_S3_FORCE_PATH_STYLE").or(s3.force_path_style),
        };

        let mut cache_prefix = var("IMGRUST_CACHE_S3_PREFIX").unwrap_or_default();
        cache_prefix = cache_prefix.trim_start_matches('/').to_string();
        if !cache_prefix.is_empty() && !cache_prefix.ends_with('/') {
            cache_prefix.push('/');
        }

        let key = hex_var("IMGRUST_KEY");
        let salt = hex_var("IMGRUST_SALT");

        Config {
            bind: var("IMGRUST_BIND").unwrap_or_else(|| "0.0.0.0:8080".into()),
            key,
            salt,
            thumbor_key: var("IMGRUST_THUMBOR_KEY").map(String::into_bytes).unwrap_or_default(),
            thumbor_base_url: var("IMGRUST_THUMBOR_BASE_URL").unwrap_or_default(),
            thumbor_cache_root: var("IMGRUST_THUMBOR_CACHE_ROOT").map(|r| format!("{}/", r.trim_matches('/'))),
            thumbor_auto_webp: flag("IMGRUST_THUMBOR_AUTO_WEBP").unwrap_or(false),
            allow_unsafe: flag("IMGRUST_ALLOW_UNSAFE_URL").unwrap_or(false),
            unsafe_redirect: flag("IMGRUST_UNSAFE_REDIRECT").unwrap_or(false),
            unsafe_redirect_options: var("IMGRUST_UNSAFE_REDIRECT_OPTIONS")
                .map(|v| v.split(',').map(|s| s.trim().trim_matches('/').to_string()).filter(|s| !s.is_empty()).collect())
                .unwrap_or_default(),
            base_url: var("IMGRUST_BASE_URL").unwrap_or_default(),
            allowed_sources: var("IMGRUST_ALLOWED_SOURCES")
                .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
                .unwrap_or_default(),
            workers: num("IMGRUST_WORKERS", cpus).max(1),
            vips_concurrency: num("IMGRUST_VIPS_CONCURRENCY", 1).max(1),
            queue_timeout: Duration::from_millis(num("IMGRUST_QUEUE_TIMEOUT_MS", 10_000)),
            download_timeout: Duration::from_millis(num("IMGRUST_DOWNLOAD_TIMEOUT_MS", 10_000)),
            max_src_bytes: num::<usize>("IMGRUST_MAX_SRC_MB", 50) * 1024 * 1024,
            max_src_pixels: (num::<f64>("IMGRUST_MAX_SRC_RESOLUTION", 50.0) * 1e6) as u64,
            max_animation_frames: num("IMGRUST_MAX_ANIMATION_FRAMES", 300).max(1),
            max_animation_pixels: (num::<f64>("IMGRUST_MAX_ANIMATION_RESOLUTION", 200.0) * 1e6) as u64,
            max_dimension: num("IMGRUST_MAX_DIMENSION", 8192),
            quality: num::<u8>("IMGRUST_QUALITY", 80).clamp(1, 100),
            avif_quality: num::<u8>("IMGRUST_AVIF_QUALITY", 60).clamp(1, 100),
            webp_effort: num::<i32>("IMGRUST_WEBP_EFFORT", 4).clamp(0, 6),
            avif_effort: num::<i32>("IMGRUST_AVIF_EFFORT", 3).clamp(0, 9),
            auto_webp: flag("IMGRUST_AUTO_WEBP").unwrap_or(true),
            auto_avif: flag("IMGRUST_AUTO_AVIF").unwrap_or(false),
            ttl: num("IMGRUST_TTL", 31_536_000),
            mem_cache_bytes: num::<u64>("IMGRUST_MEM_CACHE_MB", 256) * 1024 * 1024,
            cache_bucket: var("IMGRUST_CACHE_S3_BUCKET"),
            cache_prefix,
            s3,
            cache_s3,
        }
    }

    /// Settings that change the bytes we produce; mixed into every cache key
    /// so a config change never serves stale variants.
    pub fn fingerprint(&self) -> String {
        format!(
            "v1|q{}|aq{}|we{}|ae{}|mf{}",
            self.quality, self.avif_quality, self.webp_effort, self.avif_effort, self.max_animation_frames
        )
    }
}
