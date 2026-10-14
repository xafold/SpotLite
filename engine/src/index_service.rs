//! `spotlite index-service`: the shell's indexing helper, one process per stretch of work.
//!
//! Protocol: one command per line on stdin, one JSON event per line on stdout.
//!   stdin:  sync | sync code | stop | backfill on | backfill off      (EOF = stop)
//!   stdout: {"event": "missing" | "pruned" | "reindex" | "stopped" | "sync" | "file" | "rest" | "summary" | "waiting" |
//!            "idle" | "error" | "exit", ...}; events about the code index say "code": true
//!
//! The loop: sync, index what the sync just queued (if it's a few files: new files go first), then the backfill if
//! allowed; then the code index the same way; then the rest of long files. Between jobs it yields to stop and to
//! "backfill off" at once, and to a new sync once indexing has had about as long as the last sync took. When it's done
//! it keeps the model warm for `linger` seconds (screenshots come in bursts), then exits. Before the model has
//! downloaded it only syncs (file names are searchable at once), says it's "waiting", and exits. Folders that aren't
//! there (an unplugged drive) or can't be read are skipped, and what was indexed from them stays.
use crate::emit;
use crate::error::Result;
use crate::indexer::{Indexer, Model, NOW_READING};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub struct Part {
    pub indexer: Indexer,
    /// The chosen folders, there or not.
    pub roots: Vec<String>,
    /// When set, files under none of these folders leave the index (folders taken out of the shell).
    pub keep_only: Option<Vec<String>>,
}

impl Part {
    fn code(&self) -> bool {
        self.indexer.store.is_code()
    }
}

struct Flags {
    sync: bool,
    code_sync: bool,
    stop: bool,
    backfill: bool,
    /// A sync asked for while files are being indexed waits until then: indexing gets at least as long as the sync
    /// before it (2 s at the least), or a folder that keeps changing would never get past crawling.
    yield_at: Instant,
}

struct Shared {
    flags: Mutex<Flags>,
    wake: Condvar,
}

pub struct IndexService {
    main: Part,
    code: Option<Part>,
    folder: PathBuf,
    model: Model,
    shared: Arc<Shared>,
    /// The last report of folders that aren't there or can't be read, by index.
    unavailable: [Option<(Vec<String>, Vec<String>)>; 2],
    new_files_limit: usize,
    linger: Duration,
}

/// The file a helper stopped on (the watchdog), written beside the index for the next helper.
pub fn stopped_file(folder: &Path) -> PathBuf {
    folder.join("stopped-file.json")
}

/// The exit status that says "a file was stopped: start a new helper" (EX_TEMPFAIL).
pub const STOPPED_EXIT_CODE: i32 = 75;

impl IndexService {
    pub fn new(main: Part, code: Option<Part>, folder: PathBuf, backfill: bool, new_files_limit: usize, linger: f64) -> Self {
        let model = Model::new(Some(folder.join("worker.log")));
        let flags = Flags { sync: true, code_sync: true, stop: false, backfill, yield_at: Instant::now() };
        IndexService { main, code, folder, model, shared: Arc::new(Shared { flags: Mutex::new(flags), wake: Condvar::new() }),
                       unavailable: [None, None], new_files_limit, linger: Duration::from_secs_f64(linger) }
    }

    fn flags<T>(&self, read: impl FnOnce(&mut Flags) -> T) -> T {
        read(&mut self.shared.flags.lock().unwrap())
    }

