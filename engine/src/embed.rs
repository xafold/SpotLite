//! EmbeddingGemma 2 on llama.cpp, in this process (the C shim in shim/llama_shim.c does the llama.cpp calls). Pictures
//! are sized here the way the reference (transformers) sizes them, so llama.cpp sees the same pixels; sound arrives as
//! 16 kHz mono samples and is cut at 30 s.
use crate::paths;
use std::ffi::{CStr, CString, c_char, c_float, c_int, c_uchar};
use std::path::{Path, PathBuf};
use std::sync::Once;

#[repr(C)]
struct DgModel {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn dg_init(backends: *const c_char, log_path: *const c_char, names: *mut c_char, names_len: c_int);
    fn dg_gpu_free_mb() -> c_int;
    fn dg_open(text: *const c_char, media: *const c_char, max_tokens: c_int, threads: c_int, use_gpu: c_int,
               error: *mut c_char, error_len: c_int) -> *mut DgModel;
    fn dg_close(model: *mut DgModel);
    fn dg_dim(model: *const DgModel) -> c_int;
    fn dg_audio_rate(model: *const DgModel) -> c_int;
    fn dg_embed_texts(model: *mut DgModel, texts: *const *const c_char, n: c_int, out: *mut c_float) -> c_int;
    fn dg_embed_image(model: *mut DgModel, width: c_int, height: c_int, rgb: *const c_uchar, out: *mut c_float)
        -> c_int;
    fn dg_embed_audio(model: *mut DgModel, samples: c_int, pcm: *const c_float, out: *mut c_float) -> c_int;
}

/// Which vectors this pipeline makes (the model files, llama.cpp, the resizing, the prompts). Indexes record it, so
/// vectors from another pipeline never meet theirs.
pub const VECTOR_VERSION: u32 = 1;
pub const DIMENSION: usize = 768;
pub const LLAMA_BUILD_TAG: &str = env!("LLAMA_BUILD_TAG");

/// What goes into index fingerprints: the same for every embedder of this build, known without loading one. Rust's
/// image decoding differs from DigUp's Swift app's by a pixel here and there, so its vectors are a source of their own.
pub fn vector_source() -> String {
    format!("google/embeddinggemma-2@{} llama.cpp-rust-vectors{VECTOR_VERSION} text-q8_0+mmproj-q8_0",
            &crate::download::REVISION[..7])
}

/// The model files: from $SPOTLITE_MODELS (only there, when set), else SpotLite's Models folder, then the Hugging Face cache.
pub struct ModelPaths {
    pub text: PathBuf,
    pub media: Option<PathBuf>,
}

pub fn model_folders() -> Vec<PathBuf> {
    match std::env::var("SPOTLITE_MODELS") {
        Ok(chosen) if !chosen.is_empty() => vec![PathBuf::from(chosen)],
        _ => vec![crate::download::installed(), crate::download::hugging_face_snapshot()],
    }
}

pub fn locate_models() -> Result<ModelPaths, String> {
    let (text, media) = (crate::download::TEXT.name, crate::download::MEDIA.name);
    for folder in model_folders() {
        if folder.join(text).exists() {
            let media_path = folder.join(media);
            return Ok(ModelPaths { text: folder.join(text), media: media_path.exists().then_some(media_path) });
        }
    }
    Err(format!("no {text} in {}; run: spotlite download-model",
                model_folders().iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join(", ")))
}

/// Both files are there, so indexing can run.
pub fn models_complete() -> bool {
    locate_models().map(|models| models.media.is_some()).unwrap_or(false)
}

/// A picture in memory, 8-bit RGB.
pub struct Rgb {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

/// One input's vector, or why it has none (an unreadable input must not sink its neighbours).
pub type Embedding = Result<Vec<f32>, String>;

pub struct Embedder {
    model: *mut DgModel,
    pub text_only: bool,
    audio_rate: usize,
}

// The model is used from one thread at a time (the indexer's, or the search service's under its lock).
unsafe impl Send for Embedder {}

static INIT: Once = Once::new();

/// The threads llama.cpp computes on: every logical processor up to 8 (llama.cpp's default is 4), or $SPOTLITE_THREADS.
fn threads() -> c_int {
    std::env::var("SPOTLITE_THREADS").ok().and_then(|t| t.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get().min(8)).unwrap_or(4) as c_int).max(1)
}

