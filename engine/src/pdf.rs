//! PDFs through PDFium (Chromium's PDF library, prebuilt in Vendor/pdfium): pages as text, or as pictures when they
//! have little text (scans, slides) or text read in glyph order. PDFium is one process-wide library and not
//! thread-safe: every use goes through `with_document`, under one lock.
use crate::embed::Rgb;
use crate::extract::ExtractError;
use crate::ocr;
use crate::text::PageTextProblem;
use std::ffi::{CString, c_char, c_int, c_ulong, c_ushort, c_void};
use std::sync::{Mutex, Once};

type Handle = *mut c_void;

unsafe extern "C" {
    fn FPDF_InitLibrary();
    fn FPDF_LoadDocument(path: *const c_char, password: *const c_char) -> Handle;
    fn FPDF_GetLastError() -> c_ulong;
    fn FPDF_CloseDocument(document: Handle);
    fn FPDF_GetPageCount(document: Handle) -> c_int;
    fn FPDF_LoadPage(document: Handle, index: c_int) -> Handle;
    fn FPDF_ClosePage(page: Handle);
    fn FPDFText_LoadPage(page: Handle) -> Handle;
    fn FPDFText_ClosePage(text: Handle);
    fn FPDFText_CountChars(text: Handle) -> c_int;
    fn FPDFText_GetText(text: Handle, start: c_int, count: c_int, result: *mut c_ushort) -> c_int;
    fn FPDF_GetPageWidthF(page: Handle) -> f32;
    fn FPDF_GetPageHeightF(page: Handle) -> f32;
    fn FPDFBitmap_Create(width: c_int, height: c_int, alpha: c_int) -> Handle;
    fn FPDFBitmap_FillRect(bitmap: Handle, left: c_int, top: c_int, width: c_int, height: c_int, color: c_ulong) -> c_int;
    fn FPDF_RenderPageBitmap(bitmap: Handle, page: Handle, x: c_int, y: c_int, width: c_int, height: c_int,
                             rotate: c_int, flags: c_int);
    fn FPDFBitmap_GetBuffer(bitmap: Handle) -> *mut c_void;
    fn FPDFBitmap_GetStride(bitmap: Handle) -> c_int;
    fn FPDFBitmap_Destroy(bitmap: Handle);
    fn FPDF_GetMetaText(document: Handle, tag: *const c_char, buffer: *mut c_void, length: c_ulong) -> c_ulong;
}

const FPDF_ERR_PASSWORD: c_ulong = 4;
const FPDF_ANNOT: c_int = 0x01;

static LOCK: Mutex<()> = Mutex::new(());
static INIT: Once = Once::new();

pub struct Page {
    pub number: usize,           // 1-based
    pub text: String,            // may be empty for scans
    /// The page as a picture, when it had too little text (scans, slides) or text read in glyph order.
    pub image: Option<Rgb>,
    /// Text read in glyph order: too garbled to show or match words against, but enough of its words survive for a
    /// meaning vector beside the page's picture.
    pub garbled: Option<String>,
    /// The text came from OCR (a scan, or a text layer that lost letters).
    pub ocr: bool,
}

/// Runs `body` with the document open: `Locked` when it needs a password, `Unreadable` when it won't open.
fn with_document<T>(path: &str, body: impl FnOnce(Handle) -> Result<T, ExtractError>) -> Result<T, ExtractError> {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    INIT.call_once(|| unsafe { FPDF_InitLibrary() });
    let c_path = CString::new(path).map_err(|_| ExtractError::Unreadable("bad path".into()))?;
    let document = unsafe { FPDF_LoadDocument(c_path.as_ptr(), std::ptr::null()) };
    if document.is_null() {
        return Err(if unsafe { FPDF_GetLastError() } == FPDF_ERR_PASSWORD { ExtractError::Locked } else {
            ExtractError::Unreadable("can't open PDF".into())
        });
    }
    let result = body(document);
    unsafe { FPDF_CloseDocument(document) };
    result
}

/// The page's text in reading order, lines ending in "\n".
fn page_text(page: Handle) -> String {
    unsafe {
        let text_page = FPDFText_LoadPage(page);
        if text_page.is_null() {
            return String::new();
        }
        let count = FPDFText_CountChars(text_page);
        let mut out = String::new();
        if count > 0 {
            let mut buffer = vec![0u16; count as usize + 1];
            let written = FPDFText_GetText(text_page, 0, count, buffer.as_mut_ptr());
            if written > 1 {
                out = String::from_utf16_lossy(&buffer[..written as usize - 1]).replace("\r\n", "\n").replace('\r', "\n");
            }
        }
        FPDFText_ClosePage(text_page);
        out
    }
}

