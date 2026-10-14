//! Reading files for the model: pictures (decoded, turned the way the camera held them, sized for the model), word
//! processor and web documents, and sound and video through ffmpeg.
use crate::embed::{self, Rgb};
use crate::paths;
use regex::Regex;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

#[derive(Debug)]
pub enum ExtractError {
    Unreadable(String),
    TooSmall(usize, usize),
    Locked,
    /// Not for the index after all, once read (code that a program made, or that holds a private key).
    LeftOut(String),
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Unreadable(why) => write!(f, "unreadable: {why}"),
            Self::TooSmall(width, height) => write!(f, "small image {width}×{height}"),
            Self::Locked => f.write_str("password-protected"),
            Self::LeftOut(why) => f.write_str(why),
        }
    }
}

impl From<std::io::Error> for ExtractError {
    fn from(error: std::io::Error) -> Self {
        Self::Unreadable(error.to_string())
    }
}

// MARK: Pictures

/// A picture ready for the model, and what it was.
pub struct PreparedImage {
    pub image: Rgb,
    /// Its size as shown, before scaling.
    pub width: usize,
    pub height: usize,
    /// EXIF DateTimeOriginal, as written by the camera.
    pub taken: Option<String>,
}

/// The way the camera held the picture (1–8) and when it took it.
fn exif(path: &str) -> (u32, Option<String>) {
    let Ok(file) = std::fs::File::open(path) else { return (1, None) };
    let Ok(data) = exif::Reader::new().read_from_container(&mut std::io::BufReader::new(file)) else { return (1, None) };
    let orientation = data.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|field| field.value.get_uint(0)).unwrap_or(1);
    let taken = data.get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY).and_then(|field| match &field.value {
        exif::Value::Ascii(values) => values.first().map(|v| String::from_utf8_lossy(v).into_owned()),
        _ => None,
    });
    (orientation, taken)
}

fn oriented(image: image::RgbImage, orientation: u32) -> image::RgbImage {
    use image::imageops::{flip_horizontal, flip_vertical, rotate90, rotate180, rotate270};
    match orientation {
        2 => flip_horizontal(&image),
        3 => rotate180(&image),
        4 => flip_vertical(&image),
        5 => flip_horizontal(&rotate90(&image)),
        6 => rotate90(&image),
        7 => flip_horizontal(&rotate270(&image)),
        8 => rotate270(&image),
        _ => image,
    }
}

/// Decodes a picture: JPEG, PNG, GIF, BMP, WebP, TIFF, PNM and TGA here; HEIC and AVIF through ffmpeg.
pub fn decode(path: &str, min_side: usize) -> Result<(image::RgbImage, Option<String>), ExtractError> {
    let ext = paths::extension(path);
    if ["cr2", "nef", "arw", "dng", "raf", "orf", "rw2"].contains(&ext.as_str()) {
        return Err(ExtractError::Unreadable(format!("RAW pictures ({}) aren't read on this system yet",
                                                    ext.to_uppercase())));
    }
    let (orientation, taken) = exif(path);
    if let Ok((width, height)) = image::image_dimensions(path) {
        let (width, height) = if orientation >= 5 { (height, width) } else { (width, height) };
        if (width.max(height) as usize) < min_side {
            return Err(ExtractError::TooSmall(width as usize, height as usize));
        }
        let decoded = image::ImageReader::open(path)?.with_guessed_format()?.decode()
            .map_err(|_| ExtractError::Unreadable("can't decode image".into()))?;
        return Ok((oriented(decoded.into_rgb8(), orientation), taken));
    }
    let image = decode_with_ffmpeg(path)?;
    if (image.width().max(image.height()) as usize) < min_side {
        return Err(ExtractError::TooSmall(image.width() as usize, image.height() as usize));
    }
    Ok((image, taken))
}

/// The whole picture through ffmpeg as 8-bit RGB (PPM down a pipe).
fn decode_with_ffmpeg(path: &str) -> Result<image::RgbImage, ExtractError> {
    let output = ffmpeg_output(&["-v", "error", "-nostdin", "-i", path, "-frames:v", "1", "-pix_fmt", "rgb24", "-f",
                                 "image2pipe", "-c:v", "ppm", "-"]);
    let data = output.map_err(|_| {
        let ext = paths::extension(path);
        ExtractError::Unreadable(if ext == "heic" || ext == "heif" { "this ffmpeg can't read HEIC (8.1 can)".into() } else {
            "can't decode image".into()
        })
    })?;
    image::load_from_memory_with_format(&data, image::ImageFormat::Pnm).map(|i| i.into_rgb8())
        .map_err(|_| ExtractError::Unreadable("can't decode image".into()))
}