impl Embedder {
    /// `text_only`: just the text model (queries, ~0.25 GB). `log`: where llama.cpp's warnings go (the first
    /// embedder in a process decides).
    pub fn open(text_only: bool, log: Option<&Path>) -> Result<Embedder, String> {
        let models = locate_models()?;
        if !text_only && models.media.is_none() {
            return Err(format!("{} (images and audio) isn't downloaded yet", crate::download::MEDIA.name));
        }
        INIT.call_once(|| {
            // ggml's backends (CPU variants, Vulkan, CUDA) are shared libraries: from $SPOTLITE_BACKENDS, beside this
            // program, or the dev build's Vendor/llama/lib.
            let backends = std::env::var("SPOTLITE_BACKENDS").ok().filter(|b| !b.is_empty()).or_else(|| {
                std::env::current_exe().ok()
                    .and_then(|exe| exe.parent().map(Path::to_path_buf))
                    .filter(|folder| folder.join("libggml-base.so").exists() || has_backend(folder))
                    .or_else(paths::dev_llama_lib)
                    .map(|folder| folder.display().to_string())
            });
            let backends = backends.map(|b| CString::new(b).unwrap());
            let log = log.map(|l| CString::new(l.display().to_string()).unwrap());
            let mut names = vec![0 as c_char; 512];
            unsafe {
                dg_init(backends.as_ref().map_or(std::ptr::null(), |b| b.as_ptr()),
                        log.as_ref().map_or(std::ptr::null(), |l| l.as_ptr()), names.as_mut_ptr(), 512);
            }
            let names = unsafe { CStr::from_ptr(names.as_ptr()) }.to_string_lossy().into_owned();
            eprintln!("[llama.cpp] backends: {names}");
        });
        let started = std::time::Instant::now();
        let text = CString::new(models.text.display().to_string()).unwrap();
        let media = if text_only { None } else { models.media.map(|m| CString::new(m.display().to_string()).unwrap()) };
        let mut error = vec![0 as c_char; 512];
        let open = |use_gpu: bool, error: &mut Vec<c_char>| unsafe {
            dg_open(text.as_ptr(), media.as_ref().map_or(std::ptr::null(), |m| m.as_ptr()),
                    if text_only { 512 } else { 2048 }, threads(), use_gpu as c_int, error.as_mut_ptr(), 512)
        };
        // A GPU without room for the model (a 4 GB card shared with a browser, a game, another SpotLite): llama.cpp can
        // abort partway rather than fail, so the free memory is checked first, and the model runs on the CPU instead
        // (slower, still right). Measured: ~300 MB for the text model, ~1.3 GB with pictures and sound.
        let needed_mb = if text_only { 450 } else { 1400 };
        let free_mb = unsafe { dg_gpu_free_mb() };
        let cpu_asked = std::env::var_os("SPOTLITE_CPU").is_some_and(|value| value == "1");
        let mut use_gpu = free_mb >= needed_mb && !cpu_asked;
        if free_mb >= 0 && !use_gpu && !cpu_asked {
            eprintln!("[llama.cpp] the GPU has {free_mb} MB free, {needed_mb} MB needed: on the CPU");
        }
        let mut model = open(use_gpu, &mut error);
        if model.is_null() && use_gpu {
            eprintln!("[llama.cpp] the GPU couldn't take the model ({}): on the CPU",
                      unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy());
            use_gpu = false;
            model = open(false, &mut error);
        }
        if model.is_null() {
            return Err(unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy().into_owned());
        }
        assert_eq!(unsafe { dg_dim(model) } as usize, DIMENSION, "the model's vectors aren't 768-wide");
        let mut embedder = Embedder { model, text_only, audio_rate: unsafe { dg_audio_rate(model) } as usize };
        // The first calls build the GPU pipelines: pay that now.
        for _ in 0..2 {
            embedder.embed_texts(&[crate::model::query_prompt("warm up")])?;
        }
        eprintln!("[llama.cpp] ready in {:.2} s ({}{}, llama.cpp {LLAMA_BUILD_TAG}, vectors {VECTOR_VERSION})",
                  started.elapsed().as_secs_f64(), if text_only { "text-only" } else { "full" },
                  if use_gpu { "" } else { ", CPU" });
        Ok(embedder)
    }

