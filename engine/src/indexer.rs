//! Keeps the index in step with the chosen folders, then reads and embeds whatever is pending. Reading (decoding
//! pictures, rendering PDF pages, decoding sound and video) runs on a second thread one job ahead, so reading files and
//! embedding them overlap.
use crate::code::{self, CodeFile};
use crate::crawler::{self, CrawlReport};
use crate::embed::Embedder;
use crate::error::{Error, Result};
use crate::estimate::MeasuredCosts;
use crate::extract::{self, ExtractError, MediaPiece, PreparedImage};
use crate::model::{FileKind, IndexOptions, Modality, SegmentKind, document_prompt};
use crate::ocr;
use crate::paths;
use crate::pdf;
use crate::store::{FileRecord, Info, Store};
use crate::text;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

/// The model, started when the first file needs it and shared by both indexes; `close` frees it (about 1 GB).
pub struct Model {
    embedder: Option<Embedder>,
    log: Option<PathBuf>,
}

impl Model {
    pub fn new(log: Option<PathBuf>) -> Model {
        Model { embedder: None, log }
    }

    pub fn get(&mut self) -> Result<&mut Embedder> {
        if self.embedder.is_none() {
            self.embedder = Some(Embedder::open(false, self.log.as_deref()).map_err(Error::fatal)?);
        }
        Ok(self.embedder.as_mut().unwrap())
    }

    pub fn is_running(&self) -> bool {
        self.embedder.is_some()
    }

    pub fn close(&mut self) {
        self.embedder = None;
    }
}

#[derive(Debug, Default)]
pub struct SyncSummary {
    pub added: usize,
    pub changed: usize,
    pub moved: usize,
    pub removed: usize,
    pub unchanged: usize,
    pub crawl: CrawlReport,
    /// Files this sync queued (new or changed), so a watcher can index exactly those first.
    pub queued: Vec<i64>,
}

pub struct Event {
    pub done: usize,
    pub total: usize,
    pub path: String,
    pub kind: FileKind,
    pub state: String,
    pub seconds: f64,
    pub note: Option<String>,
}

/// One step of the later pass: how far it has got (`done` of the `total` pages and chunks), the long files not
/// finished yet, and the file it just read more of.
pub struct Step {
    pub done: usize,
    pub total: usize,
    pub files: usize,
    pub path: String,
    pub kind: FileKind,
    pub state: String,
    pub seconds: f64,
    pub note: Option<String>,
}

/// The file being read right now, and since when: for the watchdog, which ends the helper when reading one file runs
/// away (and has to say which file that was).
pub static NOW_READING: Mutex<Option<(String, Instant)>> = Mutex::new(None);

fn start_reading(path: &str) {
    *NOW_READING.lock().unwrap() = Some((path.to_string(), Instant::now()));
}

/// Files read and embedded together (images 8 at a time, other files one by one), or one step of a long file.
#[derive(Clone)]
struct Job {
    files: Vec<FileRecord>,
    step: Option<Range<usize>>,
}

struct Outcome {
    state: &'static str,
    detail: String,
}

enum Payload {
    Nothing,
    Images { screenshot: bool, ready: Vec<(usize, PreparedImage, String)> },
    Pdf { pages: Vec<pdf::Page>, read: Range<usize>, total: usize, title: Option<String> },
    Doc { chunks: Vec<String>, read: Range<usize>, total: usize, characters: usize },
    Audio { pieces: Vec<MediaPiece>, duration: f64 },
    Video { frames: Vec<MediaPiece>, sound: Vec<MediaPiece>, duration: f64 },
    Code(CodeFile),
}

/// A job read from disk and decoded, ready for the model. Its scratch files live in `folder`.
struct Prepared {
    job: Job,
    folder: PathBuf,
    outcomes: HashMap<usize, Outcome>,
    payload: Payload,
}

pub struct Indexer {
    pub store: Store,
    pub options: IndexOptions,
    scratch: PathBuf,
    jobs_prepared: usize,
    /// When set, an index built with another model or other settings is emptied and embedded again, and this hears
    /// about it. When None, indexing stops with a fingerprint mismatch.
    pub reset_on_model_change: Option<Box<dyn FnMut(&str, &str)>>,
    /// Where what indexing costs here is kept, for estimates.
    pub costs_folder: Option<PathBuf>,
    measured: MeasuredCosts,
    /// The fingerprint was checked and written since the model started.
    adopted: bool,
}

type Results = Vec<(String, Option<String>)>;

impl Indexer {
    pub fn new(store: Store, options: IndexOptions) -> Result<Indexer> {
        // One folder per process ($TMPDIR/SpotLite-<pid>, cleared up after a process that died), one in it per indexer.
        let scratch = std::env::temp_dir().join(format!("SpotLite-{}", std::process::id())).join(extract::unique_name());
        std::fs::create_dir_all(&scratch)?;
        Ok(Indexer { store, options, scratch, jobs_prepared: 0, reset_on_model_change: None, costs_folder: None,
                     measured: MeasuredCosts::default(), adopted: false })
    }

