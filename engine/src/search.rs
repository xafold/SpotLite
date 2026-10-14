//! Hybrid search: meaning (EmbeddingGemma's vectors) + words (FTS5). Meaning is the cosine similarity put on one
//! scale per query: every vector is scored anyway, so the distribution comes for free, and only segments that stand
//! out (`z_gate`) count as meaning matches. That also gives an honest "nothing found". Words rescue what a vector can't
//! hold exactly: codes, IDs and names.
use crate::code;
use crate::embed::DIMENSION;
use crate::error::Result;
use crate::model::{FileKind, IndexOptions, Modality, SegmentKind};
use crate::store::{Info, Store};
use crate::text;
use half::f16;
use half::slice::HalfFloatSliceExt;
use rusqlite::OptionalExtension;
use std::collections::{BTreeSet, HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct Hit {
    pub path: String,
    pub kind: FileKind,
    /// Fused rank score, for ordering only.
    pub score: f64,
    /// How far the best segment stands out for this query.
    pub z: Option<f64>,
    pub cosine: Option<f64>,
    /// 1-based rank among keyword matches.
    pub keyword_rank: Option<usize>,
    pub segment: SegmentKind,
    pub segment_id: i64,
    /// A PDF page, or seconds into sound or video, or a code stretch's first line.
    pub loc: Option<f64>,
    pub loc_end: Option<f64>,
    pub excerpt: Option<String>,
    pub modified: f64,
    pub size: i64,
    pub info: Info,
    /// Code: the line to open at, in the stretch.
    pub focus: Option<f64>,
}

impl Hit {
    /// As JSON: what `spotlite search --json` prints, plus what a shell's preview needs.
    pub fn json(&self) -> serde_json::Value {
        let mut row = serde_json::json!({
            "path": self.path, "kind": self.kind.name(), "segment": self.segment.name(), "segment_id": self.segment_id,
            "score": self.score, "modified": self.modified, "size": self.size, "info": self.info,
        });
        let fields = [("z", self.z), ("cosine", self.cosine), ("loc", self.loc), ("loc_end", self.loc_end),
                      ("focus", self.focus)];
        for (key, value) in fields {
            if let Some(value) = value {
                row[key] = value.into();
            }
        }
        if let Some(rank) = self.keyword_rank {
            row["keyword_rank"] = rank.into();
        }
        if let Some(excerpt) = &self.excerpt {
            row["excerpt"] = excerpt.clone().into();
        }
        row
    }
}

/// How much finding the query's words counts against meaning. Code's words count for less: they're everywhere in code.
pub const WORD_WEIGHT: f64 = 2.0;
pub const CODE_WORD_WEIGHT: f64 = 1.0;

pub struct Searcher {
    pub store: Store,
    /// rows × 768 float16, row-major.
    halves: Vec<f16>,
    row_file: Vec<i64>,
    row_segment: Vec<i64>,
    row_modality: Vec<Modality>,
    /// False for the vectors the later pass adds: scored, but they don't set the scale.
    row_sets_scale: Vec<bool>,
    file_kind: HashMap<i64, FileKind>,
    pub vector_source: Option<String>,
    /// False after `release_vectors()`: keyword search still works, and the next meaning search loads them again.
    pub vectors_loaded: bool,
}

impl Searcher {
    pub fn new(store: Store, load_vectors: bool) -> Result<Searcher> {
        let mut searcher = Searcher { store, halves: vec![], row_file: vec![], row_segment: vec![], row_modality: vec![],
                                      row_sets_scale: vec![], file_kind: HashMap::new(), vector_source: None,
                                      vectors_loaded: false };
        if load_vectors { searcher.reload()? } else { searcher.reload_files()? }
        Ok(searcher)
    }

    pub fn vector_count(&self) -> usize {
        self.row_file.len()
    }

    fn reload_files(&mut self) -> Result<()> {
        self.file_kind = self.store.file_kinds()?;
        self.vector_source = self.store.meta("vector_source")?;
        Ok(())
    }

    /// Reads the files and all vectors again (after indexing added some).
    pub fn reload(&mut self) -> Result<()> {
        self.release_vectors();
        self.reload_files()?;
        let rows: usize = self.store.db.query_row("SELECT COUNT(*) FROM segments WHERE vec IS NOT NULL", [],
                                                  |row| row.get::<_, i64>(0))? as usize;
        self.halves.reserve_exact(rows * DIMENSION);
        let steps = IndexOptions::default();
        let mut statement = self.store.db.prepare(
            "SELECT s.id, s.file, s.modality, s.vec, s.kind, s.loc, f.kind, CAST(json_extract(f.info, '$.pages') AS INTEGER) \
             FROM segments s JOIN files f ON f.id = s.file WHERE s.vec IS NOT NULL ORDER BY s.id")?;
        let mut cursor = statement.query([])?;
        while let Some(row) = cursor.next()? {
            let blob = row.get_ref(3)?.as_blob().map_err(|e| e.to_string())?;
            if blob.len() != DIMENSION * 2 || self.row_file.len() >= rows {
                continue;
            }
            self.halves.extend(blob.chunks_exact(2).map(|pair| f16::from_le_bytes([pair[0], pair[1]])));
            self.row_segment.push(row.get(0)?);
            self.row_file.push(row.get(1)?);
            self.row_modality.push(Modality::parse(&row.get::<_, String>(2)?));
            // Past the first step of a long PDF (its pages) or document (its chunks).
            let kind: String = row.get(4)?;
            let place = row.get::<_, Option<f64>>(5)?.unwrap_or(0.0) as usize;
            let later = if row.get::<_, String>(6)? == "pdf" {
                kind != "chunk" && row.get::<_, Option<i64>>(7)?.unwrap_or(0) as usize > steps.big_pdf_pages
                    && place > steps.pdf_step
            } else {
                kind == "chunk" && place > steps.doc_step
            };
            self.row_sets_scale.push(!later);
        }
        self.vectors_loaded = true;
        Ok(())
    }

    /// Frees the vectors' memory; keyword search keeps working.
    pub fn release_vectors(&mut self) {
        self.halves = vec![];
        self.row_file = vec![];
        self.row_segment = vec![];
        self.row_modality = vec![];
        self.row_sets_scale = vec![];
        self.vectors_loaded = false;
    }

    /// Cosine similarity of `query` with every stored vector (all are L2-normalized), on every core: each thread turns
    /// its rows into float32 a row at a time and takes the dot product.
    fn score(&self, query: &[f32]) -> Vec<f32> {
        let rows = self.row_file.len();
        let mut scores = vec![0f32; rows];
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(16);
        // ponytail: one thread under 8k rows; past that the threads pay for themselves (70k rows: ~15 → ~3 ms).
        let per_thread = if rows < 8192 { rows.max(1) } else { rows.div_ceil(threads) };
        std::thread::scope(|scope| {
            for (index, chunk) in scores.chunks_mut(per_thread).enumerate() {
                let first = index * per_thread;
                let halves = &self.halves[first * DIMENSION..(first + chunk.len()) * DIMENSION];
                scope.spawn(move || {
                    let mut row = [0f32; DIMENSION];
                    for (score, half_row) in chunk.iter_mut().zip(halves.chunks_exact(DIMENSION)) {
                        half_row.convert_to_f32_slice(&mut row);
                        *score = dot(&row, query);
                    }
                });
            }
        });
        scores
    }

    pub fn search(&mut self, query_text: &str, query_vector: Option<&[f32]>, kinds: Option<&BTreeSet<FileKind>>,
                  limit: usize, z_gate: f64, word_weight: Option<f64>, prefix_last: bool) -> Result<Vec<Hit>> {
        let is_code = self.store.is_code();
        let word_weight = word_weight.unwrap_or(if is_code { CODE_WORD_WEIGHT } else { WORD_WEIGHT });
        if query_vector.is_some() && !self.vectors_loaded {
            self.reload()?;
        }
        let allowed = |file: i64| kinds.is_none_or(|kinds| self.file_kind.get(&file).is_some_and(|k| kinds.contains(k)));

        // Meaning: score every vector on this query's scale, keep each file's best segment.
        let mut best: HashMap<i64, (f64, f64, usize)> = HashMap::new();   // z, cosine, row
        if let Some(query) = query_vector.filter(|q| q.len() == DIMENSION && !self.row_file.is_empty()) {
            let scores = self.score(query);
            let meaning = meaning(&scores, &self.row_modality, Some(&self.row_sets_scale));
            for row in 0..scores.len() {
                let file = self.row_file[row];
                if allowed(file) && best.get(&file).is_none_or(|b| meaning[row] > b.0) {
                    best.insert(file, (meaning[row], scores[row] as f64, row));
                }
            }
        }

        // Words: FTS5 with Porter stemming over file names, folder names, page and document text. A name from code is
        // matched as one phrase, as it's written.
        let terms = terms(query_text);
        let identifier = if is_code { code::identifier(query_text) } else { None };
        let mut patterns = match &identifier {
            Some(words) => vec![code::phrase(words)],
            None => patterns(&terms, prefix_last),
        };
        // A word that's in no file as typed matches as the start of longer words ("grok" finds "Grok4"). A word in the
        // index as typed stays whole, so "cat" doesn't bring in "category"; so does a word of two letters.
        if identifier.is_none() {
            for index in 0..terms.len() {
                if patterns[index].ends_with('*') {
                    continue;
                }
                // A word written as one that files write as two ("spiderman", "Spider-Man.mkv") matches either way.
                if let Some(two) = self.as_two_words(&terms[index])? {
                    patterns[index] = format!("({} OR {two})", patterns[index]);
                } else if terms[index].chars().count() >= 3 && !self.is_indexed(&patterns[index])? {
                    patterns[index].push('*');
                }
            }
        }
        let mut words: Vec<(i64, i64, Option<String>)> = vec![];   // file, segment, snippet
        let mut coverage: HashMap<i64, f64> = HashMap::new();      // share of the query's words found in the file
        let mut weighted: HashMap<i64, f64> = HashMap::new();      // the same, a rare word counting for more
        let mut exact: HashSet<i64> = HashSet::new();              // files with all the words, close together
        let mut named: HashSet<i64> = HashSet::new();              // the rest with all the words in name and folders
        let mut subject: Option<HashSet<i64>> = None;              // files with the query's subject word (`subject_of`)
        if !patterns.is_empty() {
            let mut seen = HashSet::new();
            // Ranked first, quoted after: snippet() is the costly part, and only the files kept need one (it was most
            // of a search's time, 6–28 ms of 8–31, when computed for every match before the ranking cut).
            let query = patterns.join(" OR ");
            let mut statement = self.store.db.prepare_cached(
                "SELECT s.file, s.id FROM fts JOIN segments s ON s.id = fts.rowid WHERE fts MATCH ? \
                 ORDER BY bm25(fts, 3.0, 1.0) LIMIT 1000")?;
            let mut rows = statement.query([&query])?;
            while let Some(row) = rows.next()? {
                let file: i64 = row.get(0)?;
                if words.len() >= 100 || !allowed(file) || !seen.insert(file) {
                    continue;
                }
                words.push((file, row.get(1)?, None));
            }
            drop(rows);
            let mut quote = self.store.db.prepare_cached(
                "SELECT snippet(fts, 1, '«', '»', '…', 12), original FROM fts WHERE fts MATCH ? AND rowid = ?")?;
            for (_, segment, snippet) in words.iter_mut() {
                let (marked, original): (Option<String>, Option<String>) =
                    quote.query_row(rusqlite::params![&query, *segment], |row| Ok((row.get(0)?, row.get(1)?)))?;
                // Text that normalizing changed is quoted as it's written.
                *snippet = original.and_then(|o| text::snippet(&o, &terms, 12)).or(marked);
            }
            if !words.is_empty() {
                let files = words.iter().map(|w| w.0.to_string()).collect::<Vec<_>>().join(",");
                let mut statement = self.store.db.prepare(&format!(
                    "SELECT DISTINCT s.file FROM fts JOIN segments s ON s.id = fts.rowid WHERE fts MATCH ? AND s.file IN ({files})"))?;
                let total: f64 = self.store.db.prepare_cached("SELECT count(*) FROM files")?.query_row([], |row| row.get(0))?;
                let found: Vec<i64> = patterns.iter().map(|pattern| self.files_matching(pattern)).collect::<Result<_>>()?;
                let rarity: Vec<f64> = found.iter().map(|&found| (1.0 + total / (1.0 + found as f64)).ln()).collect();
                if let Some(index) = subject_of(&found, total as usize).filter(|_| !is_code) {
                    let mut files = HashSet::new();
                    let mut statement = self.store.db.prepare_cached(
                        "SELECT DISTINCT s.file FROM fts JOIN segments s ON s.id = fts.rowid WHERE fts MATCH ?")?;
                    for file in statement.query_map([&patterns[index]], |row| row.get::<_, i64>(0))? {
                        files.insert(file?);
                    }
                    subject = Some(files);
                }
                let rarity_sum: f64 = rarity.iter().sum::<f64>().max(f64::EPSILON);
                for (pattern, rarity) in patterns.iter().zip(&rarity) {
                    let found = statement.query_map([pattern], |row| row.get::<_, i64>(0))?;
                    for file in found {
                        let file = file?;
                        *coverage.entry(file).or_default() += 1.0 / patterns.len() as f64;
                        *weighted.entry(file).or_default() += rarity / rarity_sum;
                    }
                }
                // All the words, close together (stopwords dropped from the query may sit between them): one word
                // alone, or NEAR(…) for several (which takes phrases only: with a word that's two ways, in one part).
                let together = if patterns.len() == 1 { patterns[0].clone() }
                    else if patterns.iter().any(|pattern| pattern.starts_with('(')) { patterns.join(" AND ") }
                    else { format!("NEAR({}, {})", patterns.join(" "), 2 * patterns.len()) };
                let mut statement = self.store.db.prepare_cached(
                    "SELECT DISTINCT s.file FROM fts JOIN segments s ON s.id = fts.rowid WHERE fts MATCH ? LIMIT 5000")?;
                for file in statement.query_map([&together], |row| row.get::<_, i64>(0))? {
                    let file = file?;
                    if allowed(file) {
                        exact.insert(file);
                    }
                }
                // NEAR stays in one column, so "readme spotlite" misses spotlite/README.md: its name has one word, its
                // folders the other. A file's name and folders together having all the words count as together too.
                if patterns.len() > 1 {
                    let mut statement = self.store.db.prepare_cached(
                        "SELECT DISTINCT s.file FROM fts JOIN segments s ON s.id = fts.rowid WHERE fts MATCH ? \
                         AND s.kind = 'name' LIMIT 5000")?;
                    for file in statement.query_map([patterns.join(" AND ")], |row| row.get::<_, i64>(0))? {
                        let file = file?;
                        if allowed(file) && !exact.contains(&file) {
                            named.insert(file);
                        }
                    }
                }
            }
        }

        // A file qualifies by standing out in meaning, or by containing at least half of the query's words. Score:
        // meaning z + a bonus that grows with word coverage (rare words weigh more), + one for the words all together
        // when few files have them (`exact_bonus`), + 1 for the kind the query asks for.
        let lookup = is_lookup(&terms) || identifier.is_some();
        let (meaning_weight, words_weight) = if lookup { (0.5, word_weight * 2.0) } else { (1.0, word_weight) };
        let hinted = kind_hints(query_text);
        let together_bonus = exact_bonus(exact.len(), self.file_kind.len());
        let named_bonus = exact_bonus(named.len(), self.file_kind.len());
        let mut candidates: HashSet<i64> = best.iter().filter(|(_, b)| b.0 >= z_gate).map(|(f, _)| *f).collect();
        candidates.extend(coverage.iter().filter(|(_, share)| **share >= 0.5).map(|(f, _)| *f));
        candidates.extend(exact.iter().chain(&named).filter(|file| coverage.contains_key(file)));
        // A page, passage or line of code that means something like the query but has none of its words is often
        // about something else ("spiderman taken into hospital": pages about hospitals): its meaning counts in
        // proportion to the share of the words it has, rare words weighing more, down to half. A picture, a frame or
        // a sound has no words to have, so its meaning counts whole. Only when some file has all the words: with none
        // (a query in another language, or other words for the same thing) the words say nothing about the meaning,
        // and halving text let pictures and sounds outrank the page that means it (DigUp's multilingual evals).
        let worded = coverage.values().any(|share| *share > 0.999);
        let mut scored: Vec<(i64, f64)> = candidates.into_iter().map(|file| {
            let share = weighted.get(&file).copied().unwrap_or(0.0);
            let meaning = best.get(&file).map_or(0.0, |b| {
                let text = self.row_modality[b.2] == Modality::Text;
                b.0.max(0.0) * if worded && text { 0.5 + 0.5 * share } else { 1.0 }
            });
            let mut score = meaning_weight * meaning + words_weight * share.powf(1.5);
            // Text without the query's subject ("spiderman") is about something else: half.
            let text = best.get(&file).is_none_or(|b| self.row_modality[b.2] == Modality::Text);
            if text && subject.as_ref().is_some_and(|files| !files.contains(&file)) {
                score *= 0.5;
            }
            if exact.contains(&file) {
                score += together_bonus;
            } else if named.contains(&file) {
                score += named_bonus;
            }
            if self.file_kind.get(&file).is_some_and(|kind| hinted.contains(kind)) {
                score += 1.0;
            }
            (file, score)
        }).collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));

        let mut word_rank: HashMap<i64, (usize, i64, Option<String>)> = HashMap::new();
        for (rank, (file, segment, snippet)) in words.into_iter().enumerate() {
            word_rank.entry(file).or_insert((rank + 1, segment, snippet));
        }
        let mut hits = vec![];
        for (file, score) in scored.into_iter().take(limit) {
            let rank = hits.len();
            let semantic = best.get(&file).copied();
            let keyword = word_rank.get(&file);
            // Point at the best meaning segment if it stands out; else, or when all the words are in the file's text
            // (not just its name), at the segment the words matched.
            let words_in_text = exact.contains(&file) && match keyword {
                Some(k) => self.store.db.prepare_cached("SELECT kind FROM segments WHERE id = ?")?
                    .query_row([k.1], |row| row.get::<_, String>(0)).optional()?.is_some_and(|kind| kind != "name"),
                None => false,
            };
            let use_meaning = semantic.is_some_and(|s| s.0 >= z_gate || keyword.is_none()) && !words_in_text;
            let Some(mut segment_id) = (if use_meaning { semantic.map(|s| self.row_segment[s.2]) } else {
                keyword.map(|k| k.1)
            }) else { continue };
            let mut focus = None;
            if is_code {
                // Where a name is defined is looked for in the first 10 results only.
                let words_here = keyword.is_some_and(|k| k.1 == segment_id) || identifier.is_some();
                (segment_id, focus) = self.code_place(segment_id, file, if rank < 10 { identifier.as_ref() } else { None },
                                                      query_text, &terms, words_here)?;
            }
            let (path, kind, modified, size, info): (String, String, f64, i64, Option<String>) = self.store.db.prepare_cached(
                "SELECT path, kind, mtime, size, info FROM files WHERE id = ?")?.query_row([file],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))?;
            let (segment, loc, loc_end, stored_excerpt): (String, Option<f64>, Option<f64>, Option<String>) =
                self.store.db.prepare_cached("SELECT kind, loc, loc_end, excerpt FROM segments WHERE id = ?")?.query_row([segment_id],
                                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?;
            // Quote the matched words only when they come from the segment pointed at, so a PDF hit on page 5 never
            // shows page 2's words. A segment with no place and no text of its own (a screenshot's picture) can borrow
            // them from its file's other text, which describes the same picture.
            let mut snippet = keyword.filter(|k| k.1 == segment_id).and_then(|k| k.2.clone())
                .filter(|s| s.contains('«')).map(|s| text::excerpt(&s, 400));
            if snippet.is_none() && stored_excerpt.is_none() && loc.is_none() {
                if let Some((_, matched, Some(matched_snippet))) = keyword.filter(|k| k.2.as_ref().is_some_and(|s| s.contains('«'))) {
                    let matched_kind: Option<String> = self.store.db.prepare_cached(
                        "SELECT kind FROM segments WHERE id = ?")?.query_row([matched], |row| row.get(0)).optional()?;
                    if matched_kind.as_deref() != Some("name") {
                        snippet = Some(text::excerpt(matched_snippet, 400));
                    }
                }
            }
            hits.push(Hit {
                path, kind: FileKind::parse(&kind).unwrap_or(FileKind::Doc), score, z: semantic.map(|s| s.0),
                cosine: semantic.map(|s| s.1), keyword_rank: keyword.map(|k| k.0), segment: SegmentKind::parse(&segment),
                segment_id, loc, loc_end, excerpt: snippet.or(stored_excerpt), modified, size,
                info: info.and_then(|i| serde_json::from_str(&i).ok()).unwrap_or_default(), focus,
            });
        }
        Ok(hits)
    }

    /// Code: the stretch to point at, and the line in it to open at. A name looked up points at the stretch that
    /// defines it, when one does. Otherwise the line is the first with the query's words, when they matched here.
    fn code_place(&self, segment: i64, file: i64, identifier: Option<&Vec<String>>, query: &str, terms: &[String],
                  words_here: bool) -> Result<(i64, Option<f64>)> {
        let name = query.trim().replace("()", "");
        if let Some(identifier) = identifier {
            let mut best: Option<(i64, usize, u8)> = None;
            let mut statement = self.store.db.prepare(
                "SELECT s.id, s.loc, coalesce(fts.original, fts.body) FROM fts JOIN segments s ON s.id = fts.rowid \
                 WHERE fts MATCH ? AND s.file = ? ORDER BY s.loc LIMIT 300")?;
            let mut rows = statement.query(rusqlite::params![code::phrase(identifier), file])?;
            while let Some(row) = rows.next()? {
                if best.is_some_and(|b| b.2 >= 2) {
                    break;
                }
                let Some(text): Option<String> = row.get(2)? else { continue };
                let first = row.get::<_, Option<f64>>(1)?.unwrap_or(1.0) as usize;
                if let Some((line, strength)) = code::definition(&name, &text, first) {
                    if strength > best.map_or(0, |b| b.2) {
                        best = Some((row.get(0)?, line, strength));
                    }
                }
            }
            if let Some((segment, line, _)) = best {
                return Ok((segment, Some(line as f64)));
            }
        }
        if !words_here && identifier.is_none() {
            return Ok((segment, None));
        }
        let Some(text) = self.text_of_segment(segment)? else { return Ok((segment, None)) };
        let first: Option<f64> = self.store.db.prepare_cached("SELECT loc FROM segments WHERE id = ?")?.query_row([segment], |row| row.get(0))?;
        let words = if identifier.is_none() { terms.to_vec() } else { vec![name.to_lowercase()] };
        Ok((segment, code::first_line(&words, &text, first.unwrap_or(1.0) as usize).map(|l| l as f64)))
    }

    /// The text of a page, a document's passage or a code stretch, as it's written: the preview shows it.
    pub fn text_of_segment(&self, id: i64) -> Result<Option<String>> {
        Ok(self.store.db.prepare_cached("SELECT coalesce(original, body) FROM fts WHERE rowid = ?")?.query_row([id], |row| row.get(0))
            .optional()?)
    }

    /// Whether keyword search finds `pattern` in any file.
    /// The word as two that are in the index next to each other ("spider man" for "spiderman"), as an FTS5 phrase.
    fn as_two_words(&self, word: &str) -> Result<Option<String>> {
        let letters: Vec<char> = word.chars().collect();
        if letters.len() < 6 || !letters.iter().all(|c| c.is_alphabetic()) {
            return Ok(None);
        }
        for cut in 3..=letters.len() - 3 {
            let phrase = format!("\"{} {}\"", letters[..cut].iter().collect::<String>(), letters[cut..].iter().collect::<String>());
            if self.is_indexed(&phrase)? {
                return Ok(Some(phrase));
            }
        }
        Ok(None)
    }

    /// How many files have words matching `pattern`.
    fn files_matching(&self, pattern: &str) -> Result<i64> {
        Ok(self.store.db.prepare_cached(
            "SELECT count(DISTINCT s.file) FROM fts JOIN segments s ON s.id = fts.rowid WHERE fts MATCH ?")?
            .query_row([pattern], |row| row.get(0))?)
    }

    fn is_indexed(&self, pattern: &str) -> Result<bool> {
        Ok(self.store.db.prepare_cached("SELECT 1 FROM fts WHERE fts MATCH ? LIMIT 1")?.query_row([pattern], |_| Ok(())).optional()?
            .is_some())
    }
}

