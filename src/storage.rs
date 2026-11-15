//! Source fetching (HTTP + S3) and the S3-backed result cache.

use std::time::Duration;

use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation};
use aws_sdk_s3::error::DisplayErrorContext;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use bytes::{Bytes, BytesMut};

use crate::config::{Config, S3Params};
use crate::error::AppError;
use crate::options::Format;

/// `retries: false` is for the result cache, where a failed lookup must cost
/// one round trip, not three: it is simply treated as a miss.
pub async fn build_s3_client(p: &S3Params, retries: bool) -> Client {
    let mut loader = aws_config::defaults(BehaviorVersion::latest());

    let env_region = std::env::var("AWS_REGION").or_else(|_| std::env::var("AWS_DEFAULT_REGION")).ok();
    match (&p.region, env_region) {
        (Some(r), _) => loader = loader.region(Region::new(r.clone())),
        // S3-compatible stores do not care, but the SDK refuses to sign without one.
        (None, None) => loader = loader.region(Region::new("us-east-1")),
        (None, Some(_)) => {}
    }
    if let Some(endpoint) = &p.endpoint {
        loader = loader.endpoint_url(endpoint.clone());
    }
    if let (Some(ak), Some(sk)) = (&p.access_key, &p.secret_key) {
        loader = loader.credentials_provider(Credentials::new(ak.clone(), sk.clone(), None, None, "imgrust"));
    }
    let shared = loader.load().await;

    let mut builder = aws_sdk_s3::config::Builder::from(&shared);
    if !retries {
        builder = builder.retry_config(aws_sdk_s3::config::retry::RetryConfig::disabled());
    }
    let conf = builder
        // Custom endpoints (MinIO, R2, Spaces...) generally want path-style.
        .force_path_style(p.force_path_style.unwrap_or(p.endpoint.is_some()))
        // Optional checksums break several S3-compatible providers.
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
        .build();
    Client::from_conf(conf)
}

pub struct Fetcher {
    http: reqwest::Client,
    s3: Client,
    max_bytes: usize,
}

impl Fetcher {
    pub fn new(cfg: &Config, s3: Client) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("imgrust/", env!("CARGO_PKG_VERSION")))
            .timeout(cfg.download_timeout)
            .connect_timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(64)
            .pool_idle_timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("failed to build HTTP client");
        Fetcher { http, s3, max_bytes: cfg.max_src_bytes }
    }

    pub async fn fetch(&self, url: &str) -> Result<Bytes, AppError> {
        match url.strip_prefix("s3://") {
            Some(rest) => self.fetch_s3(rest).await,
            None => self.fetch_http(url).await,
        }
    }

    fn too_large(&self) -> AppError {
        AppError::unprocessable(format!("source image is larger than {} MB", self.max_bytes / 1024 / 1024))
    }

    async fn fetch_http(&self, url: &str) -> Result<Bytes, AppError> {
        let mut resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| AppError::bad_gateway(format!("failed to fetch source: {}", strip_url(&e))))?;

        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(AppError::not_found("source image not found"));
        }
        if !status.is_success() {
            return Err(AppError::bad_gateway(format!("source responded with {}", status.as_u16())));
        }
        if resp.content_length().is_some_and(|l| l > self.max_bytes as u64) {
            return Err(self.too_large());
        }

        let mut buf = BytesMut::with_capacity(resp.content_length().unwrap_or(64 * 1024) as usize);
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| AppError::bad_gateway(format!("failed to read source: {}", strip_url(&e))))?
        {
            if buf.len() + chunk.len() > self.max_bytes {
                return Err(self.too_large());
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf.freeze())
    }

    /// True only when the bucket listing positively says the key is absent.
    async fn is_missing(&self, bucket: &str, key: &str) -> bool {
        match self.s3.list_objects_v2().bucket(bucket).prefix(key).max_keys(1).send().await {
            Ok(out) => !out.contents().iter().any(|o| o.key() == Some(key)),
            Err(_) => false,
        }
    }

    async fn fetch_s3(&self, rest: &str) -> Result<Bytes, AppError> {
        let (bucket, key) = rest
            .split_once('/')
            .filter(|(b, k)| !b.is_empty() && !k.is_empty())
            .ok_or_else(|| AppError::bad_request("S3 source must look like s3://bucket/key"))?;

        let out = match self.s3.get_object().bucket(bucket).key(key).send().await {
            Ok(o) => o,
            Err(e) => {
                let detail = brief(&format!("{}", DisplayErrorContext(&e)));
                let svc = e.into_service_error();
                if svc.is_no_such_key() {
                    return Err(AppError::not_found("source image not found"));
                }
                // Some S3 front ends answer a missing object with a bare 5xx
                // page instead of NoSuchKey. Ask the listing API, which does
                // answer properly, before calling it an outage.
                if aws_sdk_s3::error::ProvideErrorMetadata::code(&svc).is_none() && self.is_missing(bucket, key).await {
                    tracing::debug!(bucket, key, "S3 source does not exist");
                    return Err(AppError::not_found("source image not found"));
                }
                tracing::warn!(bucket, key, error = %detail, "S3 source fetch failed");
                return Err(AppError::bad_gateway("failed to fetch source from S3"));
            }
        };
        if out.content_length().is_some_and(|l| l as u64 > self.max_bytes as u64) {
            return Err(self.too_large());
        }
        let data = out
            .body
            .collect()
            .await
            .map_err(|e| AppError::bad_gateway(format!("failed to read source from S3: {e}")))?
            .into_bytes();
        if data.len() > self.max_bytes {
            return Err(self.too_large());
        }
        Ok(data)
    }
}

