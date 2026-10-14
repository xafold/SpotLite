//! spotlite: search your files by meaning. SpotLite's engine for Linux, in Rust: the helper processes the shell runs
//! (`search-service`, `index-service`, `download-model`, `estimate --json`) and a developer CLI.
mod code;
mod crawler;
mod download;
mod embed;
mod error;
mod estimate;
mod extract;
mod index_service;
mod indexer;
mod model;
mod ocr;
mod paths;
mod pdf;
mod rtf;
mod rules;
mod search;
mod search_service;
mod store;
mod text;

use error::{Error, Result};
use model::{FileKind, IndexOptions};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const USAGE: &str = "spotlite: search your files by meaning (SpotLite's engine)

  spotlite estimate <folder>... [--json]     what would be indexed, and how long the first pass takes
  spotlite index <folder>... [--limit N]     sync the index with these folders, embed what's new, then the rest
                                          of long PDFs and documents
  spotlite search <words>... [--kind K,K] [-n 10] [--text] [--json]
                                          \"code: <words>\" searches the code index; --text adds each
                                          hit's matched text (the PDF page, the screenshot's words, the lines)
  spotlite status                            what's in the index
  spotlite index-service <folder>... [--code-root DIR]... [--code-exclude DIR]... [--backfill] [--prune]
                      [--memory-limit MB] [--time-limit S]
                                          the shell's indexing helper (JSON lines)
  spotlite search-service <folder>... [--code-root DIR]... [--encoder-idle S]
                                          the searcher as a process, index open and encoder warm (JSON lines)
  spotlite download-model [--to DIR]         the model files, downloaded with resume and checked (JSON lines)

Options:
  --index DIR          index folder (default: ~/.local/share/spotlite/index)
  --code               estimate, index or show the code index of these folders (code.sqlite)
  --kind K,K           screenshot, image, pdf, doc, audio, video
  --include-code       also index loose source code and data files as documents (outside code projects)
  --include-datasets   don't skip folders that look like datasets
  --exclude DIR        don't index this folder (repeatable)
  --exclude-ext EXT    don't index files with this extension (repeatable, or comma-separated: \"heic,txt\")
  --reindex            drop vectors and embed everything again

The model: EmbeddingGemma 2 on llama.cpp (Vulkan or CUDA when built so, else the CPU; the CPU also when the GPU
has no room for it, or with SPOTLITE_CPU=1), from $SPOTLITE_MODELS (only there, when set), else
~/.local/share/spotlite/Models or the Hugging Face cache.";