fn dot(a: &[f32; DIMENSION], b: &[f32]) -> f32 {
    // Eight running sums, so the compiler keeps them in one vector register.
    let mut sums = [0f32; 8];
    for (a, b) in a.chunks_exact(8).zip(b.chunks_exact(8)) {
        for lane in 0..8 {
            sums[lane] += a[lane] * b[lane];
        }
    }
    sums.iter().sum()
}

/// This query's meaning scores: each vector's cosine similarity minus the average of its modality, in standard
/// deviations of all of them together. The scale comes from the vectors in `sets_scale` (the later pages of long
/// files are scored on it but don't move it).
pub fn meaning(scores: &[f32], modality: &[Modality], sets_scale: Option<&[bool]>) -> Vec<f64> {
    let counts = |row: usize| sets_scale.is_none_or(|s| s[row]);
    let means = modality_means(scores, modality, sets_scale);
    let centered: Vec<f64> = scores.iter().zip(modality).map(|(&s, m)| s as f64 - means[m]).collect();
    let (mut count, mut sum) = (0f64, 0f64);
    for (row, value) in centered.iter().enumerate() {
        if counts(row) {
            count += 1.0;
            sum += value;
        }
    }
    let mean = sum / count.max(1.0);
    let squares: f64 = centered.iter().enumerate().filter(|(row, _)| counts(*row)).map(|(_, v)| (v - mean).powi(2)).sum();
    let deviation = (squares / count.max(1.0)).sqrt().max(0.02);
    centered.iter().map(|v| (v - mean) / deviation).collect()
}