    /// Removes the scratch folder (for a process about to exit).
    pub fn finish(&self) {
        let _ = std::fs::remove_dir_all(&self.scratch);
    }

    // MARK: Sync

    /// Crawls `roots` and reconciles the index: new files are queued, changed ones re-queued, moved ones re-pathed
    /// (no re-embedding), and files that are gone or newly excluded are removed.
    pub fn sync(&self, roots: &[String]) -> Result<SyncSummary> {
        let crawl = crawler::crawl(roots, &self.options);
        let existing = self.store.files_under(roots)?;
        let by_path: HashMap<&str, &FileRecord> = existing.iter().map(|f| (f.path.as_str(), f)).collect();
        let by_inode: HashMap<(u64, u64), &FileRecord> = existing.iter().map(|f| ((f.device, f.inode), f)).collect();
        let seen: HashSet<&str> = crawl.candidates.iter().map(|c| c.path.as_str()).collect();
        let mut moved_from: HashSet<String> = HashSet::new();
        let mut summary = SyncSummary::default();
        self.store.transaction(|| {
            for file in &crawl.candidates {
                if let Some(row) = by_path.get(file.path.as_str()) {
                    if row.size != file.size || (row.mtime - file.mtime).abs() > 0.001 || row.kind != file.kind {
                        self.store.requeue_as(row.id, file)?;
                        summary.changed += 1;
                        summary.queued.push(row.id);
                    } else {
                        summary.unchanged += 1;
                    }
                } else if let Some(row) = by_inode.get(&(file.device, file.inode)).filter(|row| {
                    !seen.contains(row.path.as_str()) && !moved_from.contains(&row.path) && row.size == file.size
                        && (row.mtime - file.mtime).abs() < 0.001
                }) {
                    self.store.move_to(row.id, &file.path)?;
                    moved_from.insert(row.path.clone());
                    summary.moved += 1;
                } else {
                    summary.queued.push(self.store.insert(file)?);
                    summary.added += 1;
                }
            }
            // A root that went away while it was walked (a drive unplugged mid-crawl) looked emptier than it is: its
            // rows stay rather than count as deleted.
            let vanished: Vec<String> = roots.iter().filter(|r| !Path::new(r).exists())
                .map(|r| if r.ends_with('/') { r.clone() } else { format!("{r}/") }).collect();
            for row in &existing {
                if !seen.contains(row.path.as_str()) && !moved_from.contains(&row.path)
                    && !vanished.iter().any(|v| row.path.starts_with(v)) {
                    self.store.delete(row.id)?;
                    summary.removed += 1;
                }
            }
            Ok(())
        })?;
        summary.crawl = crawl;
        Ok(summary)
    }

    // MARK: Run

    /// Processes pending files, screenshots first, newest first (`only`: just these files). Long PDFs and documents get
    /// their first step and stay `partial` for `finish_long_files`. Per-file problems are noted on the file; a fatal
    /// error stops the run (pending files stay pending). `should_stop` is asked between jobs.
    pub fn run(&mut self, model: &mut Model, limit: Option<usize>, only: Option<&HashSet<i64>>,
               should_stop: &mut dyn FnMut() -> bool, on_event: &mut dyn FnMut(Event)) -> Result<BTreeMap<String, usize>> {
        let queue = self.store.pending(limit, only)?;
        let total = queue.len();
        let jobs: Vec<Job> = batches(queue).into_iter().map(|files| Job { files, step: None }).collect();
        let mut states: BTreeMap<String, usize> = BTreeMap::new();
        let mut done = 0;
        self.process(model, jobs, should_stop, &mut |_, job, results, seconds| {
            for (offset, file) in job.files.iter().enumerate() {
                let (state, note) = &results[offset];
                *states.entry(state.clone()).or_default() += 1;
                on_event(Event { done: done + offset + 1, total, path: file.path.clone(), kind: file.kind,
                                 state: state.clone(), seconds, note: note.clone() });
            }
            done += job.files.len();
        }, &mut |_| false)?;
        Ok(states)
    }

