//! `spotlite search-service`: the searcher as a process for the shell. It keeps the index open and the query encoder warm,
//! so a search while typing costs milliseconds, not a process start.
//!
//! Protocol: one JSON request per line on stdin, one JSON response per line on stdout, carrying the request's `id`.
//!   {"op":"search","q":…,"code":bool,"kinds":[…],"limit":n,"meaning":bool,"prefix_last":bool}
//!       → {"hits":[…], "strong":n, "meaning":bool, "ms":{"embed":…,"search":…}}
//!   {"op":"embed_ahead","q":…,"code":bool}   → {"ok":true}: embeds and caches what's typed so far
//!   {"op":"text","segment":id,"code":bool}    → {"text":…}: the part of a file a hit points at, for the preview
//!   {"op":"render","path":…,"kind":"pdf|video|image","loc":n,"pixels":n} → {"image":path}: a JPEG for a preview or a
//!                                               thumbnail, made beside the requests (typing never waits on one)
//!   {"op":"reload"}                           → {"ok":true,"vectors":n,"code_vectors":n}
//!   {"op":"folders","roots":[…],"skipped":[…],"types":[…],"code_roots":[…],"code_skipped":[…]} → {"ok":true}
//!   {"op":"prewarm"}                          → {"ok":true}: starts the encoder and loads the vectors
//!   {"op":"status"}                           → {"encoder":"stopped|starting|ready|unavailable","vectors":n,…}
//!   {"op":"quit"}                             → {"ok":true}, then exit
//! The encoder exits after `--encoder-idle` seconds without a query and the vectors are released; keyword search keeps
//! working and the next query brings both back.
use crate::embed::Embedder;
use crate::emit;
use crate::error::Result;
use crate::extract;
use crate::model::{self, FileKind, IndexOptions};
use crate::paths;
use crate::rules;
use crate::search::{self, Searcher};
use crate::store::{self, Store};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

struct State {
    directory: PathBuf,
    searcher: Searcher,
    code_searcher: Option<Searcher>,
    roots: Vec<String>,
    skipped: Vec<String>,
    types: BTreeSet<String>,
    code_roots: Vec<String>,
    code_skipped: Vec<String>,
    encoder: Option<Embedder>,
    encoder_state: &'static str,
    vector_cache: HashMap<String, Vec<f32>>,
    last_query: Instant,
    warned_about_source: bool,
}

pub struct Options {
    pub index: PathBuf,
    pub roots: Vec<String>,
    pub skipped: Vec<String>,
    pub types: BTreeSet<String>,
    pub code_roots: Vec<String>,
    pub code_skipped: Vec<String>,
    pub idle_seconds: f64,
}

pub fn serve(options: Options) -> Result<i32> {
    let store = Store::open(&options.index, store::MAIN_FILE)?;
    let previews = options.index.join("previews");
    let _ = std::fs::remove_dir_all(&previews);
    std::fs::create_dir_all(&previews)?;
    let mut state = State {
        directory: options.index.clone(), searcher: Searcher::new(store, false)?, code_searcher: None,
        roots: options.roots, skipped: options.skipped, types: options.types, code_roots: options.code_roots,
        code_skipped: options.code_skipped, encoder: None, encoder_state: "stopped", vector_cache: HashMap::new(),
        last_query: Instant::now(), warned_about_source: false,
    };
    state.open_code()?;
    emit(json!({"event": "ready", "index": options.index.display().to_string()}));
    let state = Arc::new(Mutex::new(state));
    let idle = Duration::from_secs_f64(options.idle_seconds);
    {
        let state = state.clone();
        std::thread::spawn(move || idle_watch(state, idle));
    }
    // Renders for previews: two at a time, beside the request loop, each made once per file version.
    let render_slots = Arc::new((Mutex::new(2usize), Condvar::new()));
    let rendered: Arc<Mutex<HashMap<String, String>>> = Arc::default();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<Value>(&line) else { continue };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let op = request["op"].as_str().unwrap_or_default().to_string();
        if op == "quit" {
            emit(json!({"id": id, "ok": true}));
            state.lock().unwrap().stop_encoder();
            return Ok(0);
        }
        if op == "render" {
            {
                let (count, freed) = &*render_slots;
                let mut free = count.lock().unwrap();
                while *free == 0 {
                    free = freed.wait(free).unwrap();
                }
                *free -= 1;
            }
            let (slots, rendered, previews) = (render_slots.clone(), rendered.clone(), previews.clone());
            std::thread::spawn(move || {
                let mut reply = render(&request, &previews, &rendered)
                    .unwrap_or_else(|error| json!({"ok": false, "error": truncate(&error, 500)}));
                reply["id"] = id;
                emit(reply);
                let (count, freed) = &*slots;
                *count.lock().unwrap() += 1;
                freed.notify_one();
            });
            continue;
        }
        let mut reply = state.lock().unwrap().handle(&op, &request)
            .unwrap_or_else(|error| json!({"ok": false, "error": truncate(&error.to_string(), 500)}));
        reply["id"] = id;
        emit(reply);
    }
    state.lock().unwrap().stop_encoder();
    Ok(0)
}

fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn strings(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
}

impl State {
    fn open_code(&mut self) -> Result<()> {
        if self.code_roots.is_empty() && !self.directory.join(store::CODE_FILE).exists() {
            return Ok(());
        }
        self.code_searcher = Some(Searcher::new(Store::open(&self.directory, store::CODE_FILE)?, false)?);
        Ok(())
    }

    fn handle(&mut self, op: &str, request: &Value) -> Result<Value> {
        match op {
            "search" => {
                let text = request["q"].as_str().unwrap_or_default();
                let code = request["code"].as_bool().unwrap_or(false);
                let kinds: Option<BTreeSet<FileKind>> = strings(&request["kinds"])
                    .map(|kinds| kinds.iter().filter_map(|k| FileKind::parse(k)).collect());
                let limit = request["limit"].as_u64().unwrap_or(40) as usize;
                let want_meaning = request["meaning"].as_bool().unwrap_or(true);
                let prefix_last = request["prefix_last"].as_bool().unwrap_or(false);
                let started = Instant::now();
                let mut vector = None;
                let mut warning = None;
                if want_meaning && !text.is_empty() {
                    vector = self.vector(text, code);
                    // Vectors from another model or runtime must never meet: keyword-only then, and say so once.
                    let searcher = if code { self.code_searcher.as_ref() } else { Some(&self.searcher) };
                    if let Some(indexed) = searcher.and_then(|s| s.vector_source.clone()) {
                        if vector.is_some() && indexed != crate::embed::vector_source() {
                            vector = None;
                            if !self.warned_about_source {
                                warning = Some(format!("meaning search is off: the index was built with {indexed}, \
                                                        the encoder is {}", crate::embed::vector_source()));
                                self.warned_about_source = true;
                            }
                        }
                    }
                }
                let embedded = Instant::now();
                let hits = if code {
                    let Some(searcher) = self.code_searcher.as_mut() else {
                        return Ok(json!({"hits": [], "strong": 0, "meaning": false}));
                    };
                    let (roots, skipped) = (&self.code_roots, &self.code_skipped);
                    searcher.search(text, vector.as_deref(), None, limit, 2.0, None, prefix_last)?.into_iter()
                        .filter(|h| rules::is_searched(&h.path, roots, skipped) && Path::new(&h.path).exists()).collect::<Vec<_>>()
                } else {
                    let (roots, skipped, types) = (&self.roots, &self.skipped, &self.types);
                    self.searcher.search(text, vector.as_deref(), kinds.as_ref(), limit, 2.0, None, prefix_last)?
                        .into_iter().filter(|h| {
                            (roots.is_empty() || rules::is_searched(&h.path, roots, skipped))
                                && !types.contains(&paths::extension(&h.path)) && Path::new(&h.path).exists()
                        }).collect::<Vec<_>>()
                };
                let mut reply = json!({
                    "hits": hits.iter().map(search::Hit::json).collect::<Vec<_>>(),
                    "strong": search::strong_count(&hits, 0.5), "meaning": vector.is_some(),
                    "ms": {"embed": (embedded - started).as_millis() as u64, "search": embedded.elapsed().as_millis() as u64},
                });
                if let Some(warning) = warning {
                    reply["warning"] = warning.into();
                }
                Ok(reply)
            }
            "embed_ahead" => {
                self.vector(request["q"].as_str().unwrap_or_default(), request["code"].as_bool().unwrap_or(false));
                Ok(json!({"ok": true}))
            }
            "text" => {
                let segment = request["segment"].as_i64().ok_or("which segment?")?;
                let searcher = if request["code"].as_bool().unwrap_or(false) { self.code_searcher.as_ref() } else {
                    Some(&self.searcher)
                };
                let text = match searcher {
                    Some(searcher) => searcher.text_of_segment(segment)?,
                    None => None,
                };
                Ok(json!({"text": text}))
            }
            "reload" => {
                let loaded = self.searcher.vectors_loaded;
                if loaded { self.searcher.reload()? } else { self.searcher = Searcher::new(Store::open(&self.directory, store::MAIN_FILE)?, false)? }
                match self.code_searcher.as_mut() {
                    Some(code) if code.vectors_loaded => code.reload()?,
                    _ => self.open_code()?,
                }
                Ok(json!({"ok": true, "vectors": self.searcher.vector_count(),
                          "code_vectors": self.code_searcher.as_ref().map_or(0, |c| c.vector_count())}))
            }
            "folders" => {
                if let Some(value) = strings(&request["roots"]) {
                    self.roots = value.iter().map(|p| paths::folder_path(p)).collect();
                }
                if let Some(value) = strings(&request["skipped"]) {
                    self.skipped = value.iter().map(|p| paths::folder_path(p)).collect();
                }
                if let Some(value) = strings(&request["types"]) {
                    self.types = IndexOptions::extensions(&value.join(","));
                }
                if let Some(value) = strings(&request["code_roots"]) {
                    self.code_roots = value.iter().map(|p| paths::folder_path(p)).collect();
                }
                if let Some(value) = strings(&request["code_skipped"]) {
                    self.code_skipped = value.iter().map(|p| paths::folder_path(p)).collect();
                }
                if self.code_searcher.is_none() {
                    self.open_code()?;
                }
                Ok(json!({"ok": true}))
            }
            "prewarm" => {
                self.last_query = Instant::now();
                self.running_encoder();
                if !self.searcher.vectors_loaded {
                    self.searcher.reload()?;
                }
                Ok(json!({"ok": true, "encoder": self.encoder_state, "vectors": self.searcher.vector_count()}))
            }
            "status" => Ok(json!({
                "encoder": self.encoder_state,
                "vectors": if self.searcher.vectors_loaded { self.searcher.vector_count() } else { 0 },
                "code_vectors": self.code_searcher.as_ref().filter(|c| c.vectors_loaded).map_or(0, |c| c.vector_count()),
                "index": self.searcher.store.file.display().to_string(),
            })),
            other => Err(format!("unknown op {other}").into()),
        }
    }

