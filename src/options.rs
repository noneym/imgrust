//! URL parsing. The format is intentionally compatible with imgproxy:
//!
//! ```text
//! /{signature}/{option}:{args}/.../plain/{source_url}[@{ext}]
//! /{signature}/{option}:{args}/.../{base64url(source_url)}[.{ext}]
//! ```

use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use hmac::{Hmac, Mac};
use percent_encoding::percent_decode_str;
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::AppError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Jpeg,
    Png,
    Webp,
    Avif,
    Gif,
}

impl Format {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Format::Jpeg,
            "png" => Format::Png,
            "webp" => Format::Webp,
            "avif" => Format::Avif,
            "gif" => Format::Gif,
            _ => return None,
        })
    }

    pub fn mime(self) -> &'static str {
        match self {
            Format::Jpeg => "image/jpeg",
            Format::Png => "image/png",
            Format::Webp => "image/webp",
            Format::Avif => "image/avif",
            Format::Gif => "image/gif",
        }
    }

    pub fn from_mime(s: &str) -> Option<Self> {
        Some(match s {
            "image/jpeg" => Format::Jpeg,
            "image/png" => Format::Png,
            "image/webp" => Format::Webp,
            "image/avif" => Format::Avif,
            "image/gif" => Format::Gif,
            _ => return None,
        })
    }

    pub fn supports_animation(self) -> bool {
        matches!(self, Format::Webp | Format::Gif)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResizeType {
    #[default]
    Fit,
    Fill,
    Force,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Gravity {
    #[default]
    Center,
    North,
    South,
    East,
    West,
    NorthEast,
    NorthWest,
    SouthEast,
    SouthWest,
    Smart,
}

/// Crop applied to the source before resizing (`c:width:height:gravity`).
/// 0 keeps the full dimension; values below 1 are a fraction of the source.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crop {
    pub width: f32,
    pub height: f32,
    /// Falls back to the `gravity` option when not given.
    pub gravity: Option<Gravity>,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub crop: Option<Crop>,
    pub resize: ResizeType,
    pub width: u32,
    pub height: u32,
    pub enlarge: bool,
    pub dpr: f32,
    pub gravity: Gravity,
    /// 0 = use the configured default.
    pub quality: u8,
    pub blur: f32,
    pub sharpen: f32,
    pub background: Option<[u8; 3]>,
    pub format: Option<Format>,
    pub cachebuster: String,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            crop: None,
            resize: ResizeType::Fit,
            width: 0,
            height: 0,
            enlarge: false,
            dpr: 1.0,
            gravity: Gravity::Center,
            quality: 0,
            blur: 0.0,
            sharpen: 0.0,
            background: None,
            format: None,
            cachebuster: String::new(),
        }
    }
}

/// Which formats we may pick when the URL does not name one.
#[derive(Debug, Clone, Copy, Default)]
pub struct Negotiated {
    pub webp: bool,
    pub avif: bool,
}

#[derive(Debug, Clone)]
pub struct ImageRequest {
    pub source: String,
    pub options: Options,
    /// For thumbor-style URLs: the percent-decoded request path, which is what
    /// thumbor's result storage keys its cache objects by.
    pub thumbor_path: Option<String>,
}

impl ImageRequest {
    /// Stable identifier of the produced variant: used as memory cache key,
    /// S3 cache object name and ETag.
    pub fn cache_key(&self, neg: Negotiated, cfg: &Config) -> String {
        let o = &self.options;
        let fmt = match o.format {
            Some(f) => f.mime().to_string(),
            None => format!("auto:{}{}", neg.webp as u8, neg.avif as u8),
        };
        let bg = o.background.map(hex::encode).unwrap_or_default();
        let canonical = format!(
            "{}\n{}\n{:?}|{:?}|{}|{}|{}|{}|{:?}|{}|{}|{}|{}|{}|{}",
            cfg.fingerprint(),
            self.source,
            o.crop,
            o.resize,
            o.width,
            o.height,
            o.enlarge as u8,
            o.dpr,
            o.gravity,
            o.quality,
            o.blur,
            o.sharpen,
            bg,
            fmt,
            o.cachebuster,
        );
        hex::encode(Sha256::digest(canonical.as_bytes()))
    }
}