/// Scaled so its long side is at most `max_pixel` (area averaging: fast, and fine for shrinking).
pub fn fit(image: image::RgbImage, max_pixel: usize) -> image::RgbImage {
    let longest = image.width().max(image.height()) as usize;
    if longest <= max_pixel {
        return image;
    }
    let scale = max_pixel as f64 / longest as f64;
    let width = ((image.width() as f64 * scale).round() as u32).max(1);
    let height = ((image.height() as f64 * scale).round() as u32).max(1);
    image::imageops::thumbnail(&image, width, height)
}

pub fn rgb(image: image::RgbImage) -> Rgb {
    Rgb { width: image.width() as usize, height: image.height() as usize, pixels: image.into_raw() }
}

/// Sized for the model's `budget` (vision soft tokens) the way the reference sizes it, ready to embed.
pub fn for_model(image: Rgb, budget: usize) -> Result<Rgb, ExtractError> {
    let (height, width) = embed::target_size(image.height, image.width, budget)
        .ok_or_else(|| ExtractError::Unreadable("image too thin to embed".into()))?;
    if (height, width) == (image.height, image.width) {
        return Ok(image);
    }
    let pixels = embed::resize_bicubic(&image.pixels, image.width, image.height, width, height);
    Ok(Rgb { width, height, pixels })
}

/// A picture read, turned, scaled to `max_pixel` (as DigUp's Mac app's ImageIO thumbnail is) and sized for `budget`.
pub fn prepare_image(path: &str, max_pixel: usize, min_side: usize, budget: usize) -> Result<PreparedImage, ExtractError> {
    let (image, taken) = decode(path, min_side)?;
    let (width, height) = (image.width() as usize, image.height() as usize);
    let image = for_model(rgb(fit(image, max_pixel)), budget)?;
    Ok(PreparedImage { image, width, height, taken })
}

/// 8×8 average hash: gray cells brighter than the mean are 1.
pub fn average_hash(image: &image::RgbImage) -> u64 {
    let (width, height) = (image.width() as usize, image.height() as usize);
    let side = 8;
    let mut cells = [0usize; 64];
    for cy in 0..side {
        let y0 = cy * height / side;
        let y1 = height.min((y0 + 1).max((cy + 1) * height / side));
        for cx in 0..side {
            let x0 = cx * width / side;
            let x1 = width.min((x0 + 1).max((cx + 1) * width / side));
            let (mut sum, mut n) = (0usize, 0usize);
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = image.get_pixel(x as u32, y as u32);
                    sum += (p[0] as usize * 299 + p[1] as usize * 587 + p[2] as usize * 114) / 1000;
                    n += 1;
                }
            }
            cells[cy * side + cx] = if n > 0 { sum / n } else { 0 };
        }
    }
    let mean = cells.iter().sum::<usize>() / 64;
    cells.iter().enumerate().filter(|(_, cell)| **cell > mean).fold(0u64, |hash, (index, _)| hash | 1 << index)
}

// MARK: Documents

/// Plain text from txt/md directly, HTML by stripping tags, docx and odt from the XML inside their zip, rtf stripped
/// of its control words. Legacy .doc isn't read.
pub fn document_text(path: &str) -> Result<String, ExtractError> {
    match paths::extension(path).as_str() {
        "docx" => Ok(plain_text_from_html(&as_html(&zip_entry(path, "word/document.xml")?, &["w:p"],
                                                   &["w:br", "w:cr"], &["w:tab"]))),
        "odt" => Ok(plain_text_from_html(&as_html(&zip_entry(path, "content.xml")?, &["text:p", "text:h"],
                                                  &["text:line-break"], &["text:tab", "text:s"]))),
        "rtf" => Ok(crate::rtf::text(&read_text(path)?)),
        "doc" => Err(ExtractError::Unreadable("legacy Word (.doc) files aren't read on this system".into())),
        "html" | "htm" => Ok(plain_text_from_html(&read_text(path)?)),
        _ => read_text(path),
    }
}