/// The average score of each modality for this query. Small groups (< 20 vectors) take the average of all.
fn modality_means(scores: &[f32], modality: &[Modality], sets_scale: Option<&[bool]>) -> HashMap<Modality, f64> {
    let (mut sum, mut count): (HashMap<Modality, f64>, HashMap<Modality, usize>) = Default::default();
    for row in 0..scores.len() {
        if sets_scale.is_none_or(|s| s[row]) {
            *sum.entry(modality[row]).or_default() += scores[row] as f64;
            *count.entry(modality[row]).or_default() += 1;
        }
    }
    let all = sum.values().sum::<f64>() / count.values().sum::<usize>().max(1) as f64;
    Modality::ALL.iter().map(|m| {
        let mean = match count.get(m) {
            Some(&n) if n >= 20 => sum[m] / n as f64,
            _ => all,
        };
        (*m, mean)
    }).collect()
}

/// Codes and IDs ("K7Q2LM") and long numbers mean nothing to an embedding but everything to keyword search: a query
/// made of them weighs words double and meaning half.
/// What a file with all of the query's words close together adds to its score: the words typed are what's wanted
/// (a project's name), so it's enough to rank above files that only mean something like them (z ≈ 4–5). It shrinks
/// as more files have the words together: a common phrase ("machine learning" in a tenth of them) says less.
fn exact_bonus(files_with_words: usize, files: usize) -> f64 {
    const BONUS: f64 = 4.0;
    let few = (files as f64 * 0.03).max(15.0);
    if files_with_words == 0 { 0.0 } else { BONUS * (few / files_with_words as f64).min(1.0) }
}