    /// The later pass: reads the rest of long PDFs and documents, a step at a time. Each step is saved as one
    /// transaction, so stopping between steps loses nothing.
    pub fn finish_long_files(&mut self, model: &mut Model, should_stop: &mut dyn FnMut() -> bool,
                             on_step: &mut dyn FnMut(Step)) -> Result<BTreeMap<String, usize>> {
        let mut jobs = vec![];
        for file in self.store.unfinished()? {
            let info = self.store.info(file.id)?;
            let size = if file.kind == FileKind::Pdf { self.options.pdf_step } else { self.options.doc_step };
            let mut start = match info.get("read_to").and_then(|v| v.parse().ok()) {
                Some(read_to) => read_to,
                None => self.store.last_place(file.id)?.unwrap_or(0),
            };
            // At least one step, which settles a file whose length was only guessed.
            let end = info.get("read_end").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0).max(start + 1);
            while start < end {
                jobs.push(Job { files: vec![file.clone()], step: Some(start..(start + size).min(end)) });
                start += size;
            }
        }
        // Progress counts pages and chunks: what's read once each job is through.
        let mut through = vec![];
        for job in &jobs {
            through.push(through.last().copied().unwrap_or(0) + job.step.as_ref().map_or(0, |s| s.len()));
        }
        let mut files = jobs.iter().map(|j| j.files[0].id).collect::<HashSet<_>>().len();
        let mut states: BTreeMap<String, usize> = BTreeMap::new();
        let finished: std::cell::RefCell<HashSet<i64>> = Default::default();
        let total = through.last().copied().unwrap_or(0);
        self.process(model, jobs, should_stop, &mut |index, job, results, seconds| {
            let file = &job.files[0];
            let (state, note) = &results[0];
            if state != "partial" && finished.borrow_mut().insert(file.id) {
                *states.entry(state.clone()).or_default() += 1;
                files -= 1;
            }
            on_step(Step { done: through[index], total, files, path: file.path.clone(), kind: file.kind,
                           state: state.clone(), seconds, note: note.clone() });
        }, &mut |job| finished.borrow().contains(&job.files[0].id))?;
        Ok(states)
    }

    /// Runs `jobs` in order until `should_stop`, passing each one's results to `committed`; jobs that `skip` turns
    /// down when their turn comes are left out. While the model embeds one job, the next is read on another thread.
    fn process(&mut self, model: &mut Model, jobs: Vec<Job>, should_stop: &mut dyn FnMut() -> bool,
               committed: &mut dyn FnMut(usize, &Job, &Results, f64), skip: &mut dyn FnMut(&Job) -> bool) -> Result<()> {
        let mut ahead: Option<std::thread::JoinHandle<Prepared>> = None;
        let mut outcome = Ok(());
        for (index, job) in jobs.iter().enumerate() {
            if should_stop() {
                break;
            }
            let started = Instant::now();
            let prepared = match ahead.take() {
                Some(handle) => handle.join().expect("reading a file panicked"),
                None => self.prepare(job.clone()).join().expect("reading a file panicked"),
            };
            if index + 1 < jobs.len() {
                ahead = Some(self.prepare(jobs[index + 1].clone()));
            }
            if skip(job) {
                let _ = std::fs::remove_dir_all(&prepared.folder);
                continue;
            }
            let model_was_running = model.is_running();
            let result = self.commit(model, &prepared);
            let _ = std::fs::remove_dir_all(&prepared.folder);
            let results = match result {
                Ok(results) => results,
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            };
            let seconds = started.elapsed().as_secs_f64();
            if model_was_running {
                self.measure(&prepared, seconds);   // else it's seconds of the model starting
            }
            committed(index, job, &results, seconds / job.files.len() as f64);
        }
        // A job prepared for a run that stopped early: its files go.
        if let Some(handle) = ahead {
            if let Ok(prepared) = handle.join() {
                let _ = std::fs::remove_dir_all(&prepared.folder);
            }
        }
        if let Some(folder) = &self.costs_folder {
            if !self.measured.tallies.is_empty() {
                self.measured.merge_into(folder);
                self.measured = MeasuredCosts::default();
            }
        }
        outcome
    }

    /// What a job read and how long it took, for the estimates.
    fn measure(&mut self, prepared: &Prepared, seconds: f64) {
        match &prepared.payload {
            Payload::Images { screenshot, ready } => {
                self.measured.add(if *screenshot { "screenshot" } else { "image" }, ready.len() as f64, seconds)
            }
            Payload::Pdf { pages, .. } => self.measured.add("pdfPage", pages.len() as f64, seconds),
            Payload::Doc { chunks, .. } => {
                let worded = chunks.iter().filter(|c| text::has_words(c)).count();
                self.measured.add("docChunk", worded as f64, seconds);
            }
            Payload::Audio { duration, .. } => self.measured.add("audioSecond", *duration, seconds),
            Payload::Video { duration, .. } => self.measured.add("videoSecond", *duration, seconds),
            Payload::Code(file) => self.measured.add("codeStretch", file.pieces.len() as f64, seconds),
            Payload::Nothing => {}
        }
    }

    /// Compares the index with this build's model and settings, without starting the model; a change empties the
    /// index (see `reset_on_model_change`) or is a fatal mismatch.
    pub fn check_model(&mut self) -> Result<()> {
        self.adopt(&format!("{} | {}", crate::embed::vector_source(), self.options.fingerprint()))
    }

    fn adopt(&mut self, fingerprint: &str) -> Result<()> {
        let Some(indexed) = self.store.meta("fingerprint")? else { return Ok(()) };
        if indexed == fingerprint {
            return Ok(());
        }
        let Some(callback) = self.reset_on_model_change.as_mut() else {
            return Err(Error::fatal(format!(
                "this index was built with different settings or model:\n  index: {indexed}\n  now:   {fingerprint}\n\
                 re-run with --reindex (vectors from different settings can't be compared)")));
        };
        // Everything is queued again.
        self.store.reset_all()?;
        callback(&indexed, fingerprint);
        Ok(())
    }

    fn worker<'m>(&mut self, model: &'m mut Model) -> Result<&'m mut Embedder> {
        if !self.adopted {
            let fingerprint = format!("{} | {}", crate::embed::vector_source(), self.options.fingerprint());
            self.adopt(&fingerprint)?;
            self.store.set_meta("fingerprint", Some(&fingerprint))?;
            self.store.set_meta("vector_source", Some(&crate::embed::vector_source()))?;
            self.adopted = true;
        }
        model.get()
    }

    // MARK: Prepare (another thread: files only, no database, no model)

    fn prepare(&mut self, job: Job) -> std::thread::JoinHandle<Prepared> {
        self.jobs_prepared += 1;
        let folder = self.scratch.join(format!("batch-{}", self.jobs_prepared));
        let options = self.options.clone();
        std::thread::spawn(move || {
            let prepared = prepare(job, &options, folder);
            *NOW_READING.lock().unwrap() = None;
            prepared
        })
    }

    // MARK: Commit (the indexing thread: the model and the database)

    fn commit(&mut self, model: &mut Model, prepared: &Prepared) -> Result<Results> {
        let batch = &prepared.job.files;
        let later = prepared.job.step.is_some();
        if let (Some(step), Some(outcome)) = (&prepared.job.step, prepared.outcomes.get(&0)) {
            return Ok(vec![self.stop_reading(&batch[0], step.start, &outcome.detail)?]);
        }
        let mut results: Results = vec![("failed".into(), None); batch.len()];
        for (&slot, outcome) in &prepared.outcomes {
            self.store.finish(batch[slot].id, outcome.state, Some(&outcome.detail), &Info::new())?;
            results[slot] = (outcome.state.into(), Some(outcome.detail.clone()));
        }
        let result = match &prepared.payload {
            Payload::Nothing => Ok(()),
            Payload::Images { screenshot, ready } => self.images(model, batch, *screenshot, ready, &mut results),
            Payload::Pdf { pages, read, total, title } => self.pdf(model, &batch[0], pages, read, *total, title, later)
                .map(|r| results[0] = r),
            Payload::Doc { chunks, read, total, characters } =>
                self.doc(model, &batch[0], chunks, read, *total, *characters, later).map(|r| results[0] = r),
            Payload::Audio { pieces, duration } => self.audio(model, &batch[0], pieces, *duration).map(|r| results[0] = r),
            Payload::Video { frames, sound, duration } =>
                self.video(model, &batch[0], frames, sound, *duration).map(|r| results[0] = r),
            Payload::Code(file) => self.code(model, &batch[0], file).map(|r| results[0] = r),
        };
        match result {
            Ok(()) => Ok(results),
            Err(error) if error.fatal => Err(error),
            Err(error) => {
                if let Some(step) = &prepared.job.step {
                    return Ok(vec![self.stop_reading(&batch[0], step.start, &error.message)?]);
                }
                for (slot, file) in batch.iter().enumerate() {
                    if !prepared.outcomes.contains_key(&slot) {
                        self.store.finish(file.id, "failed", Some(&error.message), &Info::new())?;
                        results[slot] = ("failed".into(), Some(error.message.clone()));
                    }
                }
                Ok(results)
            }
        }
    }

    /// A long file that can't be read any further keeps the pages or chunks read so far, and counts as done.
    fn stop_reading(&self, file: &FileRecord, place: usize, problem: &str) -> Result<(String, Option<String>)> {
        let mut info = self.store.info(file.id)?;
        info.remove("read_to");
        info.remove("read_end");
        let unit = if file.kind == FileKind::Pdf { "page" } else { "chunk" };
        self.store.finish(file.id, "done", Some(&format!("read to {unit} {place}, not after: {problem}")), &info)?;
        Ok(("done".into(), Some(format!("stopped before {unit} {}: {problem}", place + 1))))
    }

    fn images(&mut self, model: &mut Model, batch: &[FileRecord], screenshot: bool,
              ready: &[(usize, PreparedImage, String)], results: &mut Results) -> Result<()> {
        if ready.is_empty() {
            return Ok(());
        }
        let budget = if screenshot { self.options.screenshot_budget } else { self.options.photo_budget };
        let worker = self.worker(model)?;
        let vectors: Vec<_> = ready.iter().map(|(_, image, _)| worker.embed_image(&image.image, budget)).collect();
        self.store.transaction(|| {
            for ((slot, image, ocr), vector) in ready.iter().zip(&vectors) {
                let file = &batch[*slot];
                let vector = match vector {
                    Ok(vector) => vector,
                    Err(error) => {
                        self.store.finish(file.id, "failed", Some(error), &Info::new())?;
                        results[*slot] = ("failed".into(), Some(error.clone()));
                        continue;
                    }
                };
                self.store.add_segment(file.id, SegmentKind::Image, Modality::Image, None, None, None, None, Some(vector))?;
                // The words on screen are for keyword search only; meaning comes from the picture alone.
                if !ocr.is_empty() {
                    self.store.add_segment(file.id, SegmentKind::Ocr, Modality::Text, None, None,
                                           Some(&text::excerpt(ocr, 240)), Some(ocr), None)?;
                }
                let mut info = Info::from([("width".into(), image.width.to_string()),
                                           ("height".into(), image.height.to_string())]);
                if let Some(taken) = &image.taken {
                    info.insert("taken".into(), taken.clone());
                }
                if screenshot {
                    info.insert("ocr_chars".into(), ocr.chars().count().to_string());
                }
                self.store.finish(file.id, "done", None, &info)?;
                results[*slot] = ("done".into(), screenshot.then(|| format!("ocr {} chars", ocr.chars().count())));
            }
            Ok(())
        })
    }

    /// Pages `read` of a PDF. A long one stays `partial` with `read_to` and `read_end` in its info until its last step.
    #[allow(clippy::too_many_arguments)]
    fn pdf(&mut self, model: &mut Model, file: &FileRecord, pages: &[pdf::Page], read: &Range<usize>, total: usize,
           title: &Option<String>, later: bool) -> Result<(String, Option<String>)> {
        let end = total.min(self.options.max_pages);
        let unread = read.end < end;
        if pages.is_empty() && !later && !unread {
            self.store.finish(file.id, "skipped", Some("no pages"), &Info::new())?;
            return Ok(("skipped".into(), Some("no pages".into())));
        }
        let title = title.clone().unwrap_or_else(|| paths::stem(&file.path).to_string());
        let prompt = |text: &str| document_prompt(Some(&title), &text.chars().take(4000).collect::<String>());
        let text_pages: Vec<&pdf::Page> = pages.iter().filter(|p| p.image.is_none()).collect();
        // A page that's mostly digits (a table, a data listing) gets keyword search only.
        let word_pages: Vec<&pdf::Page> = text_pages.iter().copied().filter(|p| text::has_words(&p.text)).collect();
        let image_pages: Vec<&pdf::Page> = pages.iter().filter(|p| p.image.is_some()).collect();
        let garbled_pages: Vec<&pdf::Page> = pages.iter().filter(|p| p.garbled.is_some()).collect();
        let mut text_vectors: HashMap<usize, Vec<f32>> = HashMap::new();
        let (mut image_vectors, mut garbled_vectors) = (vec![], vec![]);
        if !word_pages.is_empty() || !image_pages.is_empty() {
            let worker = self.worker(model)?;
            let vectors = worker.embed_texts(&word_pages.iter().map(|p| prompt(&p.text)).collect::<Vec<_>>())
                .map_err(Error::fatal)?;
            text_vectors = word_pages.iter().map(|p| p.number).zip(vectors).collect();
            image_vectors = image_pages.iter()
                .map(|p| worker.embed_image(p.image.as_ref().unwrap(), self.options.page_budget)).collect();
            garbled_vectors = worker.embed_texts(&garbled_pages.iter()
                .map(|p| prompt(p.garbled.as_deref().unwrap())).collect::<Vec<_>>()).map_err(Error::fatal)?;
        }
        self.store.transaction(|| {
            // Meaning only: the garbled words are never shown or matched against what you type.
            for (page, vector) in garbled_pages.iter().zip(&garbled_vectors) {
                self.store.add_segment(file.id, SegmentKind::Page, Modality::Text, Some(page.number as f64), None, None,
                                       None, Some(vector))?;
            }
            for page in &text_pages {
                self.store.add_segment(file.id, SegmentKind::Page, Modality::Text, Some(page.number as f64), None,
                                       Some(&text::excerpt(&page.text, 240)), Some(&page.text),
                                       text_vectors.get(&page.number).map(Vec::as_slice))?;
            }
            for (page, vector) in image_pages.iter().zip(&image_vectors) {
                let excerpt = (!page.text.is_empty()).then(|| text::excerpt(&page.text, 240));
                self.store.add_segment(file.id, SegmentKind::PageImage, Modality::Image, Some(page.number as f64), None,
                                       excerpt.as_deref(), Some(&page.text), vector.as_ref().ok().map(Vec::as_slice))?;
            }
            let mut info = if later { self.store.info(file.id)? } else { Info::new() };
            info.insert("pages".into(), total.to_string());
            let scanned = info.get("scanned_pages").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0) + image_pages.len();
            info.insert("scanned_pages".into(), scanned.to_string());
            if unread {
                info.insert("read_to".into(), read.end.to_string());
                info.insert("read_end".into(), end.to_string());
            } else {
                info.remove("read_to");
                info.remove("read_end");
            }
            let detail = if unread { Some(format!("read to page {} of {total}", read.end)) } else if end < total {
                Some(format!("the first {end} of {total} pages"))
            } else {
                None
            };
            self.store.finish(file.id, if unread { "partial" } else { "done" }, detail.as_deref(), &info)
        })?;
        let span = if later { format!("pages {}–{} of {total}", read.start + 1, read.end) } else {
            format!("{}/{total} pages", pages.len())
        };
        let read_by_ocr = pages.iter().filter(|p| p.ocr).count();
        Ok((if unread { "partial" } else { "done" }.into(), Some(format!("{span}, {} as images{}", image_pages.len(),
            if read_by_ocr > 0 { format!(", {read_by_ocr} read by OCR") } else { String::new() }))))
    }

    /// Chunks `read` of a document, the way `pdf` reads pages.
    #[allow(clippy::too_many_arguments)]
    fn doc(&mut self, model: &mut Model, file: &FileRecord, chunks: &[String], read: &Range<usize>, total: usize,
           characters: usize, later: bool) -> Result<(String, Option<String>)> {
        if total == 0 {
            self.store.finish(file.id, "skipped", Some("no text"), &Info::new())?;
            return Ok(("skipped".into(), Some("no text".into())));
        }
        let title = paths::stem(&file.path).to_string();
        // Text that's mostly digits (a data dump saved as .txt) gets keyword search only.
        let worded: Vec<usize> = (0..chunks.len()).filter(|&i| text::has_words(&chunks[i])).collect();
        let vectors = if worded.is_empty() { vec![] } else {
            let prompts: Vec<String> = worded.iter().map(|&i| document_prompt(Some(&title), &chunks[i])).collect();
            self.worker(model)?.embed_texts(&prompts).map_err(Error::fatal)?
        };
        let vector: HashMap<usize, Vec<f32>> = worded.iter().copied().zip(vectors).collect();
        let end = total.min(self.options.max_chunks);
        let unread = read.end < end;
        self.store.transaction(|| {
            for (offset, chunk) in chunks.iter().enumerate() {
                self.store.add_segment(file.id, SegmentKind::Chunk, Modality::Text, Some((read.start + offset + 1) as f64),
                                       None, Some(&text::excerpt(chunk, 240)), Some(chunk),
                                       vector.get(&offset).map(Vec::as_slice))?;
            }
            let mut info = Info::from([("chunks".into(), end.to_string()), ("chars".into(), characters.to_string())]);
            if unread {
                info.insert("read_to".into(), read.end.to_string());
                info.insert("read_end".into(), end.to_string());
            }
            let detail = if unread { Some(format!("read to chunk {} of {end}", read.end)) } else if end < total {
                Some(format!("the first {end} chunks"))
            } else {
                None
            };
            self.store.finish(file.id, if unread { "partial" } else { "done" }, detail.as_deref(), &info)
        })?;
        let span = if later { format!("chunks {}–{} of {end}", read.start + 1, read.end) } else if unread {
            format!("{} of {end} chunks", chunks.len())
        } else {
            format!("{} chunks", chunks.len())
        };
        let keywords_only = chunks.len() - worded.len();
        Ok((if unread { "partial" } else { "done" }.into(),
            Some(span + &if keywords_only > 0 { format!(", {keywords_only} for keywords only") } else { String::new() })))
    }

    /// A code file's stretches of lines (a notebook's of cells), each embedded with the file's path in its repo as its
    /// title, and kept with its lines for keyword search and for opening at the place.
    fn code(&mut self, model: &mut Model, file: &FileRecord, code: &CodeFile) -> Result<(String, Option<String>)> {
        if code.pieces.is_empty() {
            self.store.finish(file.id, "skipped", Some("no code"), &Info::new())?;
            return Ok(("skipped".into(), Some("no code".into())));
        }
        // Lines that are mostly numbers and symbols (a table of constants) get keyword search only.
        let worded: Vec<usize> = (0..code.pieces.len()).filter(|&i| text::has_words(&code.pieces[i].text)).collect();
        let vectors = if worded.is_empty() { vec![] } else {
            let prompts: Vec<String> = worded.iter().map(|&i| document_prompt(Some(&code.title), &code.pieces[i].text))
                .collect();
            self.worker(model)?.embed_texts(&prompts).map_err(Error::fatal)?
        };
        let vector: HashMap<usize, Vec<f32>> = worded.iter().copied().zip(vectors).collect();
        let kind = if code.cells.is_none() { SegmentKind::Lines } else { SegmentKind::Cells };
        self.store.transaction(|| {
            for (offset, piece) in code.pieces.iter().enumerate() {
                self.store.add_segment(file.id, kind, Modality::Text, Some(piece.first as f64), Some(piece.last as f64),
                                       Some(&code::summary(&piece.text)), Some(&piece.text),
                                       vector.get(&offset).map(Vec::as_slice))?;
            }
            let mut info = Info::from([("lines".into(), code.lines.to_string())]);
            if let Some(cells) = code.cells {
                info.insert("cells".into(), cells.to_string());
            }
            if let Some(repo) = &code.repo {
                info.insert("repo".into(), repo.clone());
            }
            self.store.finish(file.id, "done", None, &info)
        })?;
        let size = code.cells.map_or(format!("{} lines", code.lines), |c| format!("{c} cells"));
        let keywords_only = code.pieces.len() - worded.len();
        Ok(("done".into(), Some(format!("{} stretches, {size}", code.pieces.len())
            + &if keywords_only > 0 { format!(", {keywords_only} for keywords only") } else { String::new() })))
    }

    fn audio(&mut self, model: &mut Model, file: &FileRecord, pieces: &[MediaPiece], duration: f64)
        -> Result<(String, Option<String>)> {
        if pieces.is_empty() {
            self.store.finish(file.id, "skipped", Some("silent or shorter than 2 s"), &Info::new())?;
            return Ok(("skipped".into(), Some("silent or too short".into())));
        }
        let worker = self.worker(model)?;
        let vectors: Vec<_> = pieces.iter()
            .map(|piece| extract::read_window(&piece.path).and_then(|samples| worker.embed_audio(&samples))).collect();
        self.store.transaction(|| {
            for (piece, vector) in pieces.iter().zip(&vectors) {
                self.store.add_segment(file.id, SegmentKind::Audio, Modality::Audio, Some(piece.start), Some(piece.end),
                                       None, None, vector.as_ref().ok().map(Vec::as_slice))?;
            }
            self.store.finish(file.id, "done", None, &Info::from([("duration".into(), format!("{duration:.1}"))]))
        })?;
        Ok(("done".into(), Some(format!("{} windows, {} s", pieces.len(), duration as i64))))
    }

    fn video(&mut self, model: &mut Model, file: &FileRecord, frames: &[MediaPiece], sound: &[MediaPiece],
             duration: f64) -> Result<(String, Option<String>)> {
        if frames.is_empty() && sound.is_empty() {
            self.store.finish(file.id, "skipped", Some("no frames or sound"), &Info::new())?;
            return Ok(("skipped".into(), Some("no frames or sound".into())));
        }
        let budget = self.options.frame_budget;
        let worker = self.worker(model)?;
        let frame_vectors: Vec<_> = frames.iter().map(|frame| {
            let image = image::open(&frame.path).map_err(|e| e.to_string())?.into_rgb8();
            let image = extract::for_model(extract::rgb(image), budget).map_err(|e| e.to_string())?;
            worker.embed_image(&image, budget)
        }).collect();
        let sound_vectors: Vec<_> = sound.iter()
            .map(|piece| extract::read_window(&piece.path).and_then(|samples| worker.embed_audio(&samples))).collect();
        self.store.transaction(|| {
            for (frame, vector) in frames.iter().zip(&frame_vectors) {
                self.store.add_segment(file.id, SegmentKind::Frame, Modality::Image, Some(frame.start), Some(frame.end),
                                       None, None, vector.as_ref().ok().map(Vec::as_slice))?;
            }
            for (piece, vector) in sound.iter().zip(&sound_vectors) {
                self.store.add_segment(file.id, SegmentKind::Audio, Modality::Audio, Some(piece.start), Some(piece.end),
                                       None, None, vector.as_ref().ok().map(Vec::as_slice))?;
            }
            self.store.finish(file.id, "done", None, &Info::from([
                ("duration".into(), format!("{duration:.1}")), ("frames".into(), frames.len().to_string()),
                ("sound_windows".into(), sound.len().to_string())]))
        })?;
        Ok(("done".into(), Some(format!("{} frames, {} sound windows, {} s", frames.len(), sound.len(), duration as i64))))
    }
}

