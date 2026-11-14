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

pub async fn build_s3_client(p: &S3Params) -> Client {
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

    let conf = aws_sdk_s3::config::Builder::from(&shared)
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

    async fn fetch_s3(&self, rest: &str) -> Result<Bytes, AppError> {
        let (bucket, key) = rest
            .split_once('/')
            .filter(|(b, k)| !b.is_empty() && !k.is_empty())
            .ok_or_else(|| AppError::bad_request("S3 source must look like s3://bucket/key"))?;

        let out = match self.s3.get_object().bucket(bucket).key(key).send().await {
            Ok(o) => o,
            Err(e) => {
                let detail = format!("{}", DisplayErrorContext(&e));
                let svc = e.into_service_error();
                if svc.is_no_such_key() {
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
                let detail = format!("{}", DisplayErrorContext(&e));
                return if e.into_service_error().is_no_such_key() { Ok(None) } else { Err(detail) };
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
            .map_err(|e| format!("{}", DisplayErrorContext(&e)))
    }
}