fn bad(msg: impl Into<String>) -> AppError {
    AppError::bad_request(msg)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    /// `/{sig}/{opt}:{args}/.../plain/{url}@{ext}`
    Imgproxy,
    /// `/{sig}/fit-in/{W}x{H}/smart/filters:.../{path}`
    Thumbor,
}

fn decode_signature(sig: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(sig.trim_end_matches('=')).ok()
}

fn is_thumbor_size(seg: &str) -> bool {
    let part = |p: &str| {
        let p = p.strip_prefix('-').unwrap_or(p);
        p.is_empty() || p == "orig" || p.bytes().all(|b| b.is_ascii_digit())
    };
    seg != "x" && seg.split_once('x').is_some_and(|(a, b)| part(a) && part(b))
}

fn is_thumbor_manual_crop(seg: &str) -> bool {
    let point = |p: &str| {
        p.split_once('x')
            .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty() && a.bytes().chain(b.bytes()).all(|c| c.is_ascii_digit()))
    };
    seg.split_once(':').is_some_and(|(a, b)| point(a) && point(b))
}

fn is_thumbor_token(seg: &str) -> bool {
    matches!(
        seg,
        "fit-in"
            | "adaptive-fit-in"
            | "full-fit-in"
            | "adaptive-full-fit-in"
            | "smart"
            | "trim"
            | "meta"
            | "left"
            | "right"
            | "center"
            | "top"
            | "middle"
            | "bottom"
    ) || seg.starts_with("filters:")
        || seg.starts_with("trim:")
        || is_thumbor_size(seg)
        || is_thumbor_manual_crop(seg)
}

/// Both URL dialects are served side by side. A real signature tells them
/// apart by its length (HMAC-SHA1 vs HMAC-SHA256); unsigned URLs by shape.
fn detect_style(sig: &str, rest: &str) -> Style {
    match decode_signature(sig).map(|b| b.len()) {
        Some(20) => return Style::Thumbor,
        Some(32) => return Style::Imgproxy,
        _ => {}
    }
    let first = rest.split('/').next().unwrap_or("");
    if is_thumbor_token(first) {
        Style::Thumbor
    } else if first == "plain" || first.contains(':') {
        Style::Imgproxy
    } else if sig == "unsafe" {
        Style::Thumbor
    } else {
        Style::Imgproxy
    }
}

/// What a URL asks for: an image, or a redirect to the signed form of itself.
pub enum Parsed {
    Request(ImageRequest),
    Redirect(String),
}

fn is_placeholder(sig: &str) -> bool {
    matches!(sig, "insecure" | "unsafe" | "_")
}

/// The option part of a URL: everything between the signature and the source.
fn option_prefix(style: Style, rest: &str) -> String {
    match style {
        Style::Imgproxy => match rest.find("/plain/") {
            Some(i) => rest[..i].to_string(),
            None => rest.split('/').take_while(|s| *s != "plain" && s.contains(':')).collect::<Vec<_>>().join("/"),
        },
        Style::Thumbor => {
            let segs: Vec<&str> = rest.split('/').collect();
            let opts = &segs[..segs.len().saturating_sub(1)];
            opts.iter().take_while(|s| is_thumbor_token(s)).copied().collect::<Vec<_>>().join("/")
        }
    }
}

