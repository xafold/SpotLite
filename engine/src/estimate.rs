//! How long a first index takes, from metadata alone (counts, PDF page counts, media durations): nothing is decoded.
//! Costs start from fixed numbers and then go by what indexing measured on this machine (`costs.json` beside the
//! index).
use crate::model::{FileKind, IndexOptions};
use crate::store::Candidate;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Seconds of indexing per unit. The Mac's numbers (an M4 Pro); a GPU here is in the same range, a CPU slower, and
/// indexing replaces them with what it measures (`MeasuredCosts`).
#[derive(Clone, Copy, Debug)]
pub struct Costs {
    pub image: f64,
    pub screenshot: f64,
    pub pdf_page: f64,
    pub doc_chunk: f64,
    pub audio_second: f64,
    pub video_second: f64,
    pub code_stretch: f64,
}

impl Default for Costs {
    fn default() -> Self {
        Costs { image: 0.12, screenshot: 0.32, pdf_page: 0.09, doc_chunk: 0.045, audio_second: 0.0089,
                video_second: 0.019, code_stretch: 0.037 }
    }
}

/// How big the code index gets: its words (~2.4 bytes of index a character of code) and a vector row for each stretch.
const CODE_BYTES_PER_CHARACTER: f64 = 2.4;
const CODE_BYTES_PER_STRETCH: f64 = 1600.0;
const CHARACTERS_PER_STRETCH: i64 = 1400;

#[derive(Default, Clone, Debug)]
pub struct Estimate {
    pub files: BTreeMap<FileKind, usize>,
    pub pdf_pages: usize,         // pages the first pass will read
    pub pdf_pages_total: usize,
    pub audio_seconds: f64,
    pub video_seconds: f64,
    pub doc_chunks: usize,
    pub seconds: f64,             // the first pass
    /// Long PDFs and documents, which get their first step in the first pass, and how long the rest takes later.
    pub long_files: usize,
    pub later_seconds: f64,
    pub code_stretches: usize,
    pub bytes: f64,
}

impl std::ops::AddAssign<&Estimate> for Estimate {
    fn add_assign(&mut self, other: &Estimate) {
        for (kind, count) in &other.files {
            *self.files.entry(*kind).or_default() += count;
        }
        self.pdf_pages += other.pdf_pages;
        self.pdf_pages_total += other.pdf_pages_total;
        self.audio_seconds += other.audio_seconds;
        self.video_seconds += other.video_seconds;
        self.doc_chunks += other.doc_chunks;
        self.seconds += other.seconds;
        self.long_files += other.long_files;
        self.later_seconds += other.later_seconds;
        self.code_stretches += other.code_stretches;
        self.bytes += other.bytes;
    }
}

pub fn estimate(candidates: &[&Candidate], options: &IndexOptions, costs: &Costs) -> Estimate {
    let mut estimate = Estimate::default();
    for file in candidates {
        *estimate.files.entry(file.kind).or_default() += 1;
        match file.kind {
            FileKind::Image => estimate.seconds += costs.image,
            FileKind::Screenshot => estimate.seconds += costs.screenshot,
            FileKind::Pdf => {
                let total = crate::pdf::page_count(&file.path);
                let all = total.min(options.max_pages);
                let first = if total > options.big_pdf_pages { options.pdf_step.min(all) } else { all };
                estimate.pdf_pages_total += total;
                estimate.pdf_pages += first;
                estimate.seconds += first as f64 * costs.pdf_page;
                if all > first {
                    estimate.long_files += 1;
                    estimate.later_seconds += (all - first) as f64 * costs.pdf_page;
                }
            }
            FileKind::Doc => {
                let all = ((file.size / 2000) as usize).min(options.max_chunks).max(1);
                let first = all.min(options.doc_step);
                estimate.doc_chunks += first;
                estimate.seconds += first as f64 * costs.doc_chunk;
                if all > first {
                    estimate.long_files += 1;
                    estimate.later_seconds += (all - first) as f64 * costs.doc_chunk;
                }
            }
            FileKind::Audio => {
                let seconds = crate::extract::duration(&file.path).unwrap_or(0.0);
                estimate.audio_seconds += seconds;
                estimate.seconds += seconds * costs.audio_second;
            }
            FileKind::Video => {
                let seconds = crate::extract::duration(&file.path).unwrap_or(0.0);
                estimate.video_seconds += seconds;
                estimate.seconds += seconds * costs.video_second;
            }
            FileKind::Code => {
                // A notebook's file is mostly its outputs, which aren't read: a guess at its cells' share.
                let characters = if file.path.to_lowercase().ends_with(".ipynb") { file.size / 8 } else { file.size };
                let stretches = ((characters + CHARACTERS_PER_STRETCH - 1) / CHARACTERS_PER_STRETCH).max(1) as usize;
                estimate.code_stretches += stretches;
                estimate.seconds += stretches as f64 * costs.code_stretch;
                estimate.bytes += characters as f64 * CODE_BYTES_PER_CHARACTER + stretches as f64 * CODE_BYTES_PER_STRETCH;
            }
        }
    }
    estimate
}