/// UTF-8 (or UTF-16 with its byte-order mark), else Latin-1.
pub fn read_text(path: &str) -> Result<String, ExtractError> {
    let bytes = std::fs::read(path)?;
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return Ok(String::from_utf8_lossy(rest).into_owned());
    }
    let utf16 = |rest: &[u8], little: bool| -> String {
        let units: Vec<u16> = rest.chunks_exact(2)
            .map(|pair| if little { u16::from_le_bytes([pair[0], pair[1]]) } else { u16::from_be_bytes([pair[0], pair[1]]) })
            .collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return Ok(utf16(rest, true));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return Ok(utf16(rest, false));
    }
    match String::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(error) => Ok(error.into_bytes().iter().map(|&b| b as char).collect()),
    }
}

fn zip_entry(path: &str, name: &str) -> Result<String, ExtractError> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|_| ExtractError::Unreadable("not a zip file".into()))?;
    let mut entry = archive.by_name(name).map_err(|_| ExtractError::Unreadable(format!("no {name} inside")))?;
    let mut bytes = vec![];
    entry.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Word-processing XML as the little HTML `plain_text_from_html` reads: paragraph ends become `</p>`, breaks `<br>`,
/// tabs and spaces a space; every other tag is stripped there.
fn as_html(xml: &str, paragraphs: &[&str], breaks: &[&str], spaces: &[&str]) -> String {
    let mut text = xml.to_string();
    for tag in paragraphs {
        text = text.replace(&format!("</{tag}>"), "</p>");
    }
    for tag in breaks {
        text = Regex::new(&format!(r"<{}\b[^>]*/?>", regex::escape(tag))).unwrap().replace_all(&text, "<br>").into_owned();
    }
    for tag in spaces {
        text = Regex::new(&format!(r"<{}\b[^>]*/?>", regex::escape(tag))).unwrap().replace_all(&text, " ").into_owned();
    }
    text
}

static HTML_DROPS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [r"(?is)<script\b.*?</script\s*>", r"(?is)<style\b.*?</style\s*>", r"(?s)<!--.*?-->", r"(?is)<head\b.*?</head\s*>"]
        .iter().map(|p| Regex::new(p).unwrap()).collect()
});
static HTML_BREAKS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<(br|/p|/div|/li|/h[1-6]|/tr|/section|/article)\b[^>]*>").unwrap());
static TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
static NUMERIC_ENTITY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&#(x[0-9a-fA-F]+|[0-9]+);").unwrap());
static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]+").unwrap());
static LINE_BREAKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\n\s*").unwrap());

/// Good enough for search: drops scripts, styles and comments, turns block ends into line breaks, strips the
/// remaining tags, and decodes common entities.
pub fn plain_text_from_html(html: &str) -> String {
    let mut text = html.to_string();
    for pattern in HTML_DROPS.iter() {
        text = pattern.replace_all(&text, " ").into_owned();
    }
    text = HTML_BREAKS.replace_all(&text, "\n").into_owned();
    text = TAGS.replace_all(&text, " ").into_owned();
    for (entity, character) in [("&nbsp;", " "), ("&amp;", "&"), ("&lt;", "<"), ("&gt;", ">"), ("&quot;", "\""),
                                ("&#39;", "'"), ("&apos;", "'"), ("&mdash;", "—"), ("&ndash;", "–"), ("&hellip;", "…")] {
        text = text.replace(entity, character);
    }
    if text.contains("&#") {
        text = NUMERIC_ENTITY.replace_all(&text, |captures: &regex::Captures| {
            let code = &captures[1];
            let value = match code.strip_prefix('x') {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => code.parse().ok(),
            };
            value.and_then(char::from_u32).map(String::from).unwrap_or_else(|| " ".into())
        }).into_owned();
    }
    let text = SPACES.replace_all(&text, " ");
    LINE_BREAKS.replace_all(&text, "\n").trim().to_string()
}

static CHUNK_SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new("[ \t\u{00A0}]+").unwrap());
static CHUNK_PARAGRAPHS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n\s*\n\s*\n+").unwrap());