/// reqwest errors embed the full URL, which may carry tokens; drop it.
fn strip_url(e: &reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connection failed"
    } else if e.is_redirect() {
        "too many redirects"
    } else {
        "request failed"
    };
    kind.to_string()
}

/// Processed variants persisted in S3 so they survive restarts and are shared
/// between replicas.
#[derive(Clone)]
pub struct S3Cache {
    client: Client,
    bucket: String,
    prefix: String,
    cache_control: String,
}

impl S3Cache {
    pub fn new(client: Client, bucket: String, prefix: String, ttl: u64) -> Self {
        S3Cache { client, bucket, prefix, cache_control: format!("public, max-age={ttl}") }
    }

    /// Object name for a variant identified by its hash key.
    pub fn hashed_key(&self, key: &str) -> String {
        // Two levels of fan-out keep listings manageable.
        format!("{}{}/{}/{}", self.prefix, &key[0..2], &key[2..4], key)
    }

    pub async fn get(&self, object: &str) -> Result<Option<(Bytes, Format)>, String> {
        let out = match self.client.get_object().bucket(&self.bucket).key(object).send().await {
            Ok(o) => o,
            Err(e) => {
                let detail = brief(&format!("{}", DisplayErrorContext(&e)));
                let svc = e.into_service_error();
                if svc.is_no_such_key() {
                    return Ok(None);
                }
                // Some S3 front ends answer a missing object with a bare 5xx
                // page instead of NoSuchKey. No S3 error code = plain miss.
                if aws_sdk_s3::error::ProvideErrorMetadata::code(&svc).is_none() {
                    tracing::debug!(object, error = %detail, "S3 cache lookup failed without an S3 error code");
                    return Ok(None);
                }
                return Err(detail);
            }
        };
        let declared = out.content_type().and_then(Format::from_mime);
        let data = out.body.collect().await.map_err(|e| e.to_string())?.into_bytes();
        // Objects written by other tools may lack a usable Content-Type.
        match declared.or_else(|| crate::vips::sniff_output_format(&data)) {
            Some(format) if !data.is_empty() => Ok(Some((data, format))),
            _ => Ok(None),
        }
    }

    pub async fn put(&self, object: &str, data: Bytes, format: Format) -> Result<(), String> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(object)
            .content_type(format.mime())
            .cache_control(&self.cache_control)
            .body(ByteStream::from(data))
            .send()
            .await
            .map(|_| ())
            .map_err(|e| brief(&format!("{}", DisplayErrorContext(&e))))
    }
}

/// SDK errors print the whole HTTP exchange; keep log lines readable.
fn brief(s: &str) -> String {
    s.chars().take(240).collect()
}