/// Signed equivalent of an unsigned URL path.
fn signed_location(style: Style, rest: &str, cfg: &Config) -> String {
    match style {
        Style::Imgproxy => {
            let raw = format!("/{rest}");
            // Sign the canonical form (with "s3://"), as URL builders do.
            let payload = restore_plain_path(&raw).unwrap_or(raw);
            let mut mac = Hmac::<Sha256>::new_from_slice(&cfg.key).expect("hmac accepts any key length");
            mac.update(&cfg.salt);
            mac.update(payload.as_bytes());
            format!("/{}{}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()), payload)
        }
        Style::Thumbor => {
            let mut mac = Hmac::<Sha1>::new_from_slice(&cfg.thumbor_key).expect("hmac accepts any key length");
            mac.update(rest.as_bytes());
            format!("/{}/{}", URL_SAFE.encode(mac.finalize().into_bytes()), rest)
        }
    }
}

fn invalid_signature() -> AppError {
    AppError::forbidden("invalid signature")
}

/// imgproxy: base64url(HMAC-SHA256(key, salt + "/" + rest)).
fn verify_imgproxy(sig: &str, signed_path: &str, cfg: &Config) -> Result<(), AppError> {
    if cfg.key.is_empty() {
        // Never let one dialect become an unsigned side door for the other.
        return if cfg.thumbor_key.is_empty() || cfg.allow_unsafe { Ok(()) } else { Err(invalid_signature()) };
    }
    let raw = decode_signature(sig).ok_or_else(invalid_signature)?;
    let check = |payload: &str| {
        let mut mac = Hmac::<Sha256>::new_from_slice(&cfg.key).expect("hmac accepts any key length");
        mac.update(&cfg.salt);
        mac.update(payload.as_bytes());
        // Constant-time comparison.
        mac.verify_slice(&raw).is_ok()
    };
    // The URL may have been re-escaped or had its "//" collapsed on the way
    // here; accept the signature over any of those spellings.
    let decoded = percent_decode_str(signed_path).decode_utf8().ok().filter(|d| d != signed_path);
    if check(signed_path)
        || restore_plain_path(signed_path).is_some_and(|p| check(&p))
        || decoded.as_ref().is_some_and(|d| check(d) || restore_plain_path(d).is_some_and(|p| check(&p)))
    {
        Ok(())
    } else {
        Err(invalid_signature())
    }
}

/// thumbor: base64url(HMAC-SHA1(security_key, rest)).
fn verify_thumbor(sig: &str, rest: &str, cfg: &Config) -> Result<(), AppError> {
    if cfg.thumbor_key.is_empty() {
        return if cfg.key.is_empty() || cfg.allow_unsafe { Ok(()) } else { Err(invalid_signature()) };
    }
    if sig == "unsafe" && cfg.allow_unsafe {
        return Ok(());
    }
    let raw = decode_signature(sig).ok_or_else(invalid_signature)?;
    let check = |payload: &[u8]| {
        let mut mac = Hmac::<Sha1>::new_from_slice(&cfg.thumbor_key).expect("hmac accepts any key length");
        mac.update(payload);
        mac.verify_slice(&raw).is_ok()
    };
    // Clients sign the URL before escaping it; proxies may or may not have
    // re-escaped it on the way here.
    if check(rest.as_bytes()) || check(&percent_decode_str(rest).collect::<Vec<u8>>()) {
        Ok(())
    } else {
        Err(invalid_signature())
    }
}

/// Reverse proxies commonly collapse "//" in paths, turning `s3://bucket/key`
/// into `s3:/bucket/key`. Put the second slash back.
fn restore_scheme(s: &str) -> Option<String> {
    ["s3:/", "https:/", "http:/"].iter().find_map(|p| {
        let rest = s.strip_prefix(p)?;
        (!rest.starts_with('/')).then(|| format!("{p}/{rest}"))
    })
}

/// Same repair for a whole imgproxy-style path, so a signature made over the
/// original URL still verifies after a proxy collapsed the slashes.
fn restore_plain_path(path: &str) -> Option<String> {
    let at = path.find("/plain/")? + "/plain/".len();
    Some(format!("{}{}", &path[..at], restore_scheme(&path[at..])?))
}

fn has_scheme(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://") || s.starts_with("s3://")
}

pub fn parse(path: &str, cfg: &Config) -> Result<Parsed, AppError> {
    let path = path.strip_prefix('/').unwrap_or(path);
    let (sig, rest) = path.split_once('/').ok_or_else(|| bad("invalid URL"))?;

    let style = detect_style(sig, rest);
    // Unsigned URL on a signed server: redirect to the signed form instead of
    // serving or rejecting it (the URL is still fully validated below).
    let has_key = match style {
        Style::Imgproxy => !cfg.key.is_empty(),
        Style::Thumbor => !cfg.thumbor_key.is_empty(),
    };
    let redirect = cfg.unsafe_redirect && !cfg.allow_unsafe && has_key && is_placeholder(sig);
    let (source, options) = match style {
        Style::Imgproxy => {
            if !redirect {
                verify_imgproxy(sig, &path[sig.len()..], cfg)?;
            }
            let (mut source, options) = parse_imgproxy(rest, cfg)?;
            if let Some(fixed) = restore_scheme(&source) {
                source = fixed;
            }
            if !has_scheme(&source) {
                source.insert_str(0, &cfg.base_url);
            }
            (source, options)
        }
        Style::Thumbor => {
            if !redirect {
                verify_thumbor(sig, rest, cfg)?;
            }
            let (mut source, options) = parse_thumbor(rest)?;
            if let Some(fixed) = restore_scheme(&source) {
                source = fixed;
            }
            if !has_scheme(&source) {
                let base = if cfg.thumbor_base_url.is_empty() { &cfg.base_url } else { &cfg.thumbor_base_url };
                source.insert_str(0, base);
            }
            (source, options)
        }
    };

    if !has_scheme(&source) {
        return Err(bad("source URL must be http://, https:// or s3:// (or configure a base URL for relative paths)"));
    }
    if !cfg.allowed_sources.is_empty() && !cfg.allowed_sources.iter().any(|p| source.starts_with(p.as_str())) {
        return Err(AppError::forbidden("source is not allowed"));
    }

    let max = cfg.max_dimension as f32;
    if options.width as f32 * options.dpr > max || options.height as f32 * options.dpr > max {
        return Err(bad(format!("requested dimensions exceed the limit of {}px", cfg.max_dimension)));
    }

    if redirect {
        let prefix = option_prefix(style, rest);
        if !cfg.unsafe_redirect_options.is_empty() && !cfg.unsafe_redirect_options.iter().any(|o| *o == prefix) {
            tracing::info!(
                options = %if prefix.is_empty() { "none" } else { prefix.as_str() },
                path = %rest,
                "unsigned URL refused: options are not in IMGRUST_UNSAFE_REDIRECT_OPTIONS"
            );
            return Err(AppError::forbidden("unsigned URLs are not allowed for these options"));
        }
        return Ok(Parsed::Redirect(signed_location(style, rest, cfg)));
    }

    let thumbor_path =
        (style == Style::Thumbor).then(|| percent_decode_str(path).decode_utf8_lossy().trim_start_matches('/').to_string());
    Ok(Parsed::Request(ImageRequest { source, options, thumbor_path }))
}

/// thumbor grammar:
/// `[trim/][fit-in/][-]Wx[-]H/[left|center|right/][top|middle|bottom/][smart/][filters:.../]image`
fn parse_thumbor(rest: &str) -> Result<(String, Options), AppError> {
    // Without fit-in thumbor fills the box exactly, cropping and upscaling as needed.
    let mut o = Options { resize: ResizeType::Fill, enlarge: true, ..Options::default() };
    let mut fit_in = false;
    let mut upscale: Option<bool> = None;
    let (mut halign, mut valign, mut smart) = (0i8, 0i8, false);

    let mut remaining = rest;
    while let Some((seg, tail)) = remaining.split_once('/') {
        match seg {
            "meta" => return Err(bad("the meta endpoint is not supported")),
            "trim" => {}
            "fit-in" | "adaptive-fit-in" | "full-fit-in" | "adaptive-full-fit-in" => fit_in = true,
            "left" => halign = -1,
            "center" => halign = 0,
            "right" => halign = 1,
            "top" => valign = -1,
            "middle" => valign = 0,
            "bottom" => valign = 1,
            "smart" => smart = true,
            _ if seg.starts_with("trim:") => {}
            _ if seg.starts_with("filters:") => apply_thumbor_filters(&mut o, &mut upscale, &seg["filters:".len()..])?,
            _ if is_thumbor_manual_crop(seg) => return Err(bad("manual crop (AxB:CxD) is not supported")),
            _ if is_thumbor_size(seg) => {
                let (w, h) = seg.split_once('x').unwrap_or_default();
                // A leading '-' means "flip" in thumbor; the size is still honoured.
                let dim = |v: &str| -> Result<u32, AppError> {
                    let v = v.strip_prefix('-').unwrap_or(v);
                    if v.is_empty() || v == "orig" {
                        Ok(0)
                    } else {
                        parse_u32(v, "size")
                    }
                };
                o.width = dim(w)?;
                o.height = dim(h)?;
            }
            _ => break,
        }
        remaining = tail;
    }

    if remaining.is_empty() {
        return Err(bad("missing image path"));
    }
    if fit_in {
        o.resize = ResizeType::Fit;
        o.enlarge = false;
    }
    if let Some(u) = upscale {
        o.enlarge = u;
    }
    o.gravity = if smart {
        Gravity::Smart
    } else {
        match (halign, valign) {
            (-1, -1) => Gravity::NorthWest,
            (0, -1) => Gravity::North,
            (1, -1) => Gravity::NorthEast,
            (-1, 0) => Gravity::West,
            (1, 0) => Gravity::East,
            (-1, 1) => Gravity::SouthWest,
            (0, 1) => Gravity::South,
            (1, 1) => Gravity::SouthEast,
            _ => Gravity::Center,
        }
    };

    let source = percent_decode_str(remaining)
        .decode_utf8()
        .map_err(|_| bad("image path is not valid UTF-8"))?
        .into_owned();
    Ok((source, o))
}

/// `quality(80):format(webp):blur(3)`. Unknown filters are ignored, like
/// thumbor does for filters that are not enabled.
fn apply_thumbor_filters(o: &mut Options, upscale: &mut Option<bool>, filters: &str) -> Result<(), AppError> {
    for part in filters.split(')') {
        let part = part.trim_start_matches(':');
        let Some((name, args)) = part.split_once('(') else { continue };
        let first = args.split(',').next().unwrap_or("").trim();
        match name {
            "quality" => {
                let q = parse_u32(first, "quality")?;
                if !(1..=100).contains(&q) {
                    return Err(bad("quality must be between 1 and 100"));
                }
                o.quality = q as u8;
            }
            "format" => {
                o.format = Some(Format::parse(first).ok_or_else(|| bad(format!("unsupported format: {first}")))?)
            }
            "blur" => {
                // blur(radius[, sigma])
                let sigma = args.split(',').nth(1).map(str::trim).filter(|s| !s.is_empty()).unwrap_or(first);
                o.blur = parse_f32(sigma, "blur")?.min(100.0);
            }
            "upscale" => *upscale = Some(true),
            "no_upscale" => *upscale = Some(false),
            "background_color" | "fill" => {
                if let Ok(bytes) = hex::decode(first.trim_start_matches('#')) {
                    if let Ok(rgb) = <[u8; 3]>::try_from(bytes.as_slice()) {
                        o.background = Some(rgb);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn parse_imgproxy(rest: &str, cfg: &Config) -> Result<(String, Options), AppError> {
    let mut options = Options::default();
    let mut remaining = rest;
    let (source, ext) = loop {
        let (seg, tail) = match remaining.split_once('/') {
            Some((s, t)) => (s, Some(t)),
            None => (remaining, None),
        };
        if seg == "plain" {
            break parse_plain(tail.ok_or_else(|| bad("missing source URL"))?)?;
        }
        if seg.contains(':') {
            apply_option(&mut options, seg, cfg)?;
            remaining = tail.ok_or_else(|| bad("missing source URL"))?;
            continue;
        }
        break parse_base64(remaining)?;
    };

    if let Some(ext) = ext {
        options.format = Some(Format::parse(&ext).ok_or_else(|| bad(format!("unsupported format: {ext}")))?);
    }
    Ok((source, options))
}

fn parse_plain(raw: &str) -> Result<(String, Option<String>), AppError> {
    if raw.is_empty() {
        return Err(bad("missing source URL"));
    }
    // "@ext" only counts when the suffix is a format we know, so URLs that
    // contain a literal '@' keep working.
    let (url, ext) = match raw.rsplit_once('@') {
        Some((u, e)) if Format::parse(e).is_some() => (u, Some(e.to_string())),
        _ => (raw, None),
    };
    let url = percent_decode_str(url)
        .decode_utf8()
        .map_err(|_| bad("source URL is not valid UTF-8"))?
        .into_owned();
    Ok((url, ext))
}

fn parse_base64(raw: &str) -> Result<(String, Option<String>), AppError> {
    let (b64, ext) = match raw.rsplit_once('.') {
        Some((b, e)) => (b, Some(e.to_string())),
        None => (raw, None),
    };
    // imgproxy allows the encoded URL to be split with slashes.
    let joined: String = b64.chars().filter(|c| *c != '/').collect();
    let bytes = URL_SAFE_NO_PAD
        .decode(joined.trim_end_matches('='))
        .map_err(|_| bad("source URL is neither plain nor valid base64"))?;
    let url = String::from_utf8(bytes).map_err(|_| bad("source URL is not valid UTF-8"))?;
    if url.is_empty() {
        return Err(bad("missing source URL"));
    }
    Ok((url, ext))
}

fn arg<'a>(args: &[&'a str], i: usize) -> Option<&'a str> {
    args.get(i).copied().filter(|s| !s.is_empty())
}

fn parse_u32(v: &str, what: &str) -> Result<u32, AppError> {
    v.parse::<u32>().map_err(|_| bad(format!("invalid {what}: {v}")))
}

fn parse_f32(v: &str, what: &str) -> Result<f32, AppError> {
    match v.parse::<f32>() {
        Ok(f) if f.is_finite() && f >= 0.0 => Ok(f),
        _ => Err(bad(format!("invalid {what}: {v}"))),
    }
}

fn parse_bool(v: &str) -> bool {
    matches!(v, "1" | "t" | "true")
}

fn parse_resize_type(v: &str) -> Result<ResizeType, AppError> {
    Ok(match v {
        "fit" => ResizeType::Fit,
        "fill" | "fill-down" => ResizeType::Fill,
        "force" => ResizeType::Force,
        "auto" => ResizeType::Auto,
        _ => return Err(bad(format!("invalid resizing type: {v}"))),
    })
}

fn parse_gravity(v: &str) -> Result<Gravity, AppError> {
    Ok(match v {
        "ce" => Gravity::Center,
        "no" => Gravity::North,
        "so" => Gravity::South,
        "ea" => Gravity::East,
        "we" => Gravity::West,
        "noea" => Gravity::NorthEast,
        "nowe" => Gravity::NorthWest,
        "soea" => Gravity::SouthEast,
        "sowe" => Gravity::SouthWest,
        "sm" => Gravity::Smart,
        _ => return Err(bad(format!("invalid gravity: {v}"))),
    })
}

fn parse_background(args: &[&str]) -> Result<[u8; 3], AppError> {
    let invalid = || bad("invalid background, expected RRGGBB or R:G:B");
    if args.len() >= 3 {
        let mut out = [0u8; 3];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = args[i].parse().map_err(|_| invalid())?;
        }
        return Ok(out);
    }
    let hexs = arg(args, 0).ok_or_else(invalid)?;
    let bytes = hex::decode(hexs).map_err(|_| invalid())?;
    <[u8; 3]>::try_from(bytes.as_slice()).map_err(|_| invalid())
}

fn apply_option(o: &mut Options, seg: &str, cfg: &Config) -> Result<(), AppError> {
    let mut parts = seg.split(':');
    let name = parts.next().unwrap_or_default();
    let args: Vec<&str> = parts.collect();

    match name {
        "rs" | "resize" => {
            if let Some(v) = arg(&args, 0) {
                o.resize = parse_resize_type(v)?;
            }
            if let Some(v) = arg(&args, 1) {
                o.width = parse_u32(v, "width")?;
            }
            if let Some(v) = arg(&args, 2) {
                o.height = parse_u32(v, "height")?;
            }
            if let Some(v) = arg(&args, 3) {
                o.enlarge = parse_bool(v);
            }
        }
        "s" | "size" => {
            if let Some(v) = arg(&args, 0) {
                o.width = parse_u32(v, "width")?;
            }
            if let Some(v) = arg(&args, 1) {
                o.height = parse_u32(v, "height")?;
            }
            if let Some(v) = arg(&args, 2) {
                o.enlarge = parse_bool(v);
            }
        }
        "c" | "crop" => {
            let width = parse_f32(arg(&args, 0).unwrap_or("0"), "crop width")?;
            let height = parse_f32(arg(&args, 1).unwrap_or("0"), "crop height")?;
            let gravity = arg(&args, 2).map(parse_gravity).transpose()?;
            o.crop = (width > 0.0 || height > 0.0).then_some(Crop { width, height, gravity });
        }
        "rt" | "resizing_type" => {
            o.resize = parse_resize_type(arg(&args, 0).ok_or_else(|| bad("missing resizing type"))?)?;
        }
        "w" | "width" => o.width = parse_u32(arg(&args, 0).unwrap_or("0"), "width")?,
        "h" | "height" => o.height = parse_u32(arg(&args, 0).unwrap_or("0"), "height")?,
        "el" | "enlarge" => o.enlarge = parse_bool(arg(&args, 0).unwrap_or("")),
        "dpr" => {
            let v = parse_f32(arg(&args, 0).unwrap_or("1"), "dpr")?;
            if !(0.1..=8.0).contains(&v) {
                return Err(bad("dpr must be between 0.1 and 8"));
            }
            o.dpr = v;
        }
        "g" | "gravity" => o.gravity = parse_gravity(arg(&args, 0).ok_or_else(|| bad("missing gravity"))?)?,
        "q" | "quality" => {
            let v = parse_u32(arg(&args, 0).unwrap_or("0"), "quality")?;
            if v > 100 {
                return Err(bad("quality must be between 1 and 100"));
            }
            o.quality = v as u8;
        }
        "f" | "format" | "ext" => {
            let v = arg(&args, 0).ok_or_else(|| bad("missing format"))?;
            o.format = Some(Format::parse(v).ok_or_else(|| bad(format!("unsupported format: {v}")))?);
        }
        "bl" | "blur" => o.blur = parse_f32(arg(&args, 0).unwrap_or("0"), "blur")?.min(100.0),
        "sh" | "sharpen" => o.sharpen = parse_f32(arg(&args, 0).unwrap_or("0"), "sharpen")?.min(10.0),
        "bg" | "background" => o.background = Some(parse_background(&args)?),
        "cb" | "cachebuster" => o.cachebuster = args.join(":"),
        // Accepted for imgproxy URL compatibility; metadata is always stripped
        // and EXIF orientation is always applied.
        "sm" | "strip_metadata" | "scp" | "strip_color_profile" | "ar" | "auto_rotate" => {}
        _ => return Err(bad(format!("unknown processing option: {name}"))),
    }
    let _ = cfg;
    Ok(())
}