/// One JSON object per line on stdout, written at once (a closed pipe is ignored: the shell may be gone).
pub fn emit(value: Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

#[derive(Default)]
struct Arguments {
    positional: Vec<String>,
    values: HashMap<String, String>,
    lists: HashMap<String, Vec<String>>,
    flags: HashSet<String>,
}

const VALUED: [&str; 11] = ["--index", "--limit", "--kind", "-n", "--new-limit", "--linger", "--encoder-idle", "--to",
                            "--memory-limit", "--time-limit", "--zgate"];
const REPEATED: [&str; 4] = ["--exclude", "--exclude-ext", "--code-root", "--code-exclude"];

impl Arguments {
    fn parse(raw: &[String]) -> Result<Arguments> {
        let mut args = Arguments::default();
        let mut index = 0;
        while index < raw.len() {
            let argument = &raw[index];
            if VALUED.contains(&argument.as_str()) || REPEATED.contains(&argument.as_str()) {
                let value = raw.get(index + 1).ok_or_else(|| format!("{argument} needs a value"))?.clone();
                if REPEATED.contains(&argument.as_str()) {
                    args.lists.entry(argument.clone()).or_default().push(value);
                } else {
                    args.values.insert(argument.clone(), value);
                }
                index += 2;
            } else if argument.starts_with("--") || argument == "-h" {
                args.flags.insert(argument.clone());
                index += 1;
            } else {
                args.positional.push(argument.clone());
                index += 1;
            }
        }
        Ok(args)
    }

    fn index_folder(&self) -> PathBuf {
        self.values.get("--index").map_or_else(paths::default_index, |i| PathBuf::from(paths::folder_path(i)))
    }

    fn code(&self) -> bool {
        self.flags.contains("--code")
    }

    fn list(&self, name: &str) -> Vec<String> {
        self.lists.get(name).cloned().unwrap_or_default()
    }

    fn number<T: std::str::FromStr>(&self, name: &str) -> Option<T> {
        self.values.get(name).and_then(|v| v.parse().ok())
    }

    fn options(&self) -> IndexOptions {
        IndexOptions {
            code: self.code(),
            include_code: self.flags.contains("--include-code"),
            include_datasets: self.flags.contains("--include-datasets"),
            excluded_folders: self.list("--exclude").iter().map(|f| paths::folder_path(f)).collect(),
            excluded_extensions: IndexOptions::extensions(&self.list("--exclude-ext").join(",")),
            ..IndexOptions::default()
        }
    }

    fn roots(&self) -> Vec<String> {
        self.positional.iter().map(|f| paths::folder_path(f)).collect()
    }

    /// The folders given, each there and readable.
    fn folders(&self) -> Result<Vec<String>> {
        if self.positional.is_empty() {
            return Err("which folders? e.g. spotlite index ~/Pictures".into());
        }
        self.roots().into_iter().map(|folder| {
            if !std::path::Path::new(&folder).is_dir() {
                return Err(format!("not a folder: {folder}").into());
            }
            std::fs::read_dir(&folder).map_err(|_| Error::from(format!("can't read {folder}")))?;
            Ok(folder)
        }).collect()
    }

    fn kinds(&self) -> Result<Option<BTreeSet<FileKind>>> {
        let Some(list) = self.values.get("--kind") else { return Ok(None) };
        list.split(',').map(|k| FileKind::parse(k.trim())
            .ok_or_else(|| Error::from(format!("unknown kind {k}; use screenshot, image, pdf, doc, audio, video"))))
            .collect::<Result<BTreeSet<_>>>().map(Some)
    }

    fn store(&self) -> Result<store::Store> {
        store::Store::open(&self.index_folder(), if self.code() { store::CODE_FILE } else { store::MAIN_FILE })
    }
}

// MARK: Formatting

fn duration(seconds: f64) -> String {
    if seconds < 90.0 {
        format!("{} s", seconds.round() as i64)
    } else if seconds < 5400.0 {
        format!("{} min", (seconds / 60.0).round() as i64)
    } else {
        format!("{:.1} h", seconds / 3600.0)
    }
}

fn clock(seconds: f64) -> String {
    let total = seconds as i64;
    if total >= 3600 { format!("{}:{:02}:{:02}", total / 3600, total / 60 % 60, total % 60) } else {
        format!("{}:{:02}", total / 60, total % 60)
    }
}

fn location(hit: &search::Hit) -> String {
    use model::SegmentKind::*;
    match (hit.segment, hit.loc) {
        (Page | PageImage, Some(loc)) => format!("p. {}", loc as i64),
        (Audio, Some(loc)) => format!("at {} (sound)", clock(loc)),
        (Frame, Some(loc)) => format!("at {}", clock(loc)),
        (Chunk, Some(loc)) => format!("part {}", loc as i64),
        (Lines, Some(loc)) => format!("lines {}–{}{}", loc as i64, hit.loc_end.unwrap_or(loc) as i64,
                                      hit.focus.map(|f| format!(", opens at {}", f as i64)).unwrap_or_default()),
        (Cells, Some(loc)) => if Some(loc) == hit.loc_end { format!("cell {}", loc as i64) } else {
            format!("cells {}–{}", loc as i64, hit.loc_end.unwrap_or(loc) as i64)
        },
        (Ocr, _) => "text in image".into(),
        _ => String::new(),
    }
}

fn reason_line(reasons: &std::collections::BTreeMap<String, usize>) -> String {
    let mut sorted: Vec<_> = reasons.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1));
    sorted.iter().map(|(reason, count)| format!("{count} {reason}")).collect::<Vec<_>>().join(" · ")
}

fn states_line(states: &std::collections::BTreeMap<String, usize>) -> String {
    states.iter().map(|(state, count)| format!("{count} {state}")).collect::<Vec<_>>().join(" · ")
}

// MARK: Commands

