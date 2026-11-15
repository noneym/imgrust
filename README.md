# imgrust

A fast, self-hosted image processing proxy written in Rust on top of libvips.

- **S3 sources** – `s3://bucket/key`, plus any `http(s)://` origin
- **S3-backed result cache** – every processed variant (size, quality, format…) is stored in a bucket and reused across restarts and replicas
- **In-memory hot cache** in front of it, bounded by bytes
- **Animated WebP / GIF** – frames are resized and cropped individually, timing is preserved, GIF → animated WebP
- **Resize, crop, quality, format conversion** (JPEG, PNG, WebP, AVIF, GIF), blur, sharpen, background
- **Request coalescing** – N concurrent requests for the same cold variant trigger one computation
- **Bounded CPU concurrency** with load shedding (`503`) instead of unbounded memory growth
- **Two URL dialects side by side** – imgproxy-style (`/sig/rs:fit:0:210/plain/s3://…`) and thumbor-style (`/sig/372x372/path.jpg`), each with its own signature scheme, so existing URLs keep working
- `ETag` / `304`, `Vary: Accept`, `Cache-Control`, Prometheus metrics, container health check

Website: <https://imgrust.com>

## Quick start

```bash
docker compose up --build
```

This starts imgrust on `:8080` together with a local S3-compatible server ([S3Mock](https://github.com/adobe/S3Mock), on `:9090`) holding two buckets: `images` (sources) and `imgrust-cache` (processed variants). If port 8080 is taken: `IMGRUST_PORT=8089 docker compose up --build`.

Upload a file to `images`, then request it in either URL style:

```bash
curl -T photo.jpg http://localhost:9090/images/photo.jpg

curl -o out.webp "http://localhost:8080/insecure/rs:fill:400:300/q:80/plain/s3://images/photo.jpg@webp"
curl -o out.jpg  "http://localhost:8080/unsafe/400x300/photo.jpg"
```

The `X-Cache` response header tells you where the response came from: `MISS` (processed now), `HIT` (memory) or `HIT-S3` (cache bucket).

### Production

```bash
docker build -t imgrust .

docker run -d -p 8080:8080 \
  -e IMGRUST_KEY=$(openssl rand -hex 32) \
  -e IMGRUST_SALT=$(openssl rand -hex 32) \
  -e IMGRUST_S3_REGION=eu-central-1 \
  -e IMGRUST_S3_ACCESS_KEY=... \
  -e IMGRUST_S3_SECRET_KEY=... \
  -e IMGRUST_CACHE_S3_BUCKET=my-image-cache \
  imgrust
```

Put a CDN in front of it. imgrust sends long-lived `Cache-Control`, `ETag` and `Vary: Accept` headers.

## URL formats

imgrust understands two URL dialects on the same server. A signed URL is recognised by the length of its signature (HMAC-SHA256 → imgproxy, HMAC-SHA1 → thumbor); an unsigned one by its shape.

### imgproxy style

```
/{signature}/{option}/{option}/.../plain/{source_url}[@{format}]
/{signature}/{option}/{option}/.../{base64url(source_url)}[.{format}]
```

```
/insecure/rs:fit:0:210/plain/s3://processed-videos/34973_ndeiz/preview.webp
/insecure/c:950:600:we/rs:fit:0:210/plain/s3://processed-videos/34973_ndeiz/preview_image.webp
```

- In a `plain` source URL, escape `?` as `%3F` and `@` as `%40`.
- If no format is given, imgrust keeps the source format, or negotiates WebP/AVIF from the `Accept` header (see `IMGRUST_AUTO_WEBP`, `IMGRUST_AUTO_AVIF`).
- When no signing key is configured the signature segment is not checked and can be any placeholder (`insecure`, `_`, …).

### Processing options

| Option | Alias | Description |
| --- | --- | --- |
| `crop:w:h:gravity` | `c` | Crop the source **before** resizing. `0` = full dimension, values below 1 = fraction of the source. Gravity is optional |
| `resize:type:w:h:enlarge` | `rs` | Resizing type, width, height and enlarge flag in one option |
| `size:w:h:enlarge` | `s` | Width, height and enlarge flag |
| `resizing_type:type` | `rt` | `fit` (default), `fill`, `force`, `auto` |
| `width:n` / `height:n` | `w` / `h` | Target size; `0` = derive from aspect ratio |
| `enlarge:1` | `el` | Allow upscaling (off by default) |
| `dpr:n` | | Multiply dimensions, `0.1`–`8` |
| `gravity:type` | `g` | Crop anchor for `fill`: `ce`, `no`, `so`, `ea`, `we`, `noea`, `nowe`, `soea`, `sowe`, `sm` (smart) |
| `quality:n` | `q` | 1–100 |
| `format:ext` | `f`, `ext` | `jpg`, `png`, `webp`, `avif`, `gif` |
| `blur:sigma` | `bl` | Gaussian blur |
| `sharpen:sigma` | `sh` | Sharpen |
| `background:RRGGBB` | `bg` | Background for flattening transparency (also `bg:R:G:B`) |
| `cachebuster:value` | `cb` | Changes the cache key without changing the image |

Resizing types:

- `fit` – scale to fit inside `w`×`h`, keeping the aspect ratio
- `fill` – scale to cover `w`×`h`, then crop the overflow according to gravity
- `force` – scale to exactly `w`×`h`, ignoring the aspect ratio
- `auto` – `fill` when source and target have the same orientation, otherwise `fit`

Metadata is always stripped, EXIF orientation is always applied and colours are converted to sRGB.

### thumbor style

```
/{signature}/[trim/][fit-in/]{W}x{H}/[left|center|right/][top|middle|bottom/][smart/][filters:.../]{image}
```

```
/aP98eWA5fPRm4uLQbTO8TdENsE8=/372x372/merve-taskin/ch37_329_0.jpg
/unsafe/fit-in/600x400/filters:quality(70):format(webp)/merve-taskin/ch37_329_0.jpg
```

- `{W}x{H}` without `fit-in` fills the box exactly (crop + upscale, like thumbor); with `fit-in` the image is fitted inside it and not upscaled. `0` or an empty dimension keeps the aspect ratio.
- `{image}` is either a full `http(s)://` / `s3://` URL or a path relative to `IMGRUST_THUMBOR_BASE_URL` (e.g. `s3://my-bucket/`).
- Supported filters: `quality(n)`, `format(ext)`, `blur(radius[,sigma])`, `upscale()`, `no_upscale()`, `background_color(hex)` / `fill(hex)`. Other filters are ignored.
- Not supported: manual crop (`AxB:CxD`), `meta`, flipping (a negative size is treated as its absolute value); `trim` is accepted and ignored.

### Animations

Animated GIF and WebP sources stay animated when the output format is WebP or GIF. With automatic format selection, an animated GIF is served as animated WebP to clients that accept it. `g:sm` falls back to centre gravity for animations, and AVIF/JPEG/PNG output uses the first frame.

### Signing URLs

**thumbor style:** `signature = base64url(HMAC-SHA1(IMGRUST_THUMBOR_KEY, path))` with padding, where `path` is everything after the signature (no leading `/`) and the key is used as plain text. This is thumbor's own scheme, so URLs generated for an existing thumbor keep validating when `IMGRUST_THUMBOR_KEY` is set to its `SECURITY_KEY`.

If only one of the two keys is configured, URLs of the other dialect are rejected, so it can never be used as an unsigned side door.

**imgproxy style:** the scheme is the same as imgproxy's: `signature = base64url(HMAC-SHA256(key, salt + path))` without padding, where `path` is everything after the signature including the leading `/`, and key/salt are the hex-decoded values of `IMGRUST_KEY` / `IMGRUST_SALT`.

```python
import base64, hashlib, hmac

key = bytes.fromhex("...")
salt = bytes.fromhex("...")
path = "/rs:fill:400:300/plain/s3://images/photo.jpg@webp"

sig = base64.urlsafe_b64encode(hmac.new(key, salt + path.encode(), hashlib.sha256).digest()).rstrip(b"=").decode()
print(f"/{sig}{path}")
```

## How caching works

1. A cache key is derived from the source URL, all options, the (negotiated) output format and the encoder settings.
2. `If-None-Match` matching that key → `304` without any I/O.
3. Memory cache (`IMGRUST_MEM_CACHE_MB`).
4. S3 cache bucket: `{prefix}{k[0:2]}/{k[2:4]}/{key}`, stored with its `Content-Type`.
5. Otherwise the source is fetched and processed; the result is returned immediately and uploaded to the cache bucket in the background.

**Migrating from thumbor:** point `IMGRUST_CACHE_S3_BUCKET` at thumbor's result-storage bucket and set `IMGRUST_THUMBOR_CACHE_ROOT`. Everything thumbor already rendered is served straight from that bucket, and what imgrust renders is stored in the same layout, so both can run side by side on one cache. imgproxy-style variants live under `IMGRUST_CACHE_S3_PREFIX` in the same bucket.

There is no expiry inside imgrust: use an S3 **lifecycle rule** on the cache bucket/prefix to expire old variants. If a source object changes under the same URL, change the URL or add a `cb:` option; bumping `IMGRUST_CACHE_S3_PREFIX` invalidates the whole S3 cache.

## Configuration

| Variable | Default | Description |
| --- | --- | --- |
| `IMGRUST_BIND` | `0.0.0.0:8080` | Listen address |
| `IMGRUST_KEY`, `IMGRUST_SALT` | – | Hex-encoded signing key and salt. No key = unsigned URLs accepted |
| `IMGRUST_THUMBOR_KEY` | – | Plain-text security key for thumbor-style URLs |
| `IMGRUST_THUMBOR_BASE_URL` | `IMGRUST_BASE_URL` | Prefix for relative thumbor-style image paths (e.g. `s3://media/`) |
| `IMGRUST_THUMBOR_CACHE_ROOT` | – | Share thumbor's S3 result storage: thumbor-style results are read from and written to `{root}/[auto_webp/]{request path}` in the cache bucket, the layout of `thumbor_aws.result_storage` (set it to thumbor's `AWS_RESULT_STORAGE_ROOT_PATH`, e.g. `rs`) |
| `IMGRUST_THUMBOR_AUTO_WEBP` | `false` | thumbor's `AUTO_WEBP` for thumbor-style URLs when sharing its result storage |
| `IMGRUST_ALLOW_UNSAFE_URL` | `false` | Like thumbor's `ALLOW_UNSAFE_URL`: accept unsigned URLs (`/unsafe/…`, and imgproxy-style URLs when no `IMGRUST_KEY` is set) even though a signing key is configured |
| `IMGRUST_BASE_URL` | – | Prefix prepended to source URLs that have no scheme (e.g. `s3://media/`) |
| `IMGRUST_ALLOWED_SOURCES` | any | Comma-separated list of allowed source URL prefixes |
| `IMGRUST_S3_ENDPOINT` | AWS | Custom S3 endpoint (R2, MinIO, Spaces…) |
| `IMGRUST_S3_REGION` | `AWS_REGION` or `us-east-1` | Region |
| `IMGRUST_S3_ACCESS_KEY`, `IMGRUST_S3_SECRET_KEY` | AWS credential chain | Static credentials |
| `IMGRUST_S3_FORCE_PATH_STYLE` | `true` with a custom endpoint | Path-style addressing |
| `IMGRUST_CACHE_S3_BUCKET` | – | Bucket for processed variants; unset disables the S3 cache |
| `IMGRUST_CACHE_S3_PREFIX` | – | Key prefix in the cache bucket |
| `IMGRUST_CACHE_S3_ENDPOINT`, `_REGION`, `_ACCESS_KEY`, `_SECRET_KEY`, `_FORCE_PATH_STYLE` | source settings | Override when the cache bucket lives on another provider/account |
| `IMGRUST_MEM_CACHE_MB` | `256` | In-memory cache size |
| `IMGRUST_TTL` | `31536000` | `max-age` sent to clients, seconds |
| `IMGRUST_WORKERS` | CPU count | Images processed concurrently |
| `IMGRUST_VIPS_CONCURRENCY` | `1` | libvips threads per image |
| `IMGRUST_QUEUE_TIMEOUT_MS` | `10000` | Max wait for a free worker before `503` |
| `IMGRUST_DOWNLOAD_TIMEOUT_MS` | `10000` | HTTP source timeout |
| `IMGRUST_MAX_SRC_MB` | `50` | Max source file size |
| `IMGRUST_MAX_SRC_RESOLUTION` | `50` | Max source resolution per frame, megapixels |
| `IMGRUST_MAX_ANIMATION_FRAMES` | `300` | Frames kept from an animation |
| `IMGRUST_MAX_ANIMATION_RESOLUTION` | `200` | Max total megapixels across all frames |
| `IMGRUST_MAX_DIMENSION` | `8192` | Max requested width/height (after DPR) |
| `IMGRUST_QUALITY` | `80` | Default JPEG/WebP quality |
| `IMGRUST_AVIF_QUALITY` | `60` | Default AVIF quality |
| `IMGRUST_WEBP_EFFORT` | `4` | WebP encoder effort, 0–6 |
| `IMGRUST_AVIF_EFFORT` | `3` | AVIF encoder effort, 0–9 |
| `IMGRUST_AUTO_WEBP` | `true` | Serve WebP when the client accepts it and no format is given |
| `IMGRUST_AUTO_AVIF` | `false` | Same for AVIF (slower to encode) |
| `IMGRUST_LOG_LEVEL` | `info` | `error`, `warn`, `info`, `debug`, `trace` |

## Endpoints

- `GET /health` – liveness
- `GET /version` – running version (also sent as the `Server: imgrust/x.y.z` response header)
- `GET /metrics` – Prometheus counters (requests, cache hits per layer, misses, processing time…)

## Security notes

- Set `IMGRUST_KEY`/`IMGRUST_SALT` and/or `IMGRUST_THUMBOR_KEY` in production. Without them anyone can make your server fetch and process arbitrary URLs.
- HTTP sources are fetched from wherever the URL points, including private network addresses. If the service runs inside a private network, restrict sources with `IMGRUST_ALLOWED_SOURCES` in addition to signing.
- Only JPEG, PNG, WebP, GIF, AVIF and HEIC inputs are accepted; everything else is rejected before it reaches a decoder.

## Landing page

`landing/` contains the static site for imgrust.com:

```bash
docker build -t imgrust-landing landing && docker run -p 8081:80 imgrust-landing
```

## Versioning

imgrust follows semantic versioning. The version lives in `Cargo.toml`, every release is a `vX.Y.Z` git tag, and changes are listed in [CHANGELOG.md](CHANGELOG.md). Cache keys carry their own format version, so upgrading never serves variants produced by incompatible settings.

## Development

The build needs libvips ≥ 8.13 with development headers and `pkg-config`. The Dockerfile is the reference build environment.