    pub fn serve(mut self) -> i32 {
        remove_stale_scratch_folders();
        if let Err(error) = self.start() {
            emit(json!({"event": "error", "message": error.to_string(), "fatal": true}));
            self.finish();
            return 1;
        }
        let shared = self.shared.clone();
        std::thread::spawn(move || read_commands(shared));
        let (mut new_files, mut new_code): (HashSet<i64>, HashSet<i64>) = Default::default();
        let mut code_counted = self.code.is_none();
        loop {
            if self.flags(|f| std::mem::take(&mut f.sync)) {
                self.sync(false, &mut new_files);
            }
            // The code folders are crawled with the first sync, so their counts show from the start; their files are
            // read once the main index has nothing waiting.
            if !code_counted {
                code_counted = true;
                if self.flags(|f| std::mem::take(&mut f.code_sync)) {
                    self.sync(true, &mut new_code);
                }
            }
            if self.flags(|f| f.stop) {
                break;
            }
            if !crate::embed::models_complete() {
                // Nothing to embed with yet; the shell starts another helper once the download is done.
                self.emit_counts("waiting", json!({"for": "model"}));
                break;
            }
            if !new_files.is_empty() {
                if !self.run(false, Some(&new_files)) {
                    break;
                }
                new_files = self.left(false, &new_files);
                continue;
            }
            if self.main.indexer.store.pending_count().unwrap_or(0) > 0 && self.flags(|f| f.backfill) {
                if !self.run(false, None) {
                    break;
                }
                if self.flags(|f| f.sync) {
                    continue;
                }
            }
            // The code index once the main one has nothing waiting: its own sync, what it just queued at once, the
            // rest under the backfill's rules.
            if self.code.is_some() && (self.main.indexer.store.pending_count().unwrap_or(1) == 0 || !self.flags(|f| f.backfill)) {
                if self.flags(|f| std::mem::take(&mut f.code_sync)) {
                    self.sync(true, &mut new_code);
                }
                if !new_code.is_empty() {
                    if !self.run(true, Some(&new_code)) {
                        break;
                    }
                    new_code = self.left(true, &new_code);
                    continue;
                }
                let code_pending = self.code.as_ref().unwrap().indexer.store.pending_count().unwrap_or(0);
                if code_pending > 0 && self.flags(|f| f.backfill) {
                    if !self.run(true, None) {
                        break;
                    }
                    if self.flags(|f| f.sync || f.code_sync) {
                        continue;
                    }
                }
            }
            // Once nothing is waiting: the rest of long PDFs and documents, the same way.
            let code_pending = self.code.as_ref().map_or(0, |c| c.indexer.store.pending_count().unwrap_or(1));
            if self.flags(|f| f.backfill) && self.main.indexer.store.pending_count().unwrap_or(1) == 0 && code_pending == 0
                && self.main.indexer.store.unfinished_count().map_or(0, |c| c.0) > 0 {
                if !self.finish_long_files() {
                    break;
                }
                if self.flags(|f| f.sync || f.code_sync) {
                    continue;
                }
            }
            if self.code.is_some() && self.flags(|f| f.code_sync) {
                continue;
            }
            self.emit_counts("idle", json!({}));
            // Nothing warm to keep around: leave now. Else wait a little for more work.
            if !self.model.is_running() || !self.wait_for_work() {
                break;
            }
        }
        self.finish();
        emit(json!({"event": "exit"}));
        0
    }

    fn start(&mut self) -> Result<()> {
        // A file an earlier helper had to stop on isn't read again until it changes.
        if let Some((path, why)) = take_stopped_file(&self.folder) {
            for part in self.parts_mut() {
                if part.indexer.store.mark_stopped(&path, &why)? {
                    emit(json!({"event": "stopped", "path": path, "why": why, "code": part.code()}));
                }
            }
        }
        // What older indexes couldn't read: screenshots' and scans' words, once OCR is there.
        let again = self.main.indexer.store.read_again_once()?;
        if again > 0 {
            emit(json!({"event": "rescan", "files": again}));
        }
        for part in self.parts_mut() {
            let code = part.code();
            part.indexer.reset_on_model_change = Some(Box::new(move |indexed: &str, now: &str| {
                emit(json!({"event": "reindex", "from": indexed, "to": now, "code": code}));
            }));
            part.indexer.check_model()?;   // a new model: everything is queued again
            if let Some(keep) = &part.keep_only {
                let pruned = part.indexer.store.prune(keep)?;
                if pruned > 0 {
                    emit(json!({"event": "pruned", "files": pruned, "code": code}));
                }
            }
        }
        Ok(())
    }

    fn parts_mut(&mut self) -> Vec<&mut Part> {
        let mut parts = vec![&mut self.main];
        if let Some(code) = self.code.as_mut() {
            parts.push(code);
        }
        parts
    }

    fn part(&mut self, code: bool) -> &mut Part {
        if code { self.code.as_mut().unwrap() } else { &mut self.main }
    }

    fn finish(&mut self) {
        self.main.indexer.finish();
        if let Some(code) = &self.code {
            code.indexer.finish();
        }
        self.model.close();
    }

    /// New files a run stopped before (a sync or stop came first): kept for the next round. Done: anything still
    /// pending joins the backfill.
    fn left(&mut self, code: bool, files: &HashSet<i64>) -> HashSet<i64> {
        let yielded = self.flags(|f| f.sync || f.stop || (code && f.code_sync));
        if !yielded {
            return HashSet::new();
        }
        self.part(code).indexer.store.pending(None, Some(files)).map(|p| p.iter().map(|f| f.id).collect()).unwrap_or_default()
    }