fn estimate(args: &Arguments) -> Result<()> {
    if args.positional.is_empty() {
        return Err("which folders? e.g. spotlite estimate ~/Documents".into());
    }
    let options = args.options();
    let costs = estimate::MeasuredCosts::load(&args.index_folder()).costs();
    let json_out = args.flags.contains("--json");
    let mut total = 0.0;
    for folder in args.roots() {
        // One folder that isn't there or can't be read mustn't sink the others.
        if !std::path::Path::new(&folder).is_dir() || std::fs::read_dir(&folder).is_err() {
            if json_out {
                emit(json!({"folder": folder, "readable": false, "seconds": 0, "files": {}, "pdf_pages": 0,
                            "audio_seconds": 0, "video_seconds": 0}));
            } else {
                println!("{}    can't read it (not there, or not allowed)", paths::tilde(&folder));
            }
            continue;
        }
        let started = Instant::now();
        let crawl = crawler::crawl(std::slice::from_ref(&folder), &options);
        let parts = estimate::by_subfolder(&crawl.candidates, &folder, &options, &costs);
        let mut whole = estimate::Estimate::default();
        for part in parts.values() {
            whole += part;
        }
        // Biggest first; setup lists them under the folder, so that a long first pass can be cut down.
        let mut subfolders: Vec<(&String, &estimate::Estimate)> = parts.iter().filter(|(key, _)| **key != folder).collect();
        subfolders.sort_by(|a, b| b.1.seconds.total_cmp(&a.1.seconds).then(a.0.cmp(b.0)));
        total += whole.seconds;
        let files = |e: &estimate::Estimate| -> Value {
            e.files.iter().map(|(kind, count)| (kind.name().to_string(), json!(count))).collect::<serde_json::Map<_, _>>().into()
        };
        if json_out {
            emit(json!({
                "folder": folder, "seconds": whole.seconds, "readable": true, "files": files(&whole),
                "pdf_pages": whole.pdf_pages, "pdf_pages_total": whole.pdf_pages_total,
                "audio_seconds": whole.audio_seconds, "video_seconds": whole.video_seconds,
                "long_files": whole.long_files, "later_seconds": whole.later_seconds,
                "code_stretches": whole.code_stretches, "bytes": whole.bytes,
                "subfolders": subfolders.iter().take(500).map(|(key, e)| json!({
                    "folder": key, "seconds": e.seconds, "files": files(e), "long_files": e.long_files,
                    "later_seconds": e.later_seconds, "bytes": e.bytes,
                })).collect::<Vec<_>>(),
                "skipped": crawl.skipped.values().sum::<usize>(), "datasets": crawl.datasets.len(),
                "ms": started.elapsed().as_millis() as u64,
            }));
            continue;
        }
        let kinds: Vec<String> = whole.files.iter().map(|(kind, count)| match kind {
            FileKind::Pdf => format!("{count} PDFs ({} pages; {} in the first pass)", whole.pdf_pages_total, whole.pdf_pages),
            FileKind::Audio => format!("{count} audio ({})", duration(whole.audio_seconds)),
            FileKind::Video => format!("{count} video ({})", duration(whole.video_seconds)),
            FileKind::Code => format!("{count} code files ({} stretches)", whole.code_stretches),
            other => format!("{count} {}{}", other.name(), if *count == 1 { "" } else { "s" }),
        }).collect();
        println!("{}    first index ≈ {}", paths::tilde(&folder), duration(whole.seconds));
        println!("  {}", if kinds.is_empty() { "nothing to index".to_string() } else { kinds.join(" · ") });
        if whole.long_files > 0 {
            println!("  then the rest of {} long files ≈ {}", whole.long_files, duration(whole.later_seconds));
        }
        if !subfolders.is_empty() {
            let names: Vec<String> = subfolders.iter().take(5)
                .map(|(key, e)| format!("{} ≈ {}", paths::file_name(key), duration(e.seconds))).collect();
            println!("  biggest subfolders: {}{}", names.join(" · "),
                     if subfolders.len() > 5 { format!(" · {} more", subfolders.len() - 5) } else { String::new() });
        }
        if !crawl.skipped.is_empty() {
            println!("  skipped: {}", reason_line(&crawl.skipped));
        }
        if !crawl.skipped_folders.is_empty() {
            println!("  folders not walked: {}", reason_line(&crawl.skipped_folders));
        }
        for (dataset, count) in &crawl.datasets {
            println!("  looks like a dataset (skipped): {} ({count} files)", paths::tilde(dataset));
        }
        println!("  (looked at it in {:.2} s)", started.elapsed().as_secs_f64());
    }
    if args.positional.len() > 1 && !json_out {
        println!("all together ≈ {}", duration(total));
    }
    Ok(())
}