/// The page as a picture, fitted into a square of `pixels`, annotations drawn.
fn render(page: Handle, pixels: usize) -> Option<Rgb> {
    unsafe {
        let (width, height) = (FPDF_GetPageWidthF(page) as f64, FPDF_GetPageHeightF(page) as f64);
        if width <= 0.0 || height <= 0.0 {
            return None;
        }
        let scale = pixels as f64 / width.max(height);
        let (w, h) = (((width * scale).round() as usize).max(1), ((height * scale).round() as usize).max(1));
        let bitmap = FPDFBitmap_Create(w as c_int, h as c_int, 0);
        if bitmap.is_null() {
            return None;
        }
        FPDFBitmap_FillRect(bitmap, 0, 0, w as c_int, h as c_int, 0xFFFF_FFFF);
        FPDF_RenderPageBitmap(bitmap, page, 0, 0, w as c_int, h as c_int, 0, FPDF_ANNOT);
        let buffer = FPDFBitmap_GetBuffer(bitmap) as *const u8;
        let stride = FPDFBitmap_GetStride(bitmap) as usize;
        let mut rgb = vec![0u8; w * h * 3];
        if !buffer.is_null() {
            let bytes = std::slice::from_raw_parts(buffer, stride * h);   // BGRx
            for y in 0..h {
                for x in 0..w {
                    let (s, d) = (y * stride + x * 4, (y * w + x) * 3);
                    rgb[d] = bytes[s + 2];
                    rgb[d + 1] = bytes[s + 1];
                    rgb[d + 2] = bytes[s];
                }
            }
        }
        FPDFBitmap_Destroy(bitmap);
        (!buffer.is_null()).then_some(Rgb { width: w, height: h, pixels: rgb })
    }
}

/// A metadata string ("Title", "Author"…), or None.
fn meta(document: Handle, tag: &str) -> Option<String> {
    let tag = CString::new(tag).ok()?;
    unsafe {
        let size = FPDF_GetMetaText(document, tag.as_ptr(), std::ptr::null_mut(), 0);
        if size <= 2 {
            return None;
        }
        let mut buffer = vec![0u16; size as usize / 2];
        FPDF_GetMetaText(document, tag.as_ptr(), buffer.as_mut_ptr() as *mut c_void, size);
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        let text = String::from_utf16_lossy(&buffer[..end]);
        (!text.trim().is_empty()).then_some(text)
    }
}

/// Reads the pages in `range(page_count)` (0-based). Pages with real text become text; scans, slides and pages whose
/// text layer came out in glyph order are rendered (`render_pixels`), then `prepare` sizes the picture for the model
/// (None: a page too thin to embed, left out).
pub fn pages(path: &str, range: impl FnOnce(usize) -> std::ops::Range<usize>, min_text: usize, render_pixels: usize,
             prepare: impl Fn(Rgb) -> Option<Rgb>) -> Result<(Vec<Page>, usize, Option<String>), ExtractError> {
    with_document(path, |document| {
        let total = unsafe { FPDF_GetPageCount(document) }.max(0) as usize;
        let wanted = range(total);
        let mut pages = vec![];
        for index in wanted.start.min(total)..wanted.end.min(total) {
            let page = unsafe { FPDF_LoadPage(document, index as c_int) };
            if page.is_null() {
                continue;
            }
            let mut text = page_text(page).trim().to_string();
            let mut garbled = None;
            let mut done = false;
            match PageTextProblem::check(&text) {
                Some(PageTextProblem::GlyphOrder) => garbled = Some(std::mem::take(&mut text)),
                problem => {
                    // Letters lost to a lam ligature: OCR's reading replaces the layer when it's real Arabic.
                    if problem == Some(PageTextProblem::LostLetters) && ocr::reads_arabic() {
                        let read = render(page, 2000).map(|image| ocr::text_of_image(&image)).unwrap_or_default();
                        if read.chars().count() >= min_text && PageTextProblem::check(&read).is_none()
                            && read.chars().any(|c| (0x0600..=0x06FF).contains(&(c as u32))) {
                            pages.push(Page { number: index + 1, text: read, image: None, garbled: None, ocr: true });
                            done = true;
                        }
                    }
                    if !done && text.chars().count() >= min_text {
                        pages.push(Page { number: index + 1, text: text.clone(), image: None, garbled: None, ocr: false });
                        done = true;
                    }
                }
            }
            if !done {
                if let Some(image) = render(page, render_pixels) {
                    // A scan, or a page that's mostly a picture: its words come from OCR, for keyword search only (an
                    // invoice number on a scan), and its meaning stays the picture's. Not for a layer read in glyph
                    // order (Bengali), which OCR can't read either.
                    let mut ocr = false;
                    if garbled.is_none() && ocr::available() {
                        let read = ocr::text_of_image(&image);
                        if read.chars().count() > text.chars().count() {
                            (text, ocr) = (read, true);
                        }
                    }
                    if let Some(image) = prepare(image) {
                        pages.push(Page { number: index + 1, text, image: Some(image), garbled, ocr });
                    }
                }
            }
            unsafe { FPDF_ClosePage(page) };
        }
        Ok((pages, total, meta(document, "Title")))
    })
}

/// Page `number` (from 1) as a picture for a preview, fitted into a square of `pixels`.
pub fn render_page(path: &str, number: usize, pixels: usize) -> Result<Rgb, ExtractError> {
    with_document(path, |document| {
        let page = unsafe { FPDF_LoadPage(document, number.saturating_sub(1) as c_int) };
        if page.is_null() {
            return Err(ExtractError::Unreadable(format!("no page {number}")));
        }
        let image = render(page, pixels);
        unsafe { FPDF_ClosePage(page) };
        image.ok_or_else(|| ExtractError::Unreadable(format!("can't render page {number}")))
    })
}

pub fn page_count(path: &str) -> usize {
    with_document(path, |document| Ok(unsafe { FPDF_GetPageCount(document) }.max(1) as usize)).unwrap_or(1)
}