/// Estimates for each subfolder one level down, and for the files right in `folder` (under its own path): the folder
/// lists show where a big first pass comes from.
pub fn by_subfolder(candidates: &[Candidate], folder: &str, options: &IndexOptions, costs: &Costs)
    -> HashMap<String, Estimate> {
    let prefix = if folder.ends_with('/') { folder.to_string() } else { format!("{folder}/") };
    let real = format!("{}/", crate::paths::real_path(folder));
    let mut groups: HashMap<String, Vec<&Candidate>> = HashMap::new();
    for file in candidates {
        let rest = file.path.strip_prefix(&prefix).or_else(|| file.path.strip_prefix(&real));
        let key = rest.and_then(|rest| rest.find('/').map(|slash| format!("{prefix}{}", &rest[..slash])))
            .unwrap_or_else(|| folder.to_string());
        groups.entry(key).or_default().push(file);
    }
    groups.into_iter().map(|(key, files)| (key, estimate(&files, options, costs))).collect()
}

#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug, PartialEq)]
pub struct Tally {
    pub seconds: f64,
    pub units: f64,
}

/// How this machine compares to the one behind `Costs`: 1 is as fast, 2 takes twice as long.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Speed {
    pub embedding: f64,
    pub ocr: f64,
}

/// What indexing costs here, from the indexing itself: seconds and units summed kind by kind as jobs finish.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct MeasuredCosts {
    #[serde(default)]
    pub tallies: BTreeMap<String, Tally>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<Speed>,
}

/// Units a kind needs before its measured cost is trusted: a few dozen files, or minutes of media.
fn enough(name: &str) -> f64 {
    match name {
        "image" | "screenshot" => 20.0,
        "pdfPage" | "docChunk" => 30.0,
        "audioSecond" | "videoSecond" => 600.0,
        "codeStretch" => 100.0,
        _ => f64::INFINITY,
    }
}

static LOCK: Mutex<()> = Mutex::new(());

impl MeasuredCosts {
    /// A job that took `seconds` for `units` of `name`. Recent work counts most: past ten times what's enough, what
    /// came before is halved.
    pub fn add(&mut self, name: &str, units: f64, seconds: f64) {
        if units <= 0.0 || seconds <= 0.0 {
            return;
        }
        let tally = self.tallies.entry(name.to_string()).or_default();
        let limit = enough(name);
        if tally.units > 10.0 * if limit.is_finite() { limit } else { 20.0 } {
            tally.seconds /= 2.0;
            tally.units /= 2.0;
        }
        tally.seconds += seconds;
        tally.units += units;
    }

    /// The costs to estimate with: measured ones where there are enough, else the fixed ones.
    pub fn costs(&self) -> Costs {
        let mut costs = Costs::default();
        if let Some(speed) = self.speed {
            let embedding = speed.embedding.clamp(0.25, 10.0);
            for cost in [&mut costs.image, &mut costs.screenshot, &mut costs.pdf_page, &mut costs.doc_chunk,
                         &mut costs.audio_second, &mut costs.video_second, &mut costs.code_stretch] {
                *cost *= embedding;
            }
        }
        let measured = |name: &str| self.tallies.get(name).filter(|t| t.units >= enough(name)).map(|t| t.seconds / t.units);
        costs.image = measured("image").unwrap_or(costs.image);
        costs.screenshot = measured("screenshot").unwrap_or(costs.screenshot);
        costs.pdf_page = measured("pdfPage").unwrap_or(costs.pdf_page);
        costs.doc_chunk = measured("docChunk").unwrap_or(costs.doc_chunk);
        costs.audio_second = measured("audioSecond").unwrap_or(costs.audio_second);
        costs.video_second = measured("videoSecond").unwrap_or(costs.video_second);
        costs.code_stretch = measured("codeStretch").unwrap_or(costs.code_stretch);
        costs
    }

    pub fn file(folder: &Path) -> PathBuf {
        folder.join("costs.json")
    }

    pub fn load(folder: &Path) -> MeasuredCosts {
        std::fs::read(Self::file(folder)).ok().and_then(|data| serde_json::from_slice(&data).ok()).unwrap_or_default()
    }

    fn save(&self, folder: &Path) {
        let _ = std::fs::create_dir_all(folder);
        let temporary = folder.join(format!("costs.json.{}", std::process::id()));
        if std::fs::write(&temporary, serde_json::to_vec_pretty(self).unwrap()).is_ok() {
            let _ = std::fs::rename(&temporary, Self::file(folder));
        }
    }

    /// Adds `new` to what's saved in `folder` (the main index and the code index each add their own kinds).
    pub fn merge_into(&self, folder: &Path) {
        let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut saved = Self::load(folder);
        for (name, tally) in &self.tallies {
            saved.add(name, tally.units, tally.seconds);
        }
        saved.save(folder);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measured_costs_take_over_once_there_are_enough() {
        let mut measured = MeasuredCosts::default();
        measured.add("image", 10.0, 1.0);
        assert_eq!(measured.costs().image, Costs::default().image);
        measured.add("image", 10.0, 1.0);
        assert!((measured.costs().image - 0.1).abs() < 1e-9);
    }
}