fn index(args: &Arguments) -> Result<()> {
    let store = args.store()?;
    if args.flags.contains("--reindex") {
        store.reset_all()?;
    }
    let index_folder = args.index_folder();
    if !args.code() {
        store.read_again_once()?;   // screenshots' and scans' words, once OCR is there
    }
    let mut indexer = indexer::Indexer::new(store, args.options())?;
    indexer.costs_folder = Some(index_folder.clone());
    indexer.check_model()?;
    let mut model = indexer::Model::new(Some(index_folder.join("worker.log")));
    let folders = args.folders()?;
    let sync = indexer.sync(&folders)?;
    println!("index: {}", paths::tilde(&indexer.store.file.display().to_string()));
    println!("sync: {} new · {} changed · {} moved · {} removed · {} unchanged", sync.added, sync.changed, sync.moved,
             sync.removed, sync.unchanged);
    if !sync.crawl.skipped.is_empty() {
        println!("skipped: {}", reason_line(&sync.crawl.skipped));
    }
    let limit = args.number::<usize>("--limit");
    let started = Instant::now();
    let states = indexer.run(&mut model, limit, None, &mut || false, &mut |event| {
        println!("[{:>4}/{}] {:<10} {:<7} {:5.2}s  {}{}", event.done, event.total, event.kind.name(), event.state,
                 event.seconds, paths::file_name(&event.path), event.note.map(|n| format!("  ({n})")).unwrap_or_default());
    })?;
    if states.is_empty() {
        println!("nothing pending");
    } else {
        println!("done in {}: {}", duration(started.elapsed().as_secs_f64()), states_line(&states));
    }
    // The rest of long files, as the shell's helper does once nothing is pending.
    if limit.is_some() || indexer.store.unfinished_count()?.0 == 0 {
        return Ok(());
    }
    let started = Instant::now();
    let finished = indexer.finish_long_files(&mut model, &mut || false, &mut |step| {
        println!("[rest {:>5}/{}] {:<4} {:<7} {:5.2}s  {}{}", step.done, step.total, step.kind.name(), step.state,
                 step.seconds, paths::file_name(&step.path), step.note.map(|n| format!("  ({n})")).unwrap_or_default());
    })?;
    println!("the rest of long files in {}: {}", duration(started.elapsed().as_secs_f64()),
             if finished.is_empty() { "none finished".into() } else { states_line(&finished) });
    Ok(())
}

fn search_command(args: &Arguments) -> Result<()> {
    let mut query = args.positional.join(" ");
    if query.is_empty() {
        return Err("search for what?".into());
    }
    let code = model::code_query(&query);
    if let Some(code) = &code {
        query = code.clone();
    }
    let store = store::Store::open(&args.index_folder(), if code.is_some() { store::CODE_FILE } else { store::MAIN_FILE })?;
    let mut searcher = search::Searcher::new(store, true)?;
    let started = Instant::now();
    let vector = if searcher.vector_count() > 0 {
        let mut encoder = embed::Embedder::open(true, Some(&args.index_folder().join("worker-query.log")))
            .map_err(Error::fatal)?;
        let prompt = if code.is_some() { model::code_query_prompt(&query) } else { model::query_prompt(&query) };
        encoder.embed_texts(&[prompt]).map_err(Error::fatal)?.pop()
    } else {
        None
    };
    let encoded = Instant::now();
    let limit = args.number("-n").unwrap_or(10);
    let z_gate = args.number("--zgate").unwrap_or(2.0);
    let hits = searcher.search(&query, vector.as_deref(), args.kinds()?.as_ref(), limit, z_gate, None, false)?;
    let searched = encoded.elapsed();
    // --text: the matched part's whole text, for whoever reads the results next (a person, or an agent).
    let texts: Vec<Option<String>> = if args.flags.contains("--text") {
        hits.iter().map(|hit| searcher.text_of_segment(hit.segment_id)).collect::<Result<_>>()?
    } else {
        vec![None; hits.len()]
    };
    if args.flags.contains("--json") {
        let rows: Vec<Value> = hits.iter().zip(&texts).map(|(hit, text)| {
            let mut row = hit.json();
            if let Some(text) = text {
                row["text"] = text.clone().into();
            }
            row
        }).collect();
        println!("{}", serde_json::to_string_pretty(&rows).unwrap());
        return Ok(());
    }
    if hits.is_empty() {
        println!("no good matches");
    }
    // The whole path on the first line: what opens or reads the file next takes it as it is.
    for (rank, (hit, text)) in hits.iter().zip(&texts).enumerate() {
        let why: Vec<String> = [hit.z.map(|z| format!("z {z:.1}")), hit.keyword_rank.map(|r| format!("words #{r}"))]
            .into_iter().flatten().collect();
        let place = location(hit);
        println!("{:>2}. {:<10} {}{}   ({})", rank + 1, hit.kind.name(), hit.path,
                 if place.is_empty() { String::new() } else { format!("  [{place}]") }, why.join(", "));
        if let Some(text) = text.as_ref().filter(|t| !t.trim().is_empty()) {
            for line in text.trim().lines() {
                println!("      | {line}");
            }
        } else if let Some(excerpt) = hit.excerpt.as_ref().filter(|e| !e.is_empty()) {
            println!("      “{}”", excerpt.chars().take(160).collect::<String>());
        }
    }
    println!("({} vectors · query encoder {:.2} s incl. start-up · search {:.1} ms)", searcher.vector_count(),
             (encoded - started).as_secs_f64(), searched.as_secs_f64() * 1000.0);
    Ok(())
}