    /// A part's chosen folders that are there and readable right now; also what `pending` and the counts look at.
    fn available_roots(&mut self, code: bool) -> Vec<String> {
        let roots = self.part(code).roots.clone();
        let missing: Vec<String> = roots.iter().filter(|r| !Path::new(r).is_dir()).cloned().collect();
        let unreadable: Vec<String> = roots.iter().filter(|r| !missing.contains(r) && std::fs::read_dir(r).is_err())
            .cloned().collect();
        let report = (missing, unreadable);
        let last = &self.unavailable[usize::from(code)];
        let changed = match last {
            Some(last) => *last != report,
            None => !(report.0.is_empty() && report.1.is_empty()),
        };
        if changed {
            emit(json!({"event": "missing", "folders": report.0, "unreadable": report.1, "code": code}));
        }
        let available: Vec<String> = roots.into_iter().filter(|r| !report.0.contains(r) && !report.1.contains(r)).collect();
        self.unavailable[usize::from(code)] = Some(report);
        self.part(code).indexer.store.set_scope(Some(&available));
        available
    }

    fn sync(&mut self, code: bool, new_files: &mut HashSet<i64>) {
        let started = Instant::now();
        let started_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
        let roots = self.available_roots(code);
        match self.part(code).indexer.sync(&roots) {
            Ok(summary) => {
                if summary.queued.len() <= self.new_files_limit {
                    new_files.extend(&summary.queued);
                }
                self.emit_counts("sync", json!({
                    "code": code, "new": new_files.len(), "added": summary.added, "changed": summary.changed,
                    "moved": summary.moved, "removed": summary.removed, "unchanged": summary.unchanged,
                    "ms": started.elapsed().as_millis() as u64, "started": started_at,
                }));
            }
            Err(error) => emit(json!({"event": "error", "message": format!("sync failed: {error}"), "code": code})),
        }
        let took = started.elapsed().max(Duration::from_secs(2));
        self.flags(|f| f.yield_at = Instant::now() + took);
    }

    /// Embeds a part's pending files (`only` some). False when indexing has to stop.
    fn run(&mut self, code: bool, only: Option<&HashSet<i64>>) -> bool {
        let started = Instant::now();
        let is_new = only.is_some();
        let shared = self.shared.clone();
        let mut should_stop = move || {
            let f = shared.flags.lock().unwrap();
            f.stop || (!is_new && !f.backfill) || (Instant::now() >= f.yield_at && (f.sync || (code && f.code_sync)))
        };
        let IndexService { main, code: code_part, model, .. } = self;
        let part = if code { code_part.as_mut().unwrap() } else { main };
        let result = part.indexer.run(model, None, only, &mut should_stop, &mut |event| {
            emit(json!({"event": "file", "new": is_new, "done": event.done, "total": event.total, "path": event.path,
                        "kind": event.kind.name(), "state": event.state, "seconds": event.seconds,
                        "note": event.note.unwrap_or_default(), "code": code}));
        });
        match result {
            Ok(states) => {
                if !states.is_empty() {
                    emit(json!({"event": "summary", "new": is_new, "states": states, "code": code,
                                "ms": started.elapsed().as_millis() as u64}));
                }
                true
            }
            Err(error) => {
                emit(json!({"event": "error", "message": error.to_string(), "fatal": true, "code": code}));
                false
            }
        }
    }

    /// The rest of long files, yielding as the backfill does. False when indexing has to stop.
    fn finish_long_files(&mut self) -> bool {
        let started = Instant::now();
        let shared = self.shared.clone();
        let has_code = self.code.is_some();
        let mut should_stop = move || {
            let f = shared.flags.lock().unwrap();
            f.stop || !f.backfill || (Instant::now() >= f.yield_at && (f.sync || (f.code_sync && has_code)))
        };
        let result = self.main.indexer.finish_long_files(&mut self.model, &mut should_stop, &mut |step| {
            emit(json!({"event": "rest", "done": step.done, "total": step.total, "files": step.files, "path": step.path,
                        "kind": step.kind.name(), "state": step.state, "seconds": step.seconds,
                        "note": step.note.unwrap_or_default()}));
        });
        match result {
            Ok(states) => {
                if !states.is_empty() {
                    emit(json!({"event": "summary", "rest": true, "states": states,
                                "ms": started.elapsed().as_millis() as u64}));
                }
                true
            }
            Err(error) => {
                emit(json!({"event": "error", "message": error.to_string(), "fatal": true}));
                false
            }
        }
    }