/// The query's subject: of several words, the one in far fewer files than the rest (a third of the next, and few of
/// all: "spiderman" in 3 files of 579, "hospital" in 16, "taken" in 32), usually a name. None when no word stands out
/// (two words in 14 and 12 files), when there's one word, or when the word is in no file.
fn subject_of(files_with: &[i64], files: usize) -> Option<usize> {
    let few = (files as f64 * 0.03).max(15.0);
    let mut order: Vec<usize> = (0..files_with.len()).filter(|&i| files_with[i] > 0).collect();
    order.sort_by_key(|&i| files_with[i]);
    let (rarest, next) = (*order.first()?, *order.get(1)?);
    (files_with[rarest] as f64 <= few && files_with[rarest] * 3 <= files_with[next]).then_some(rarest)
}

fn is_lookup(terms: &[String]) -> bool {
    !terms.is_empty() && terms.iter().all(|t| {
        t.chars().any(|c| c.is_numeric()) && (t.chars().any(char::is_alphabetic) || t.chars().count() >= 4)
    })
}

/// How many of `hits` (best first) are strong: scoring at least `ratio` × the top hit. The rest trail far behind, and
/// the panel folds them under "weaker matches".
pub fn strong_count(hits: &[Hit], ratio: f64) -> usize {
    match hits.first() {
        Some(top) if top.score > 0.0 => hits.iter().take_while(|h| h.score >= top.score * ratio).count(),
        _ => hits.len(),
    }
}

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "of", "in", "on", "at", "to", "for", "with", "and", "or", "my", "me", "that", "this",
    "these", "those", "from", "about", "where", "what", "which", "who", "when", "is", "was", "are", "were", "be",
    "it", "its", "some", "any", "into", "onto", "by", "as", "his", "her", "their", "our", "your", "has", "have", "had",
    "find", "show", "file", "files", "photo", "photos", "picture", "image", "images",
    "screenshot", "screenshots", "video", "videos", "pdf", "document", "documents", "recording",
    "এবং", "ও", "এই", "সেই", "একটি", "আমার", "থেকে", "জন্য", "সাথে", "মধ্যে", "কোথায়", "কোন", "কী", "কি", "যে",
    "ছবি", "স্ক্রিনশট", "ভিডিও", "ফাইল", "পিডিএফ",
    "في", "من", "على", "الى", "عن", "مع", "هذا", "هذه", "ذلك", "التي", "الذي", "اين", "ما",
    "صورة", "صور", "لقطة", "شاشة", "فيديو", "ملف", "مستند",
];