    pub fn embed_texts(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        // A NUL inside a text would end it early in C: dropped.
        let owned: Vec<CString> = texts.iter().map(|t| CString::new(t.replace('\0', "")).unwrap()).collect();
        let pointers: Vec<*const c_char> = owned.iter().map(|t| t.as_ptr()).collect();
        let mut out = vec![0f32; texts.len() * DIMENSION];
        let status = unsafe { dg_embed_texts(self.model, pointers.as_ptr(), texts.len() as c_int, out.as_mut_ptr()) };
        if status != 0 {
            return Err(format!("llama_process failed ({status})"));
        }
        Ok(out.chunks(DIMENSION).map(<[f32]>::to_vec).collect())
    }

    /// A picture at the size `budget` (vision soft tokens) asks for.
    pub fn embed_image(&mut self, image: &Rgb, budget: usize) -> Embedding {
        if self.text_only {
            return Err("this embedder was started text-only".into());
        }
        let (height, width) = target_size(image.height, image.width, budget).ok_or("image too thin to embed")?;
        let resized;
        let pixels = if (height, width) == (image.height, image.width) {
            &image.pixels
        } else {
            resized = resize_bicubic(&image.pixels, image.width, image.height, width, height);
            &resized
        };
        let mut out = vec![0f32; DIMENSION];
        let status = unsafe {
            dg_embed_image(self.model, width as c_int, height as c_int, pixels.as_ptr(), out.as_mut_ptr())
        };
        if status != 0 { Err(format!("the encoder failed ({status})")) } else { Ok(out) }
    }

    /// Sound as mono samples at the model's rate (16 kHz); past 30 s is dropped, as in the reference.
    pub fn embed_audio(&mut self, samples: &[f32]) -> Embedding {
        if self.text_only {
            return Err("this embedder was started text-only".into());
        }
        let samples = &samples[..samples.len().min(30 * self.audio_rate)];
        let mut out = vec![0f32; DIMENSION];
        let status = unsafe { dg_embed_audio(self.model, samples.len() as c_int, samples.as_ptr(), out.as_mut_ptr()) };
        if status != 0 { Err(format!("the encoder failed ({status})")) } else { Ok(out) }
    }
}

impl Drop for Embedder {
    fn drop(&mut self) {
        unsafe { dg_close(self.model) };
    }
}

fn has_backend(folder: &Path) -> bool {
    std::fs::read_dir(folder).map(|entries| entries.flatten()
        .any(|e| e.file_name().to_string_lossy().starts_with("libggml-cpu"))).unwrap_or(false)
}

/// transformers' Gemma 4 image processor: scale to fill `budget` soft tokens (each 3 × 3 patches of 16 px) keeping
/// the aspect ratio, each side rounded down to a multiple of 48 px. (height, width), or None for a picture too thin.
pub fn target_size(height: usize, width: usize, budget: usize) -> Option<(usize, usize)> {
    let (patch, pooling) = (16usize, 3usize);
    let max_patches = budget * pooling * pooling;
    let factor = ((max_patches * patch * patch) as f64 / (height * width) as f64).sqrt();
    let side = pooling * patch;
    let mut target_height = (factor * height as f64 / side as f64).floor() as usize * side;
    let mut target_width = (factor * width as f64 / side as f64).floor() as usize * side;
    if target_height == 0 && target_width == 0 {
        return None;
    }
    let max_side = (max_patches / (pooling * pooling)) * side;
    if target_height == 0 {
        target_height = side;
        target_width = (width / height * side).min(max_side);
    } else if target_width == 0 {
        target_width = side;
        target_height = (height / width * side).min(max_side);
    }
    Some((target_height, target_width))
}

const PRECISION: i32 = 22;

fn filter(value: f64) -> f64 {
    let (x, a) = (value.abs(), -0.5);
    if x < 1.0 {
        ((a + 2.0) * x - (a + 3.0)) * x * x + 1.0
    } else if x < 2.0 {
        (((x - 5.0) * x + 8.0) * x - 4.0) * a
    } else {
        0.0
    }
}

