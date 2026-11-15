//! Image pipeline on top of libvips, through a deliberately tiny FFI surface.
//!
//! libvips gives us shrink-on-load, streaming pipelines and animated WebP/GIF
//! handling ("toilet roll" images: all frames stacked vertically, with the
//! frame height stored in the `page-height` metadata field).

use std::ffi::{c_char, c_double, c_int, c_void, CStr, CString};
use std::ptr;

use bytes::Bytes;

use crate::config::Config;
use crate::options::{Format, Gravity, Negotiated, Options, ResizeType};

#[repr(C)]
struct VipsImage {
    _private: [u8; 0],
}

const VIPS_SIZE_FORCE: c_int = 3;
const VIPS_INTERESTING_ATTENTION: c_int = 3;
const VIPS_FOREIGN_HEIF_COMPRESSION_AV1: c_int = 4;

extern "C" {
    fn vips_init(argv0: *const c_char) -> c_int;
    fn vips_concurrency_set(concurrency: c_int);
    fn vips_cache_set_max(max: c_int);
    fn vips_cache_set_max_mem(max_mem: usize);
    fn vips_cache_set_max_files(max_files: c_int);
    fn vips_block_untrusted_set(state: c_int);
    fn vips_error_buffer() -> *const c_char;
    fn vips_error_clear();

    fn vips_image_new_from_buffer(buf: *const c_void, len: usize, option_string: *const c_char, ...) -> *mut VipsImage;
    fn vips_image_copy_memory(image: *mut VipsImage) -> *mut VipsImage;
    fn vips_image_get_width(image: *const VipsImage) -> c_int;
    fn vips_image_get_height(image: *const VipsImage) -> c_int;
    fn vips_image_get_n_pages(image: *mut VipsImage) -> c_int;
    fn vips_image_get_orientation(image: *mut VipsImage) -> c_int;
    fn vips_image_hasalpha(image: *mut VipsImage) -> c_int;
    fn vips_image_set_int(image: *mut VipsImage, name: *const c_char, i: c_int);

    fn vips_thumbnail_image(input: *mut VipsImage, out: *mut *mut VipsImage, width: c_int, ...) -> c_int;
    fn vips_autorot(input: *mut VipsImage, out: *mut *mut VipsImage, ...) -> c_int;
    fn vips_thumbnail_buffer(buf: *mut c_void, len: usize, out: *mut *mut VipsImage, width: c_int, ...) -> c_int;
    fn vips_extract_area(input: *mut VipsImage, out: *mut *mut VipsImage, left: c_int, top: c_int, width: c_int, height: c_int, ...) -> c_int;
    fn vips_smartcrop(input: *mut VipsImage, out: *mut *mut VipsImage, width: c_int, height: c_int, ...) -> c_int;
    fn vips_arrayjoin(input: *mut *mut VipsImage, out: *mut *mut VipsImage, n: c_int, ...) -> c_int;
    fn vips_gaussblur(input: *mut VipsImage, out: *mut *mut VipsImage, sigma: c_double, ...) -> c_int;
    fn vips_sharpen(input: *mut VipsImage, out: *mut *mut VipsImage, ...) -> c_int;
    fn vips_flatten(input: *mut VipsImage, out: *mut *mut VipsImage, ...) -> c_int;

    fn vips_array_double_new(array: *const c_double, n: c_int) -> *mut c_void;
    fn vips_area_unref(area: *mut c_void);

    fn vips_jpegsave_buffer(input: *mut VipsImage, buf: *mut *mut c_void, len: *mut usize, ...) -> c_int;
    fn vips_pngsave_buffer(input: *mut VipsImage, buf: *mut *mut c_void, len: *mut usize, ...) -> c_int;
    fn vips_webpsave_buffer(input: *mut VipsImage, buf: *mut *mut c_void, len: *mut usize, ...) -> c_int;
    fn vips_gifsave_buffer(input: *mut VipsImage, buf: *mut *mut c_void, len: *mut usize, ...) -> c_int;
    fn vips_heifsave_buffer(input: *mut VipsImage, buf: *mut *mut c_void, len: *mut usize, ...) -> c_int;

    fn g_object_unref(object: *mut c_void);
    fn g_free(mem: *mut c_void);
}