/// The content words of a query, normalized as keyword search keeps its text. Kind words ("screenshot", "video") and
/// stopwords are dropped: they describe what to look for, not what's in it.
pub fn terms(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    text::normalized(query).to_lowercase().split(|c: char| !text::is_word_char(c))
        .filter(|w| w.chars().count() >= 2 && !STOPWORDS.contains(w) && seen.insert(w.to_string()))
        .map(str::to_string).collect()
}

/// FTS5 patterns for the query's words: quoted, and as prefixes for the last word while it's being typed and for
/// words of languages that attach endings.
fn patterns(terms: &[String], prefix_last: bool) -> Vec<String> {
    terms.iter().enumerate().map(|(index, term)| {
        let prefix = (prefix_last && index == terms.len() - 1) || text::inflects(term);
        format!("\"{term}\"{}", if prefix { "*" } else { "" })
    }).collect()
}

const KIND_WORDS: &[(&str, FileKind)] = &[
    ("screenshot", FileKind::Screenshot), ("screenshots", FileKind::Screenshot), ("photo", FileKind::Image),
    ("photos", FileKind::Image), ("picture", FileKind::Image), ("pictures", FileKind::Image), ("image", FileKind::Image),
    ("images", FileKind::Image), ("pdf", FileKind::Pdf), ("pdfs", FileKind::Pdf), ("paper", FileKind::Pdf),
    ("papers", FileKind::Pdf), ("document", FileKind::Doc), ("documents", FileKind::Doc), ("note", FileKind::Doc),
    ("notes", FileKind::Doc), ("video", FileKind::Video), ("videos", FileKind::Video), ("clip", FileKind::Video),
    ("movie", FileKind::Video), ("audio", FileKind::Audio), ("song", FileKind::Audio), ("podcast", FileKind::Audio),
    ("voice", FileKind::Audio), ("sound", FileKind::Audio),
    ("স্ক্রিনশট", FileKind::Screenshot), ("ছবি", FileKind::Image), ("পিডিএফ", FileKind::Pdf), ("নোট", FileKind::Doc),
    ("ভিডিও", FileKind::Video), ("অডিও", FileKind::Audio), ("গান", FileKind::Audio), ("রেকর্ডিং", FileKind::Audio),
    ("لقطة", FileKind::Screenshot), ("صورة", FileKind::Image), ("صور", FileKind::Image), ("مستند", FileKind::Doc),
    ("ملاحظات", FileKind::Doc), ("فيديو", FileKind::Video), ("مقطع", FileKind::Video), ("صوت", FileKind::Audio),
    ("تسجيل", FileKind::Audio), ("اغنية", FileKind::Audio),
];