    /// Waits up to `linger` for something to do. False: time's up, or told to stop.
    fn wait_for_work(&mut self) -> bool {
        let deadline = Instant::now() + self.linger;
        let shared = self.shared.clone();
        let mut flags = shared.flags.lock().unwrap();
        loop {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, timeout) = shared.wake.wait_timeout(flags, deadline - now).unwrap();
            flags = guard;
            if timeout.timed_out() {
                return false;
            }
            if flags.stop {
                return false;
            }
            if flags.sync || (flags.code_sync && self.code.is_some()) {
                return true;
            }
            if flags.backfill {
                let pending = self.main.indexer.store.pending_count().unwrap_or(0) > 0
                    || self.code.as_ref().is_some_and(|c| c.indexer.store.pending_count().unwrap_or(0) > 0)
                    || self.main.indexer.store.unfinished_count().map_or(0, |c| c.0) > 0;
                if pending {
                    return true;
                }
            }
        }
    }

    /// An event with both indexes' counts: the main one's as they are, the code index's prefixed "code_".
    fn emit_counts(&self, event: &str, extra: Value) {
        let mut fields = extra;
        fields["event"] = event.into();
        for part in std::iter::once(&self.main).chain(self.code.as_ref()) {
            let Ok(counts) = part.indexer.store.counts() else { continue };
            let prefix = if part.code() { "code_" } else { "" };
            let (mut files, mut searchable, mut pending) = (0, 0, 0);
            for states in counts.by_kind_and_state.values() {
                for (state, count) in states {
                    files += count;
                    if state == "done" || state == "partial" {
                        searchable += count;
                    }
                    if state == "pending" {
                        pending += count;
                    }
                }
            }
            fields[format!("{prefix}files")] = files.into();
            fields[format!("{prefix}searchable")] = searchable.into();
            fields[format!("{prefix}pending")] = pending.into();
            fields[format!("{prefix}vectors")] = counts.vectors.into();
            if !part.code() {
                fields["unfinished"] = counts.unfinished.into();
            }
        }
        emit(fields);
    }
}

fn read_commands(shared: Arc<Shared>) {
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        {
            let mut flags = shared.flags.lock().unwrap();
            match line.trim() {
                "sync" => flags.sync = true,
                "sync code" => flags.code_sync = true,
                "stop" => flags.stop = true,
                "backfill on" => flags.backfill = true,
                "backfill off" => flags.backfill = false,
                _ => {}
            }
        }
        shared.wake.notify_all();
    }
    shared.flags.lock().unwrap().stop = true;   // the shell went away
    shared.wake.notify_all();
}

/// Scratch folders ($TMPDIR/SpotLite-<pid>) of indexing processes that are gone (killed mid-batch).
fn remove_stale_scratch_folders() {
    let temp = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&temp) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix("SpotLite-").and_then(|p| p.parse::<u32>().ok()) else { continue };
        if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The note an earlier helper left, removed as it's read.
fn take_stopped_file(folder: &Path) -> Option<(String, String)> {
    let file = stopped_file(folder);
    let data = std::fs::read(&file).ok()?;
    let _ = std::fs::remove_file(&file);
    let note: Value = serde_json::from_slice(&data).ok()?;
    Some((note["path"].as_str()?.to_string(), note["why"].as_str().unwrap_or("couldn't be read").to_string()))
}

/// Ends the helper when reading one file runs away: past `memory_limit` of resident memory, or past `time_limit` on
/// one file, it writes down the file and why and exits with `STOPPED_EXIT_CODE`. The shell starts a new helper, which
/// marks that file as one that couldn't be read and goes on.
pub fn start_watchdog(folder: PathBuf, memory_limit: u64, time_limit: Duration) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        let reading = NOW_READING.lock().unwrap().clone();
        let why = if resident_memory() > memory_limit {
            Some(format!("needed more than {} of memory to read", size_text(memory_limit)))
        } else if reading.as_ref().is_some_and(|(_, since)| since.elapsed() > time_limit) {
            let seconds = time_limit.as_secs();
            Some(format!("took more than {} to read",
                         if seconds >= 120 { format!("{} minutes", seconds / 60) } else { format!("{seconds} seconds") }))
        } else {
            None
        };
        if let Some(why) = why {
            let path = reading.map(|(path, _)| path);
            let _ = std::fs::write(stopped_file(&folder), json!({"path": path, "why": why}).to_string());
            let shown = path.as_deref().unwrap_or("no file being read");
            emit(json!({"event": "error", "message": format!("watchdog: {why}: {shown}")}));
            eprintln!("watchdog: {why}: {shown}");
            std::process::exit(STOPPED_EXIT_CODE);
        }
    });
}

fn size_text(bytes: u64) -> String {
    if bytes >= 1 << 30 { format!("{:.0} GB", bytes as f64 / (1u64 << 30) as f64) } else { format!("{} MB", bytes >> 20) }
}

fn resident_memory() -> u64 {
    std::fs::read_to_string("/proc/self/statm").ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|p| p.parse::<u64>().ok()))
        .map_or(0, |pages| pages * 4096)
}

/// Half the machine's memory, but at least 4 GB and at most 8 GB: indexing peaks at 1–3 GB on the biggest files.
pub fn default_memory_limit() -> u64 {
    let total = std::fs::read_to_string("/proc/meminfo").ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("MemTotal:"))
            .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()))
        .map_or(8 << 30, |kb| kb * 1024);
    (total / 2).clamp(4 << 30, 8 << 30)
}
