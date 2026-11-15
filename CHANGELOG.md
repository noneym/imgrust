# Changelog

All notable changes to imgrust are listed here. Versions follow [semantic versioning](https://semver.org).

## 0.3.2 - 2026-11-15

### Fixed
- Truncated WebP sources (an upload or encode that was cut short: RIFF size of 0 and an incomplete last frame) are no longer rejected with `webp: unable to parse image`; the frames that arrived complete are served.

## 0.3.1 - 2026-11-15

### Changed
- A refused unsigned URL is logged with its path, so the missing option string can be identified.
- S3 errors are logged with the HTTP status the store answered with.
- `IMGRUST_UNSAFE_REDIRECT_OPTIONS` accepts `none` for URLs without any processing option.

## 0.3.0 - 2026-11-15

### Added
- `IMGRUST_UNSAFE_REDIRECT`: unsigned URLs (`/insecure/...`, `/unsafe/...`) are answered with a `301` to their signed form, so links that are already out there keep working after signing is switched on.
- `IMGRUST_UNSAFE_REDIRECT_OPTIONS`: allow-list of option strings eligible for that redirect; anything else is `403`.
- `imgrust_signed_redirects_total` metric.

### Fixed
- imgproxy-style signatures verify when the URL was percent-escaped after signing.

## 0.2.0 - 2026-11-15

### Added
- thumbor-style URLs (`/{sig}/fit-in/{W}x{H}/smart/filters:.../{path}`) with HMAC-SHA1 signatures, served side by side with imgproxy-style URLs.
- `c:` / `crop:` option: crop the source before resizing.
- Shared result cache with thumbor (`IMGRUST_THUMBOR_CACHE_ROOT`): reads and writes the `thumbor_aws.result_storage` layout.
- `IMGRUST_ALLOW_UNSAFE_URL`, `IMGRUST_THUMBOR_KEY`, `IMGRUST_THUMBOR_BASE_URL`, `IMGRUST_THUMBOR_AUTO_WEBP`.
- `GET /version` and a `Server: imgrust/x.y.z` response header; the version is logged at startup.

### Fixed
- Source URLs whose `//` was collapsed by a reverse proxy (`s3:/bucket/key`) are accepted.
- S3 front ends that answer a missing object with a bare 5xx: a missing source is now a `404` instead of a `502`, and a missing cache object is a plain miss without SDK retries.
- Error logs no longer dump the whole HTTP exchange.

## 0.1.0 - 2026-11-14

- Initial release: resize, crop, quality and format conversion (JPEG, PNG, WebP, AVIF, GIF) on libvips, animated WebP/GIF, S3 and HTTP sources, in-memory and S3 result cache, request coalescing, signed imgproxy-style URLs, Dockerfile and landing page.