/// Splits text into ~`size`-character pieces (~450 tokens) that overlap a little, breaking at whitespace. The pieces
/// are cut the same way every time, so a later step goes on where the last one stopped.
pub fn chunks(text: &str, size: usize, overlap: usize, limit: usize) -> Vec<String> {
    let clean = CHUNK_SPACES.replace_all(text, " ");
    let clean = CHUNK_PARAGRAPHS.replace_all(&clean, "\n\n");
    let clean: Vec<char> = clean.trim().chars().collect();
    let mut out = vec![];
    let mut start = 0;
    while start < clean.len() && out.len() < limit {
        let mut end = (start + size).min(clean.len());
        if end < clean.len() {
            if let Some(space) = (start..end).rev().find(|&i| clean[i].is_whitespace()) {
                if space - start > size / 2 {
                    end = space;
                }
            }
        }
        let chunk: String = clean[start..end].iter().collect();
        let chunk = chunk.trim();
        if !chunk.is_empty() {
            out.push(chunk.to_string());
        }
        if end >= clean.len() {
            break;
        }
        let mut next = if end >= start + overlap { end - overlap } else { end };
        if next <= start {
            next = end;
        }
        // Start the next chunk on a word boundary.
        while next < end && !clean[next].is_whitespace() {
            next += 1;
        }
        start = if next < end { next } else { end };
    }
    out
}

// MARK: Sound and video (ffmpeg decodes, ffprobe measures)

/// ffmpeg or ffprobe: beside this program, in $SPOTLITE_FFMPEG, then on PATH.
fn tool(name: &str) -> PathBuf {
    let mut folders = vec![];
    if let Ok(chosen) = std::env::var("SPOTLITE_FFMPEG") {
        folders.push(PathBuf::from(chosen));
    }
    if let Some(folder) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        folders.push(folder);
    }
    folders.into_iter().map(|f| f.join(name)).find(|p| p.is_file()).unwrap_or_else(|| PathBuf::from(name))
}

/// Runs ffmpeg and hands its output to `read` as it comes (a long decode never sits in memory whole).
fn ffmpeg_stream(arguments: &[&str], mut read: impl FnMut(&[u8]) -> Result<(), ExtractError>) -> Result<(), ExtractError> {
    let mut child = Command::new(tool("ffmpeg")).args(arguments).stdin(Stdio::null()).stdout(Stdio::piped())
        .stderr(Stdio::null()).spawn()
        .map_err(|_| ExtractError::Unreadable("ffmpeg isn't installed (audio and video need it)".into()))?;
    let mut stdout = child.stdout.take().unwrap();
    let mut buffer = vec![0u8; 1 << 16];
    let result = loop {
        match stdout.read(&mut buffer) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                if let Err(error) = read(&buffer[..n]) {
                    break Err(error);
                }
            }
            Err(error) => break Err(error.into()),
        }
    };
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    result?;
    if !status.success() {
        return Err(ExtractError::Unreadable(format!("ffmpeg failed ({})", status.code().unwrap_or(-1))));
    }
    Ok(())
}

fn ffmpeg_output(arguments: &[&str]) -> Result<Vec<u8>, ExtractError> {
    let mut data = vec![];
    ffmpeg_stream(arguments, |chunk| {
        data.extend_from_slice(chunk);
        Ok(())
    })?;
    Ok(data)
}

fn ffprobe(path: &str, entries: &str) -> Option<String> {
    let output = Command::new(tool("ffprobe"))
        .args(["-v", "error", "-show_entries", entries, "-of", "default=noprint_wrappers=1:nokey=1", path])
        .stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn duration(path: &str) -> Option<f64> {
    ffprobe(path, "format=duration")?.trim().parse::<f64>().ok().filter(|s| s.is_finite() && *s > 0.0)
}

/// Which kinds of streams the file has: (audio, video).
fn streams(path: &str) -> (bool, bool) {
    let text = ffprobe(path, "stream=codec_type").unwrap_or_default();
    (text.contains("audio"), text.contains("video"))
}

pub const SAMPLE_RATE: usize = 16_000;

/// A stretch of sound (16 kHz mono, i16, in a scratch file) or a video frame (a JPEG in a scratch file).
pub struct MediaPiece {
    pub path: PathBuf,
    pub start: f64,
    pub end: f64,
}

fn rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| (s as f64 / 32768.0).powi(2)).sum();
    (sum / samples.len() as f64).sqrt()
}