/// For each output pixel: the first contributing input pixel and how many; the weights, `size` per output pixel.
fn coefficients(in_size: usize, out_size: usize) -> (Vec<(usize, usize)>, Vec<i32>, usize) {
    let scale = in_size as f64 / out_size as f64;
    let filter_scale = scale.max(1.0);
    let support = 2.0 * filter_scale;
    let size = support.ceil() as usize * 2 + 1;
    let mut bounds = Vec::with_capacity(out_size);
    let mut weights = vec![0i32; out_size * size];
    let mut row = vec![0f64; size];
    for out in 0..out_size {
        let center = (out as f64 + 0.5) * scale;
        let first = ((center - support + 0.5) as i64).max(0) as usize;
        let count = ((center + support + 0.5) as usize).min(in_size) - first;
        let mut total = 0.0;
        for x in 0..size {
            row[x] = if x < count { filter(((x + first) as f64 - center + 0.5) / filter_scale) } else { 0.0 };
            total += row[x];
        }
        for x in 0..size {
            let weight = if x < count && total != 0.0 { row[x] / total } else { row[x] };
            weights[out * size + x] =
                (weight * (1i64 << PRECISION) as f64 + if weight < 0.0 { -0.5 } else { 0.5 }) as i32;
        }
        bounds.push((first, count));
    }
    (bounds, weights, size)
}

fn clip(value: i32) -> u8 {
    (value >> PRECISION).clamp(0, 255) as u8
}

/// Pillow's bicubic resampling of packed RGB (a = −0.5, the kernel widened when shrinking, 22-bit fixed point): what
/// the reference gets from PIL's Image.resize.
pub fn resize_bicubic(src: &[u8], width: usize, height: usize, to_width: usize, to_height: usize) -> Vec<u8> {
    let horizontal = |src: &[u8], in_width: usize, rows: usize, out_width: usize| -> Vec<u8> {
        let (bounds, weights, size) = coefficients(in_width, out_width);
        let mut out = vec![0u8; out_width * rows * 3];
        for y in 0..rows {
            for (x, &(first, count)) in bounds.iter().enumerate() {
                let (mut r, mut g, mut b) = (1i32 << (PRECISION - 1), 1i32 << (PRECISION - 1), 1i32 << (PRECISION - 1));
                let base = (y * in_width + first) * 3;
                for k in 0..count {
                    let w = weights[x * size + k];
                    let p = base + k * 3;
                    r = r.wrapping_add((src[p] as i32).wrapping_mul(w));
                    g = g.wrapping_add((src[p + 1] as i32).wrapping_mul(w));
                    b = b.wrapping_add((src[p + 2] as i32).wrapping_mul(w));
                }
                let o = (y * out_width + x) * 3;
                (out[o], out[o + 1], out[o + 2]) = (clip(r), clip(g), clip(b));
            }
        }
        out
    };
    let vertical = |src: &[u8], in_height: usize, row_width: usize, out_height: usize| -> Vec<u8> {
        let (bounds, weights, size) = coefficients(in_height, out_height);
        let stride = row_width * 3;
        let mut out = vec![0u8; stride * out_height];
        let mut sums = vec![0i32; stride];
        for (y, &(first, count)) in bounds.iter().enumerate() {
            sums.fill(1 << (PRECISION - 1));
            for k in 0..count {
                let row = &src[(first + k) * stride..(first + k + 1) * stride];
                let w = weights[y * size + k];
                for (sum, &value) in sums.iter_mut().zip(row) {
                    *sum = sum.wrapping_add((value as i32).wrapping_mul(w));
                }
            }
            for (o, &sum) in out[y * stride..(y + 1) * stride].iter_mut().zip(&sums) {
                *o = clip(sum);
            }
        }
        out
    };
    match (to_width != width, to_height != height) {
        (true, true) => vertical(&horizontal(src, width, height, to_width), height, to_width, to_height),
        (true, false) => horizontal(src, width, height, to_width),
        (false, true) => vertical(src, height, width, to_height),
        (false, false) => src.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_like_the_reference() {
        // 280 tokens of a 4:3 photo: at most 2520 patches of 16 px, each side a multiple of 48.
        assert_eq!(target_size(3000, 4000, 280), Some((672, 912)));
        assert_eq!(target_size(10, 100_000, 70), Some((48, 3360)));   // one row of tokens, as wide as allowed
    }

    #[test]
    fn resizing_keeps_a_flat_color() {
        let src = vec![200u8; 40 * 30 * 3];
        let out = resize_bicubic(&src, 40, 30, 17, 11);
        assert_eq!(out.len(), 17 * 11 * 3);
        assert!(out.iter().all(|&v| v == 200));
    }
}