/// Terminator for libvips' NULL-terminated varargs.
const END: *const c_char = ptr::null();

pub fn init(concurrency: i32) {
    unsafe {
        if vips_init(c"imgrust".as_ptr()) != 0 {
            panic!("failed to initialise libvips: {}", last_error());
        }
        // We parallelise across requests, not inside a single image.
        vips_concurrency_set(concurrency);
        // The operation cache only helps repeated identical pipelines on the
        // same input object; for a proxy it is pure memory overhead.
        vips_cache_set_max(0);
        vips_cache_set_max_mem(0);
        vips_cache_set_max_files(0);
        // Refuse loaders that are not safe for untrusted input.
        vips_block_untrusted_set(1);
    }
}

fn last_error() -> String {
    unsafe {
        let msg = CStr::from_ptr(vips_error_buffer()).to_string_lossy().trim().replace('\n', "; ");
        vips_error_clear();
        msg
    }
}

#[derive(Debug)]
pub enum ProcError {
    Unsupported,
    TooLarge(String),
    Vips(String),
}

fn check(code: c_int) -> Result<(), ProcError> {
    if code == 0 {
        Ok(())
    } else {
        Err(ProcError::Vips(last_error()))
    }
}

/// Owned reference to a VipsImage.
struct Image(*mut VipsImage);

impl Drop for Image {
    fn drop(&mut self) {
        unsafe { g_object_unref(self.0 as *mut c_void) }
    }
}

impl Image {
    fn wrap(p: *mut VipsImage) -> Result<Image, ProcError> {
        if p.is_null() {
            Err(ProcError::Vips(last_error()))
        } else {
            Ok(Image(p))
        }
    }
    fn width(&self) -> i32 {
        unsafe { vips_image_get_width(self.0) }
    }
    fn height(&self) -> i32 {
        unsafe { vips_image_get_height(self.0) }
    }
    fn has_alpha(&self) -> bool {
        unsafe { vips_image_hasalpha(self.0) != 0 }
    }
}