/// A sound window as the model takes it: f32 in [-1, 1].
pub fn read_window(path: &Path) -> Result<Vec<f32>, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    Ok(bytes.chunks_exact(2).map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32768.0).collect())
}

/// Sound (a file, or a video's soundtrack) as 16 kHz mono, in `window`-second pieces `hop` seconds apart, silent ones
/// skipped. One model input ignores anything past 30 s, so windows stay ≤ 30 s. Streams: memory stays at one window.
pub fn audio_windows(path: &str, window: f64, hop: f64, folder: &Path) -> Result<(Vec<MediaPiece>, f64), ExtractError> {
    if !streams(path).0 {
        return Ok((vec![], 0.0));
    }
    let duration = duration(path).unwrap_or(0.0);
    let (window_samples, hop_samples) = ((window * SAMPLE_RATE as f64) as usize, (hop * SAMPLE_RATE as f64) as usize);
    let mut buffer: Vec<i16> = vec![];
    let mut offset = 0usize;   // sample index of buffer[0] in the file
    let mut pieces = vec![];
    let mut odd: Option<u8> = None;   // a byte left over when a read ends mid-sample
    let rate = SAMPLE_RATE.to_string();
    let save = |samples: &[i16], offset: usize, pieces: &mut Vec<MediaPiece>| -> Result<(), ExtractError> {
        if rms(samples) <= 0.003 {
            return Ok(());   // ≈ −50 dBFS: silence isn't worth a vector
        }
        let out = folder.join(format!("{}.pcm", unique_name()));
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        std::fs::write(&out, bytes)?;
        pieces.push(MediaPiece { path: out, start: offset as f64 / SAMPLE_RATE as f64,
                                 end: (offset + samples.len()) as f64 / SAMPLE_RATE as f64 });
        Ok(())
    };
    ffmpeg_stream(&["-v", "error", "-nostdin", "-i", path, "-vn", "-ac", "1", "-ar", &rate, "-f", "s16le", "-"], |data| {
        let mut bytes: Vec<u8> = Vec::with_capacity(data.len() + 1);
        if let Some(first) = odd.take() {
            bytes.push(first);
        }
        bytes.extend_from_slice(data);
        if bytes.len() % 2 == 1 {
            odd = bytes.pop();
        }
        buffer.extend(bytes.chunks_exact(2).map(|pair| i16::from_le_bytes([pair[0], pair[1]])));
        while buffer.len() >= window_samples {
            save(&buffer[..window_samples], offset, &mut pieces)?;
            buffer.drain(..hop_samples);
            offset += hop_samples;
        }
        Ok(())
    })?;
    // The tail: new sound beyond the previous window's overlap, or the whole file if it was shorter than a window.
    let covered = if offset == 0 { 0 } else { window_samples - hop_samples };
    if buffer.len() >= covered + 2 * SAMPLE_RATE {
        save(&buffer, offset, &mut pieces)?;
    }
    let duration = if duration > 0.0 { duration } else { (offset + buffer.len()) as f64 / SAMPLE_RATE as f64 };
    Ok((pieces, duration))
}