impl Drop for Indexer {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Images and screenshots go to the model 8 at a time; other files one by one.
fn batches(queue: Vec<FileRecord>) -> Vec<Vec<FileRecord>> {
    let mut out: Vec<Vec<FileRecord>> = vec![];
    for file in queue {
        let joins = out.last().is_some_and(|batch: &Vec<FileRecord>| {
            batch.len() < 8 && matches!(batch[0].kind, FileKind::Image | FileKind::Screenshot) && batch[0].kind == file.kind
        });
        if joins {
            out.last_mut().unwrap().push(file);
        } else {
            out.push(vec![file]);
        }
    }
    out
}

fn clamp(range: Range<usize>, limit: usize) -> Range<usize> {
    range.start.min(limit)..range.end.min(limit)
}

fn prepare(job: Job, options: &IndexOptions, folder: PathBuf) -> Prepared {
    let batch = &job.files;
    let mut outcomes: HashMap<usize, Outcome> = HashMap::new();
    let path = batch[0].path.clone();
    start_reading(&path);
    let result: std::result::Result<Payload, ExtractError> = (|| {
        std::fs::create_dir_all(&folder)?;
        Ok(match batch[0].kind {
            FileKind::Image | FileKind::Screenshot => {
                let screenshot = batch[0].kind == FileKind::Screenshot;
                let (max_pixel, budget) = if screenshot { (1536, options.screenshot_budget) } else {
                    (1024, options.photo_budget)
                };
                let mut ready = vec![];
                for (slot, file) in batch.iter().enumerate() {
                    start_reading(&file.path);
                    match extract::prepare_image(&file.path, max_pixel, options.min_image_side, budget) {
                        Ok(image) => ready.push((slot, image, String::new())),
                        Err(error @ ExtractError::TooSmall(..)) => {
                            outcomes.insert(slot, Outcome { state: "skipped", detail: error.to_string() });
                        }
                        Err(error) => {
                            outcomes.insert(slot, Outcome { state: "failed", detail: error.to_string() });
                        }
                    }
                }
                // A screenshot's words, for keyword search: the batch's read side by side, one Tesseract each, while
                // the model embeds the batch before (0.2–2.4 s a screenshot on one core).
                if screenshot && ocr::available() {
                    std::thread::scope(|scope| {
                        let reads: Vec<_> = ready.iter().map(|(slot, _, _)| {
                            let path = &batch[*slot].path;
                            scope.spawn(move || ocr::text_of_file(path))
                        }).collect();
                        for ((_, _, text), read) in ready.iter_mut().zip(reads) {
                            *text = read.join().unwrap_or_default();
                        }
                    });
                }
                Payload::Images { screenshot, ready }
            }
            FileKind::Pdf => {
                // Long PDFs (books) get their first pages now and the rest after everything else.
                let mut wanted = 0..0;
                let budget = options.page_budget;
                let (pages, total, title) = pdf::pages(&path, |total| {
                    let range = job.step.clone().unwrap_or(if total > options.big_pdf_pages { 0..options.pdf_step } else {
                        0..options.max_pages
                    });
                    wanted = clamp(range, total.min(options.max_pages));
                    wanted.clone()
                }, options.min_page_text, 1400, |image| extract::for_model(image, budget).ok())?;
                Payload::Pdf { pages, read: wanted, total, title }
            }
            FileKind::Doc => {
                // The chunks are cut the same way every time, so a later step goes on where the last one stopped.
                let text = extract::document_text(&path)?;
                let chunks = extract::chunks(&text, 1800, 200, options.max_chunks + 1);
                let read = clamp(job.step.clone().unwrap_or(0..options.doc_step), chunks.len().min(options.max_chunks));
                Payload::Doc { chunks: chunks[read.clone()].to_vec(), read, total: chunks.len(),
                               characters: text.chars().count() }
            }
            FileKind::Audio => {
                let (pieces, duration) = extract::audio_windows(&path, options.audio_window, options.audio_hop, &folder)?;
                Payload::Audio { pieces, duration }
            }
            FileKind::Video => {
                let (frames, duration, has_audio) = extract::keyframes(&path, options.frame_interval, 1024, &folder)?;
                let sound = if has_audio {
                    extract::audio_windows(&path, options.audio_window, options.audio_hop, &folder)?.0
                } else {
                    vec![]
                };
                Payload::Video { frames, sound, duration }
            }
            FileKind::Code => Payload::Code(code::read(&path, options)?),
        })
    })();
    let payload = match result {
        Ok(payload) => payload,
        Err(error) => {
            if let ExtractError::LeftOut(_) = error {
                outcomes.insert(0, Outcome { state: "skipped", detail: error.to_string() });
            } else {
                for slot in 0..batch.len() {
                    outcomes.entry(slot).or_insert(Outcome { state: "failed", detail: error.to_string() });
                }
            }
            Payload::Nothing
        }
    };
    Prepared { job, folder, outcomes, payload }
}