/// Encoded output owned by glib; handed to `Bytes` without copying.
struct VipsBuf {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for VipsBuf {}
unsafe impl Sync for VipsBuf {}

impl AsRef<[u8]> for VipsBuf {
    fn as_ref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for VipsBuf {
    fn drop(&mut self) {
        unsafe { g_free(self.ptr as *mut c_void) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceFormat {
    Jpeg,
    Png,
    Webp,
    Gif,
    Avif,
    Heic,
}

/// Only formats recognised here ever reach a libvips loader.
fn sniff(b: &[u8]) -> Option<SourceFormat> {
    if b.len() < 12 {
        return None;
    }
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(SourceFormat::Jpeg);
    }
    if b.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Some(SourceFormat::Png);
    }
    if b.starts_with(b"GIF8") {
        return Some(SourceFormat::Gif);
    }
    if b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        return Some(SourceFormat::Webp);
    }
    if &b[4..8] == b"ftyp" {
        return match &b[8..12] {
            b"avif" | b"avis" => Some(SourceFormat::Avif),
            b"heic" | b"heix" | b"hevc" | b"mif1" | b"msf1" => Some(SourceFormat::Heic),
            _ => None,
        };
    }
    None
}

/// A WebP whose upload or encode was cut short: the RIFF header announces no
/// size (encoders patch it in last) or more bytes than there are, and the
/// final chunk is incomplete. libwebp refuses such a file outright. Returns a
/// copy cut after the last complete chunk with a correct header, so that the
/// frames that did arrive can be served. `None` when the file is intact or
/// nothing usable is left.
fn repair_truncated_webp(b: &[u8]) -> Option<Bytes> {
    if b.len() < 20 || !b.starts_with(b"RIFF") || &b[8..12] != b"WEBP" {
        return None;
    }
    let declared = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize;
    if declared != 0 && declared + 8 <= b.len() {
        return None;
    }
    let (mut pos, mut images) = (12usize, 0u32);
    while pos + 8 <= b.len() {
        let size = u32::from_le_bytes([b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]]) as usize;
        // Chunks are padded to an even length.
        let end = pos + 8 + size + (size & 1);
        if end > b.len() {
            break;
        }
        if matches!(&b[pos..pos + 4], b"ANMF" | b"VP8 " | b"VP8L") {
            images += 1;
        }
        pos = end;
    }
    if images == 0 {
        return None;
    }
    let mut out = b[..pos].to_vec();
    out[4..8].copy_from_slice(&((pos - 8) as u32).to_le_bytes());
    Some(Bytes::from(out))
}

/// Format of an already encoded image, for cache objects without a Content-Type.
pub fn sniff_output_format(b: &[u8]) -> Option<Format> {
    Some(match sniff(b)? {
        SourceFormat::Jpeg => Format::Jpeg,
        SourceFormat::Png => Format::Png,
        SourceFormat::Webp => Format::Webp,
        SourceFormat::Gif => Format::Gif,
        SourceFormat::Avif => Format::Avif,
        SourceFormat::Heic => return None,
    })
}

pub struct Output {
    pub data: Bytes,
    pub format: Format,
}

struct Plan {
    /// Size the (single) frame is resized to.
    resize: (i32, i32),
    /// Final size after cropping, when it differs from `resize`.
    crop: Option<(i32, i32)>,
}

fn plan(src_w: i32, src_h: i32, o: &Options) -> Plan {
    let (sw, sh) = (src_w as f64, src_h as f64);
    let tw = (o.width as f64 * o.dpr as f64).round();
    let th = (o.height as f64 * o.dpr as f64).round();
    let px = |v: f64| (v.round() as i32).max(1);

    if tw == 0.0 && th == 0.0 {
        return Plan { resize: (src_w, src_h), crop: None };
    }

    let mut kind = o.resize;
    if kind == ResizeType::Auto {
        let same_orientation = tw > 0.0 && th > 0.0 && ((sw >= sh) == (tw >= th));
        kind = if same_orientation { ResizeType::Fill } else { ResizeType::Fit };
    }
    // Filling needs both dimensions; otherwise it is a plain fit.
    if kind == ResizeType::Fill && (tw == 0.0 || th == 0.0) {
        kind = ResizeType::Fit;
    }

    match kind {
        ResizeType::Force => {
            let mut w = if tw > 0.0 { tw } else { sw * th / sh };
            let mut h = if th > 0.0 { th } else { sh * tw / sw };
            if !o.enlarge {
                w = w.min(sw);
                h = h.min(sh);
            }
            Plan { resize: (px(w), px(h)), crop: None }
        }
        ResizeType::Fill => {
            let mut scale = (tw / sw).max(th / sh);
            if !o.enlarge {
                scale = scale.min(1.0);
            }
            let rw = ((sw * scale).ceil() as i32).max(1);
            let rh = ((sh * scale).ceil() as i32).max(1);
            let cw = (tw as i32).min(rw).max(1);
            let ch = (th as i32).min(rh).max(1);
            Plan { resize: (rw, rh), crop: ((cw, ch) != (rw, rh)).then_some((cw, ch)) }
        }
        _ => {
            let mut scale = f64::INFINITY;
            if tw > 0.0 {
                scale = scale.min(tw / sw);
            }
            if th > 0.0 {
                scale = scale.min(th / sh);
            }
            if !o.enlarge {
                scale = scale.min(1.0);
            }
            Plan { resize: (px(sw * scale), px(sh * scale)), crop: None }
        }
    }
}

fn crop_offset(g: Gravity, from: (i32, i32), to: (i32, i32)) -> (i32, i32) {
    let dx = from.0 - to.0;
    let dy = from.1 - to.1;
    match g {
        Gravity::Center | Gravity::Smart => (dx / 2, dy / 2),
        Gravity::North => (dx / 2, 0),
        Gravity::South => (dx / 2, dy),
        Gravity::East => (dx, dy / 2),
        Gravity::West => (0, dy / 2),
        Gravity::NorthEast => (dx, 0),
        Gravity::NorthWest => (0, 0),
        Gravity::SouthEast => (dx, dy),
        Gravity::SouthWest => (0, dy),
    }
}

/// Crop to `to`, anchored by gravity. Animations are cropped frame by frame
/// and stacked back together.
fn crop_image(img: Image, frames: i32, g: Gravity, to: (i32, i32), in_memory: bool) -> Result<Image, ProcError> {
    let frame_w = img.width();
    let frame_h = img.height() / frames;
    let cw = to.0.clamp(1, frame_w);
    let ch = to.1.clamp(1, frame_h);
    if (cw, ch) == (frame_w, frame_h) {
        return Ok(img);
    }
    let (left, top) = crop_offset(g, (frame_w, frame_h), (cw, ch));

    if frames > 1 {
        let mut parts: Vec<Image> = Vec::with_capacity(frames as usize);
        for i in 0..frames {
            let mut f: *mut VipsImage = ptr::null_mut();
            check(unsafe { vips_extract_area(img.0, &mut f, left, i * frame_h + top, cw, ch, END) })?;
            parts.push(Image(f));
        }
        let mut raw: Vec<*mut VipsImage> = parts.iter().map(|p| p.0).collect();
        let mut joined: *mut VipsImage = ptr::null_mut();
        check(unsafe { vips_arrayjoin(raw.as_mut_ptr(), &mut joined, frames, c"across".as_ptr(), 1 as c_int, END) })?;
        let out = Image(joined);
        unsafe { vips_image_set_int(out.0, c"page-height".as_ptr(), ch) };
        return Ok(out);
    }

    let mut c: *mut VipsImage = ptr::null_mut();
    if g == Gravity::Smart {
        // Smart crop needs random access to the pixels.
        let mem = if in_memory { img } else { Image::wrap(unsafe { vips_image_copy_memory(img.0) })? };
        check(unsafe { vips_smartcrop(mem.0, &mut c, cw, ch, c"interesting".as_ptr(), VIPS_INTERESTING_ATTENTION, END) })?;
    } else {
        check(unsafe { vips_extract_area(img.0, &mut c, left, top, cw, ch, END) })?;
    }
    Ok(Image(c))
}

fn pick_format(src: SourceFormat, animated: bool, explicit: Option<Format>, neg: Negotiated) -> Format {
    if let Some(f) = explicit {
        return f;
    }
    if animated {
        // Never silently drop an animation: AVIF output would be a still.
        return if neg.webp || src == SourceFormat::Webp { Format::Webp } else { Format::Gif };
    }
    if neg.avif {
        return Format::Avif;
    }
    if neg.webp {
        return Format::Webp;
    }
    match src {
        SourceFormat::Jpeg | SourceFormat::Heic => Format::Jpeg,
        SourceFormat::Png => Format::Png,
        SourceFormat::Webp => Format::Webp,
        SourceFormat::Gif => Format::Gif,
        SourceFormat::Avif => Format::Avif,
    }
}

/// Decode, transform and encode. Blocking and CPU bound: call from a
/// blocking thread.
pub fn process(src: &Bytes, o: &Options, neg: Negotiated, cfg: &Config) -> Result<Output, ProcError> {
    let src_format = sniff(src).ok_or(ProcError::Unsupported)?;

    let repaired;
    let src = match (src_format, repair_truncated_webp(src)) {
        (SourceFormat::Webp, Some(fixed)) => {
            tracing::debug!(from = src.len(), to = fixed.len(), "truncated WebP source: keeping its complete frames");
            repaired = fixed;
            &repaired
        }
        _ => src,
    };

    // Header-only load: cheap, and tells us what we are dealing with before
    // any pixel is decoded.
    let (mut src_w, mut src_h, n_pages) = {
        let head = Image::wrap(unsafe { vips_image_new_from_buffer(src.as_ptr() as *const c_void, src.len(), c"".as_ptr(), END) })?;
        let orientation = unsafe { vips_image_get_orientation(head.0) };
        let pages = unsafe { vips_image_get_n_pages(head.0) }.max(1);
        let (w, h) = (head.width(), head.height());
        // Orientations 5-8 are rotated by 90 degrees once auto-rotated.
        if (5..=8).contains(&orientation) { (h, w, pages) } else { (w, h, pages) }
    };
    if src_w < 1 || src_h < 1 {
        return Err(ProcError::Vips("image has no pixels".into()));
    }
    src_w = src_w.max(1);
    src_h = src_h.max(1);

    let frame_pixels = src_w as u64 * src_h as u64;
    if frame_pixels > cfg.max_src_pixels {
        return Err(ProcError::TooLarge(format!(
            "source resolution {src_w}x{src_h} exceeds the limit of {:.0} MP",
            cfg.max_src_pixels as f64 / 1e6
        )));
    }

    let animated_source = n_pages > 1 && matches!(src_format, SourceFormat::Gif | SourceFormat::Webp);
    let format = pick_format(src_format, animated_source, o.format, neg);
    let animated = animated_source && format.supports_animation();
    let frames = if animated { (n_pages as u32).min(cfg.max_animation_frames) as i32 } else { 1 };
    let animated = frames > 1;

    if animated && frame_pixels * frames as u64 > cfg.max_animation_pixels {
        return Err(ProcError::TooLarge(format!(
            "animation of {frames} frames at {src_w}x{src_h} exceeds the limit of {:.0} MP in total",
            cfg.max_animation_pixels as f64 / 1e6
        )));
    }

    // Optional crop of the source (`c:` option), applied before resizing.
    let pre_crop = o.crop.and_then(|c| {
        let dim = |v: f32, full: i32| -> i32 {
            if v <= 0.0 {
                full
            } else if v < 1.0 {
                ((full as f32 * v).round() as i32).clamp(1, full)
            } else {
                (v.round() as i32).min(full)
            }
        };
        let size = (dim(c.width, src_w), dim(c.height, src_h));
        (size != (src_w, src_h)).then_some((size, c.gravity.unwrap_or(o.gravity)))
    });
    let (base_w, base_h) = pre_crop.map(|(s, _)| s).unwrap_or((src_w, src_h));

    let plan = plan(base_w, base_h, o);
    let (rw, rh) = plan.resize;
    let load_options = CString::new(if animated { format!("n={frames}") } else { String::new() }).unwrap();

    let mut out: *mut VipsImage = ptr::null_mut();
    if let Some((size, gravity)) = pre_crop {
        // Decode, crop, then resize the cropped region.
        let mut loaded = Image::wrap(unsafe {
            vips_image_new_from_buffer(src.as_ptr() as *const c_void, src.len(), load_options.as_ptr(), END)
        })?;
        if !animated {
            let mut r: *mut VipsImage = ptr::null_mut();
            check(unsafe { vips_autorot(loaded.0, &mut r, END) })?;
            loaded = Image(r);
        }
        let cropped = crop_image(loaded, frames, gravity, size, true)?;
        check(unsafe {
            vips_thumbnail_image(cropped.0, &mut out, rw as c_int, c"height".as_ptr(), rh as c_int, c"size".as_ptr(), VIPS_SIZE_FORCE, END)
        })?;
    } else {
        // vips_thumbnail_buffer picks the fastest strategy per format (JPEG
        // DCT shrink-on-load, WebP scale-on-load...), applies EXIF orientation
        // and converts to sRGB. With n>1 it resizes every frame and keeps
        // frame boundaries on whole pixels.
        check(unsafe {
            vips_thumbnail_buffer(
                src.as_ptr() as *mut c_void,
                src.len(),
                &mut out,
                rw as c_int,
                c"height".as_ptr(),
                rh as c_int,
                c"size".as_ptr(),
                VIPS_SIZE_FORCE,
                c"option_string".as_ptr(),
                load_options.as_ptr(),
                END,
            )
        })?;
    }
    let mut img = Image(out);

    if animated && img.height() % frames != 0 {
        return Err(ProcError::Vips("animation frames are not evenly sized after resize".into()));
    }
    if let Some(size) = plan.crop {
        img = crop_image(img, frames, o.gravity, size, false)?;
    }

    if o.blur > 0.0 {
        let mut b: *mut VipsImage = ptr::null_mut();
        check(unsafe { vips_gaussblur(img.0, &mut b, o.blur as c_double, END) })?;
        img = Image(b);
    }
    if o.sharpen > 0.0 {
        let mut s: *mut VipsImage = ptr::null_mut();
        check(unsafe { vips_sharpen(img.0, &mut s, c"sigma".as_ptr(), o.sharpen as c_double, END) })?;
        img = Image(s);
    }

    // JPEG has no alpha channel; an explicit background flattens any format.
    if img.has_alpha() && (format == Format::Jpeg || o.background.is_some()) {
        let bg = o.background.unwrap_or([255, 255, 255]);
        let rgb = [bg[0] as c_double, bg[1] as c_double, bg[2] as c_double];
        let mut f: *mut VipsImage = ptr::null_mut();
        let code = unsafe {
            let arr = vips_array_double_new(rgb.as_ptr(), 3);
            let code = vips_flatten(img.0, &mut f, c"background".as_ptr(), arr, END);
            vips_area_unref(arr);
            code
        };
        check(code)?;
        img = Image(f);
    }

    let quality = |default: u8| -> c_int { c_int::from(if o.quality > 0 { o.quality } else { default }) };
    let mut buf: *mut c_void = ptr::null_mut();
    let mut len: usize = 0;
    let strip = c"strip".as_ptr();
    let yes: c_int = 1;

    check(unsafe {
        match format {
            Format::Jpeg => vips_jpegsave_buffer(
                img.0,
                &mut buf,
                &mut len,
                c"Q".as_ptr(),
                quality(cfg.quality),
                c"optimize_coding".as_ptr(),
                yes,
                c"interlace".as_ptr(),
                yes,
                strip,
                yes,
                END,
            ),
            Format::Png => vips_pngsave_buffer(img.0, &mut buf, &mut len, c"compression".as_ptr(), 6 as c_int, strip, yes, END),
            Format::Webp => vips_webpsave_buffer(
                img.0,
                &mut buf,
                &mut len,
                c"Q".as_ptr(),
                quality(cfg.quality),
                c"effort".as_ptr(),
                cfg.webp_effort as c_int,
                strip,
                yes,
                END,
            ),
            Format::Gif => vips_gifsave_buffer(img.0, &mut buf, &mut len, strip, yes, END),
            Format::Avif => vips_heifsave_buffer(
                img.0,
                &mut buf,
                &mut len,
                c"Q".as_ptr(),
                quality(cfg.avif_quality),
                c"compression".as_ptr(),
                VIPS_FOREIGN_HEIF_COMPRESSION_AV1,
                c"effort".as_ptr(),
                cfg.avif_effort as c_int,
                strip,
                yes,
                END,
            ),
        }
    })?;

    if buf.is_null() || len == 0 {
        return Err(ProcError::Vips("encoder produced no output".into()));
    }
    Ok(Output { data: Bytes::from_owner(VipsBuf { ptr: buf as *mut u8, len }), format })
}