fn status(args: &Arguments) -> Result<()> {
    let store = args.store()?;
    let counts = store.counts()?;
    println!("index: {}", paths::tilde(&store.file.display().to_string()));
    println!("vectors: {} in {} segments", counts.vectors, counts.segments);
    for kind in FileKind::ALL {
        let Some(states) = counts.by_kind_and_state.get(kind.name()) else { continue };
        let total: i64 = states.values().sum();
        let line = states.iter().map(|(s, c)| format!("{c} {s}")).collect::<Vec<_>>().join(" · ");
        println!("  {:<11} {total:>6}   {line}", kind.name());
    }
    if counts.unfinished > 0 {
        println!("long files still being read: {} ({} pages and chunks left)", counts.unfinished, counts.left);
    }
    if let Some(fingerprint) = store.meta("fingerprint")? {
        println!("built with: {fingerprint}");
    }
    println!("this spotlite: llama.cpp {}, vectors {}", embed::LLAMA_BUILD_TAG, embed::VECTOR_VERSION);
    println!("size: {:.1} MB", store::Store::size_of(&store.file) as f64 / 1e6);
    Ok(())
}

fn index_service(args: &Arguments) -> Result<i32> {
    let folder = args.index_folder();
    let part = |file: &str, options: IndexOptions, roots: Vec<String>| -> Result<index_service::Part> {
        let mut indexer = indexer::Indexer::new(store::Store::open(&folder, file)?, options)?;
        indexer.costs_folder = Some(folder.clone());
        // With --prune, files under none of the given folders leave the index (the shell passes all its folders).
        let keep_only = args.flags.contains("--prune").then(|| roots.clone());
        Ok(index_service::Part { indexer, roots, keep_only })
    };
    let mut code_options = args.options();
    code_options.code = true;
    code_options.excluded_folders = args.list("--code-exclude").iter().map(|f| paths::folder_path(f)).collect();
    let code_roots: Vec<String> = args.list("--code-root").iter().map(|f| paths::folder_path(f)).collect();
    // A file whose reading runs away ends this helper, noted for the next one.
    index_service::start_watchdog(folder.clone(),
        args.number::<u64>("--memory-limit").map_or_else(index_service::default_memory_limit, |mb| mb << 20),
        Duration::from_secs_f64(args.number("--time-limit").unwrap_or(15.0 * 60.0)));
    let main = part(store::MAIN_FILE, args.options(), args.roots())?;
    let code = if code_roots.is_empty() { None } else { Some(part(store::CODE_FILE, code_options, code_roots)?) };
    let service = index_service::IndexService::new(main, code, folder, args.flags.contains("--backfill"),
                                                   args.number("--new-limit").unwrap_or(50),
                                                   args.number("--linger").unwrap_or(60.0));
    Ok(service.serve())
}

/// A helper ends when the shell that started it does (it may be busy and not reading its stdin).
fn watch_parent() {
    let parent = std::os::unix::process::parent_id();
    if parent == 1 {
        std::process::exit(0);
    }
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        if std::os::unix::process::parent_id() != parent {
            std::process::exit(0);
        }
    });
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = raw.first().filter(|c| !["-h", "--help", "help"].contains(&c.as_str())) else {
        println!("{USAGE}");
        return;
    };
    let result = Arguments::parse(&raw[1..]).and_then(|args| {
        if ["index-service", "search-service", "download-model"].contains(&command.as_str())
            || (command == "estimate" && args.flags.contains("--json")) {
            watch_parent();
        }
        match command.as_str() {
            "estimate" => estimate(&args).map(|_| 0),
            "index" => index(&args).map(|_| 0),
            "search" => search_command(&args).map(|_| 0),
            "status" => status(&args).map(|_| 0),
            "index-service" => index_service(&args),
            "search-service" => search_service::serve(search_service::Options {
                index: args.index_folder(), roots: args.roots(),
                skipped: args.list("--exclude").iter().map(|f| paths::folder_path(f)).collect(),
                types: IndexOptions::extensions(&args.list("--exclude-ext").join(",")),
                code_roots: args.list("--code-root").iter().map(|f| paths::folder_path(f)).collect(),
                code_skipped: args.list("--code-exclude").iter().map(|f| paths::folder_path(f)).collect(),
                idle_seconds: args.number("--encoder-idle").unwrap_or(300.0),
            }),
            "download-model" => Ok(download::run(args.values.get("--to").cloned())),
            _ => {
                println!("{USAGE}");
                Ok(2)
            }
        }
    });
    match result {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}