/// Samples a frame every `interval` seconds through ffmpeg and keeps one per shot: a frame that looks like the last
/// kept one (8×8 average hash within 4 bits) only extends it. Returns the frames, the duration, and whether the file
/// has sound.
pub fn keyframes(path: &str, interval: f64, max_pixel: usize, folder: &Path)
    -> Result<(Vec<MediaPiece>, f64, bool), ExtractError> {
    let (has_audio, has_video) = streams(path);
    let duration = duration(path).unwrap_or(0.0);
    if duration <= 0.0 || !has_video {
        return Ok((vec![], duration, has_audio));
    }
    let step = interval.max(duration / 1200.0);
    let first = 0.5f64.min(duration / 2.0);
    let prefix = format!("frame-{}-", unique_name());
    let pattern = folder.join(format!("{prefix}%06d.jpg"));
    // eof_action=pass: with keyframes only, the last one is the last input frame, and fps dropped it at the end (the
    // last shot of a clip was never seen: the parrot in penguin|zebra|parrot).
    let filter = format!("fps={}:eof_action=pass,scale={max_pixel}:{max_pixel}:force_original_aspect_ratio=decrease",
                         1.0 / step);
    // Keyframes only: a frame every few seconds needs no more, and decoding every frame of a 4K film on the CPU took
    // longer than the watchdog allows (2026-10-14: 10 minutes of 2160p x265 in 8.7 s this way). Not when they're
    // sparse: a generated 8 s clip has one, at 0 s, before the first frame taken, and gave no frames at all.
    // Four decoding threads: one per core held a dozen 4K frames at once (1.3 GB for a 2160p film). The start is cut
    // on the output side: seeking the input skipped past the only keyframes of a short clip (none came out).
    let (from, pattern) = (first.to_string(), pattern.display().to_string());
    let mut arguments = vec!["-v", "error", "-nostdin", "-threads", "4"];
    if !sparse_keyframes(path, duration) {
        arguments.extend(["-skip_frame", "nokey"]);
    }
    arguments.extend(["-i", path, "-ss", &from, "-an", "-vf", &filter, "-q:v", "3", "-f", "image2", &pattern]);
    ffmpeg_stream(&arguments, |_| Ok(()))?;
    let mut names: Vec<String> = std::fs::read_dir(folder)?.flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with(&prefix)).collect();
    names.sort();
    let mut frames: Vec<MediaPiece> = vec![];
    let mut last_hash: Option<u64> = None;
    for (index, name) in names.iter().enumerate() {
        let frame = folder.join(name);
        if let Ok(image) = image::open(&frame) {
            let hash = average_hash(&image.into_rgb8());
            if last_hash.is_some_and(|last| (hash ^ last).count_ones() <= 4) {
                let _ = std::fs::remove_file(&frame);
                continue;
            }
            last_hash = Some(hash);
        }
        frames.push(MediaPiece { path: frame, start: first + index as f64 * step, end: duration });
    }
    // Each kept frame stands for the stretch until the next kept frame.
    for index in 1..frames.len() {
        frames[index - 1].end = frames[index].start;
    }
    Ok((frames, duration, has_audio))
}

/// The first minute's keyframes are fewer than two, or more than 20 s apart: decoding only keyframes would leave most
/// of the video out. Packet headers only (nothing is decoded), so it's quick even for a 4K film.
fn sparse_keyframes(path: &str, duration: f64) -> bool {
    let Ok(output) = Command::new(tool("ffprobe"))
        .args(["-v", "error", "-select_streams", "v:0", "-read_intervals", "%+60", "-show_entries", "packet=flags",
               "-of", "csv=p=0", path])
        .stdin(Stdio::null()).stderr(Stdio::null()).output() else { return false };
    let keyframes = String::from_utf8_lossy(&output.stdout).lines().filter(|flags| flags.contains('K')).count();
    keyframes < 2 || duration.min(60.0) / keyframes as f64 > 20.0
}

/// The frame shown at `seconds`, its long side at most `pixels`, as a JPEG for a preview.
pub fn video_frame(path: &str, seconds: f64, pixels: usize, out: &Path) -> Result<(), ExtractError> {
    let filter = format!("scale='min(iw,{pixels})':'min(ih,{pixels})':force_original_aspect_ratio=decrease");
    ffmpeg_stream(&["-v", "error", "-nostdin", "-ss", &seconds.max(0.0).to_string(), "-i", path, "-frames:v", "1",
                    "-vf", &filter, "-q:v", "3", "-y", &out.display().to_string()], |_| Ok(()))?;
    if !out.exists() {
        return Err(ExtractError::Unreadable(format!("no frame at {seconds} s")));
    }
    Ok(())
}

/// A name no other scratch file has.
pub fn unique_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
    format!("{}-{nanos:x}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_html() {
        let html = "<html><head><title>x</title></head><body><p>Hello &amp; welcome</p><script>no()</script>\
                    <div>caf&#233;</div></body></html>";
        assert_eq!(plain_text_from_html(html), "Hello & welcome\ncafé");
    }

    #[test]
    fn cuts_overlapping_chunks() {
        let text = "word ".repeat(1000);
        let pieces = chunks(&text, 1800, 200, 100);
        assert_eq!(pieces.len(), 4);
        assert!(pieces.iter().all(|p| p.len() <= 1800 && !p.starts_with(' ')));
        assert_eq!(chunks(&text, 1800, 200, 1).len(), 1);
    }
}