/// Kinds the query asks for ("…in a video", "screenshot of…"): a soft boost, not a filter. Bengali attaches case
/// endings, so a Bengali kind word also counts at the start of a longer word.
fn kind_hints(query: &str) -> HashSet<FileKind> {
    text::normalized(query).to_lowercase().split(|c: char| !text::is_word_char(c)).filter(|w| !w.is_empty())
        .filter_map(|word| {
            KIND_WORDS.iter().find(|(key, _)| *key == word)
                .or_else(|| KIND_WORDS.iter().find(|(key, _)| {
                    text::inflects(key) && key.chars().count() >= 3 && word.starts_with(key)
                }))
                .map(|(_, kind)| *kind)
        }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_queries() {
        assert_eq!(terms("a screenshot of the payment declined error"), ["payment", "declined", "error"]);
        assert!(is_lookup(&terms("K7Q2LM")));
        assert!(!is_lookup(&terms("dog on the beach")));
        assert!(kind_hints("zebra in a video").contains(&FileKind::Video));
        assert_eq!(patterns(&["cat".into()], true), ["\"cat\"*"]);
        // Words together in a few files of 579: the whole bonus; in 57: under a third of it.
        assert_eq!(exact_bonus(8, 579), 4.0);
        assert!((exact_bonus(57, 579) - 4.0 * 17.37 / 57.0).abs() < 0.01);
        assert_eq!(exact_bonus(0, 579), 0.0);
        assert_eq!(subject_of(&[3, 32, 16], 579), Some(0));
        assert_eq!(subject_of(&[14, 12], 579), None);
        assert_eq!(subject_of(&[3], 579), None);
        assert_eq!(subject_of(&[0, 40, 50], 579), None);
    }

    #[test]
    fn meaning_stands_out() {
        let mut scores = vec![0.30f32; 40];
        scores[7] = 0.6;
        let z = meaning(&scores, &[Modality::Text; 40], None);
        assert!(z[7] > 5.0 && z[0] < 0.5);
    }
}
