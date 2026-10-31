// Kitty graphics: the PNG decoder libghostty calls, and reading a terminal's
// image storage back out.
use std::ffi::c_void;
use std::sync::Once;

use crate::{ffi, Terminal};

/// The Kitty image storage limit of each screen of a terminal that keeps
/// images. Holder and mirror must agree: a mirror with less room would
/// evict images the holder still places.
pub const IMAGE_BYTES: u64 = 64 << 20;

static PNG: Once = Once::new();

/// Installs the process-wide PNG decoder, once, before any terminal can
/// receive an image.
pub(crate) fn install_png() {
    PNG.call_once(|| unsafe { ffi::osewa_png_install() });
}

/// Decodes PNG data to 8-bit RGBA. The png crate's limits leave out the
/// frame buffer we give it, so a small file claiming huge dimensions is
/// refused here, before either buffer is allocated: an image past the
/// storage limit would be evicted anyway.
fn decode_png(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut d = png::Decoder::new(std::io::Cursor::new(data));
    d.set_transformations(png::Transformations::normalize_to_color8());
    let mut r = d.read_info().ok()?;
    let (w, h) = (r.info().width as u64, r.info().height as u64);
    let size = r.output_buffer_size()?;
    if w * h * 4 > IMAGE_BYTES || size as u64 > IMAGE_BYTES {
        return None;
    }
    let mut buf = vec![0; size];
    let info = r.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let px = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => px.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    (rgba.len() == w as usize * h as usize * 4).then_some((w, h, rgba))
}

#[no_mangle]
extern "C" fn osewa_rs_decode_png(
    alloc: *const c_void,
    data: *const u8,
    len: usize,
    w: *mut u32,
    h: *mut u32,
    px: *mut *mut u8,
    px_len: *mut usize,
) -> bool {
    // A panic must not unwind into C.
    let decoded = std::panic::catch_unwind(|| decode_png(crate::bytes(data, len))).ok().flatten();
    let Some((width, height, rgba)) = decoded else { return false };
    let out = unsafe { ffi::osewa_png_alloc(alloc, rgba.len()) };
    if out.is_null() {
        return false;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(rgba.as_ptr(), out, rgba.len());
        *w = width;
        *h = height;
        *px = out;
        *px_len = rgba.len();
    }
    true
}

/// One placement of an image, with its values as the program sent them and
/// where it sits now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Placement {
    pub image: u32,
    /// 0 when the program gave none.
    pub placement: u32,
    /// Pixel offsets within the first cell (X, Y).
    pub x_offset: u32,
    pub y_offset: u32,
    /// The part of the image shown (x, y, w, h); zero width or height means
    /// all of it.
    pub source: (u32, u32, u32, u32),
    /// The columns and rows the program asked for (c, r), 0 for the
    /// image's own size.
    pub cols: u32,
    pub rows: u32,
    pub z: i32,
    /// The cells it covers.
    pub grid: (u32, u32),
    /// The top-left cell in the viewport, when visible. row is negative
    /// when the placement's top scrolled above it.
    pub col: i32,
    pub row: i32,
    pub visible: bool,
    /// A Unicode placeholder placement (U=1), drawn wherever its
    /// placeholder cells are.
    pub is_virtual: bool,
}

/// How an image's pixels are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Rgb,
    Rgba,
    GrayAlpha,
    Gray,
}

/// An image in a terminal's storage. Its pixels are borrowed from the
/// terminal.
#[derive(Debug, Clone, Copy)]
pub struct Image<'a> {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// None while they are still pending: restored from a snapshot that
    /// ended before they came.
    pub pixels: Option<&'a [u8]>,
    /// Changes whenever the pixels do.
    pub generation: u64,
}

#[no_mangle]
extern "C" fn osewa_rs_placement(ud: *mut c_void, v: *const u32, z: i32, col: i32, row: i32, visible: bool, is_virtual: bool) {
    let out = unsafe { &mut *(ud as *mut Vec<Placement>) };
    let v = unsafe { std::slice::from_raw_parts(v, 12) };
    out.push(Placement {
        image: v[0],
        placement: v[1],
        x_offset: v[2],
        y_offset: v[3],
        source: (v[4], v[5], v[6], v[7]),
        cols: v[8],
        rows: v[9],
        z,
        grid: (v[10], v[11]),
        col,
        row,
        visible,
        is_virtual,
    });
}

impl Terminal {
    /// Every placement of the active screen's images.
    pub fn placements(&self) -> Vec<Placement> {
        let mut out: Vec<Placement> = Vec::new();
        unsafe { ffi::osewa_placements(self.t, (&mut out as *mut Vec<Placement>).cast()) };
        out
    }

    /// The active screen's image with this id.
    pub fn image(&self, id: u32) -> Option<Image<'_>> {
        let (mut w, mut h, mut f, mut len, mut generation) = (0u32, 0u32, 0i32, 0usize, 0u64);
        let mut px: *const u8 = std::ptr::null();
        let r = unsafe { ffi::osewa_image(self.t, id, &mut w, &mut h, &mut f, &mut px, &mut len, &mut generation) };
        if r != 0 {
            return None;
        }
        let format = match f {
            0 => PixelFormat::Rgb,
            3 => PixelFormat::GrayAlpha,
            4 => PixelFormat::Gray,
            _ => PixelFormat::Rgba,
        };
        let pixels = (!px.is_null()).then(|| crate::bytes(px, len));
        Some(Image { id, width: w, height: h, format, pixels, generation })
    }
}