    /// The query's vector (cached), or None when the encoder can't run: keyword results still work.
    fn vector(&mut self, text: &str, code: bool) -> Option<Vec<f32>> {
        self.last_query = Instant::now();
        let key = format!("{}{text}", if code { "code\0" } else { "" });
        if let Some(cached) = self.vector_cache.get(&key) {
            return Some(cached.clone());
        }
        let prompt = if code { model::code_query_prompt(text) } else { model::query_prompt(text) };
        let encoder = self.running_encoder()?;
        match encoder.embed_texts(&[prompt]) {
            Ok(mut vectors) => {
                let vector = vectors.pop()?;
                if self.vector_cache.len() > 256 {
                    self.vector_cache.clear();
                }
                self.vector_cache.insert(key, vector.clone());
                Some(vector)
            }
            Err(error) => {
                eprintln!("encoder: {error}");
                self.stop_encoder();   // the next query starts a new one
                None
            }
        }
    }

    fn running_encoder(&mut self) -> Option<&mut Embedder> {
        if self.encoder.is_none() {
            if self.encoder_state == "unavailable" {
                return None;
            }
            self.encoder_state = "starting";
            let started = Instant::now();
            match Embedder::open(true, Some(&self.directory.join("worker-query.log"))) {
                Ok(encoder) => {
                    self.encoder = Some(encoder);
                    self.encoder_state = "ready";
                    emit(json!({"event": "encoder", "state": "ready",
                                "seconds": (started.elapsed().as_secs_f64() * 100.0).round() / 100.0,
                                "source": crate::embed::vector_source()}));
                }
                Err(error) => {
                    self.encoder_state = "unavailable";
                    emit(json!({"event": "encoder", "state": "unavailable", "error": truncate(&error, 300)}));
                    return None;
                }
            }
        }
        self.encoder.as_mut()
    }

    fn stop_encoder(&mut self) {
        self.encoder = None;
        self.vector_cache.clear();
        if self.encoder_state != "unavailable" {
            self.encoder_state = "stopped";
        }
    }
}

/// After `idle` without a query the whole service exits: the model and the vectors go, and so does what the GPU driver
/// keeps for the process once it has used the GPU (~290 MB with NVIDIA's Vulkan driver, which freeing the model leaves
/// behind). The shell starts it again on its next request, in about 50 ms; keyword search answers at once and the
/// encoder warms up beside it.
fn idle_watch(state: Arc<Mutex<State>>, idle: Duration) {
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let mut state = state.lock().unwrap();
        if state.last_query.elapsed() > idle {
            state.stop_encoder();
            emit(json!({"event": "encoder", "state": "stopped", "reason": "idle"}));
            std::process::exit(0);
        }
    }
}

/// A JPEG for a preview or a thumbnail: a PDF's page, a video's frame at a moment, or a picture a web view can't show.
fn render(request: &Value, previews: &Path, rendered: &Mutex<HashMap<String, String>>) -> std::result::Result<Value, String> {
    let path = request["path"].as_str().ok_or("which file?")?;
    let kind = request["kind"].as_str().unwrap_or("image");
    let loc = request["loc"].as_f64();
    let pixels = request["pixels"].as_u64().unwrap_or(1024).clamp(32, 2048) as usize;
    let modified = crate::crawler::file_info(path).map_or(0.0, |info| info.2);
    let key = format!("{path}|{modified}|{kind}|{}|{pixels}", loc.unwrap_or(-1.0));
    if let Some(done) = rendered.lock().unwrap().get(&key).filter(|done| Path::new(done).exists()) {
        return Ok(json!({"image": done}));
    }
    let out = previews.join(format!("{}.jpg", extract::unique_name()));
    match kind {
        "pdf" => {
            let page = crate::pdf::render_page(path, loc.unwrap_or(1.0) as usize, pixels).map_err(|e| e.to_string())?;
            save_jpeg(page.width, page.height, page.pixels, &out)?;
        }
        "video" => extract::video_frame(path, loc.unwrap_or(0.0), pixels, &out).map_err(|e| e.to_string())?,
        _ => {
            let (image, _) = extract::decode(path, 0).map_err(|e| e.to_string())?;
            let image = extract::fit(image, pixels);
            save_jpeg(image.width() as usize, image.height() as usize, image.into_raw(), &out)?;
        }
    }
    let image = out.display().to_string();
    rendered.lock().unwrap().insert(key, image.clone());
    Ok(json!({"image": image}))
}

fn save_jpeg(width: usize, height: usize, pixels: Vec<u8>, out: &Path) -> std::result::Result<(), String> {
    let image = image::RgbImage::from_raw(width as u32, height as u32, pixels).ok_or("bad picture")?;
    let file = std::fs::File::create(out).map_err(|e| e.to_string())?;
    image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::BufWriter::new(file), 85)
        .encode_image(&image).map_err(|e| e.to_string())
}
