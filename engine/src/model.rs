//! What the index is made of: kinds of files and of their pieces, indexing settings, and the model's prompts.
use std::collections::BTreeSet;

/// What a file is, for indexing and for grouping results.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum FileKind {
    Screenshot,
    Image,
    Pdf,
    Doc,
    Audio,
    Video,
    /// Source code, a repo's docs and notebooks: only in the code index.
    Code,
}

impl FileKind {
    pub const ALL: [FileKind; 7] = [Self::Screenshot, Self::Image, Self::Pdf, Self::Doc, Self::Audio, Self::Video,
                                    Self::Code];

    pub fn name(self) -> &'static str {
        match self {
            Self::Screenshot => "screenshot",
            Self::Image => "image",
            Self::Pdf => "pdf",
            Self::Doc => "doc",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Code => "code",
        }
    }

    pub fn parse(name: &str) -> Option<FileKind> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }
}

/// The order of the first indexing pass: what people look for most comes first.
pub const INDEXING_ORDER_SQL: &str = "CASE kind WHEN 'screenshot' THEN 0 WHEN 'image' THEN 1 WHEN 'pdf' THEN 2 \
    WHEN 'doc' THEN 3 WHEN 'audio' THEN 4 WHEN 'video' THEN 5 ELSE 6 END";

/// One searchable piece of a file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SegmentKind {
    Name,      // file name + folder names (keyword search only)
    Image,     // the image itself
    Ocr,       // text read from a screenshot
    Page,      // a PDF page's text
    PageImage, // a PDF page rendered as an image (scans, slides)
    Chunk,     // a piece of a document's text
    Audio,     // a window of audio (a file or a video's soundtrack)
    Frame,     // a video keyframe
    Lines,     // a stretch of a code file: loc to loc_end are its first and last lines
    Cells,     // a stretch of a notebook: loc to loc_end are its first and last cells, from 1
}

impl SegmentKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Image => "image",
            Self::Ocr => "ocr",
            Self::Page => "page",
            Self::PageImage => "page_image",
            Self::Chunk => "chunk",
            Self::Audio => "audio",
            Self::Frame => "frame",
            Self::Lines => "lines",
            Self::Cells => "cells",
        }
    }

    pub fn parse(name: &str) -> SegmentKind {
        [Self::Name, Self::Image, Self::Ocr, Self::Page, Self::PageImage, Self::Chunk, Self::Audio, Self::Frame,
         Self::Lines, Self::Cells].into_iter().find(|kind| kind.name() == name).unwrap_or(Self::Name)
    }
}

/// What produced a segment's vector. Scores are put on one scale per modality: text vectors score higher than image
/// or audio vectors for the same topic.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Modality {
    None,
    Text,
    Image,
    Audio,
}

impl Modality {
    pub const ALL: [Modality; 4] = [Self::None, Self::Text, Self::Image, Self::Audio];

    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Text => "text",
            Self::Image => "image",
            Self::Audio => "audio",
        }
    }

    pub fn parse(name: &str) -> Modality {
        Self::ALL.into_iter().find(|m| m.name() == name).unwrap_or(Self::None)
    }
}

/// Indexing settings. Anything that changes vectors is part of `fingerprint`; an index never mixes settings.
#[derive(Clone, Debug)]
pub struct IndexOptions {
    /// This index is the code index: the code folders' source files, their repos' docs and notebooks.
    pub code: bool,
    /// Characters a stretch of code holds at most (whole lines).
    pub code_stretch: usize,
    /// A bigger source file was made by a program: left out.
    pub max_code_bytes: u64,
    /// A notebook's file holds its outputs too: bigger than this, it isn't read.
    pub max_notebook_bytes: u64,
    pub include_code: bool,
    pub include_datasets: bool,
    pub min_image_side: usize,
    /// Long files are read a step at a time: the first step in the first pass, the rest after everything else.
    pub big_pdf_pages: usize,
    pub pdf_step: usize,
    pub doc_step: usize,
    pub max_pages: usize,
    pub max_chunks: usize,
    /// Fewer characters than this: embed the rendered page instead.
    pub min_page_text: usize,
    /// Vision soft tokens: 70 / 140 / 280 / 560 / 1120.
    pub photo_budget: usize,
    pub screenshot_budget: usize,
    pub page_budget: usize,
    pub frame_budget: usize,
    /// Seconds between sampled video frames (near-duplicates are dropped).
    pub frame_interval: f64,
    /// The model silently ignores audio past 30 s of one input.
    pub audio_window: f64,
    pub audio_hop: f64,
    pub excluded_folders: Vec<String>,
    pub excluded_extensions: BTreeSet<String>,
}

impl Default for IndexOptions {
    fn default() -> Self {
        IndexOptions {
            code: false,
            code_stretch: 1800,
            max_code_bytes: 1 << 20,
            max_notebook_bytes: 20 << 20,
            include_code: false,
            include_datasets: false,
            min_image_side: 256,
            big_pdf_pages: 50,
            pdf_step: 30,
            doc_step: 40,
            max_pages: 5000,
            max_chunks: 5000,
            min_page_text: 200,
            photo_budget: 140,
            screenshot_budget: 280,
            page_budget: 280,
            frame_budget: 70,
            frame_interval: 3.0,
            audio_window: 30.0,
            audio_hop: 25.0,
            excluded_folders: vec![],
            excluded_extensions: BTreeSet::new(),
        }
    }
}

impl IndexOptions {
    /// File extensions as people write them ("HEIC, .txt *.md") → {"heic", "txt", "md"}.
    pub fn extensions(text: &str) -> BTreeSet<String> {
        text.split(|c: char| c == ',' || c.is_whitespace())
            .map(|word| word.trim_start_matches(['*', '.']).to_lowercase())
            .filter(|word| !word.is_empty())
            .collect()
    }

    pub fn fingerprint(&self) -> String {
        if self.code {
            return format!("code1 lines{}", self.code_stretch);
        }
        format!("photo{} shot{} page{} frame{}/{}s audio{}/{}s", self.photo_budget, self.screenshot_budget,
                self.page_budget, self.frame_budget, swift_double(self.frame_interval),
                swift_double(self.audio_window), swift_double(self.audio_hop))
    }
}

/// A Double as Swift prints it ("3.0", "25.0").
fn swift_double(value: f64) -> String {
    if value.fract() == 0.0 { format!("{value:.1}") } else { value.to_string() }
}

/// EmbeddingGemma 2's task prompts (model card). Media inputs take no prompt.
pub fn query_prompt(text: &str) -> String {
    format!("task: search result | query: {text}")
}

/// A search of the code index: the model card's prompt for finding code by what it does.
pub fn code_query_prompt(text: &str) -> String {
    format!("task: code retrieval | query: {text}")
}

pub fn document_prompt(title: Option<&str>, text: &str) -> String {
    let title = title.filter(|t| !t.is_empty()).unwrap_or("none");
    format!("title: {title} | text: {text}")
}

/// "code: retry with backoff": a search of the code index. What's typed after "code:" ("" for "code:" alone), or None
/// for a search that isn't one of code.
pub fn code_query(query: &str) -> Option<String> {
    let trimmed = query.trim_start();
    let prefix: String = trimmed.chars().take(5).collect();
    (prefix.to_lowercase() == "code:").then(|| trimmed[prefix.len()..].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_extensions_and_code_queries() {
        assert_eq!(IndexOptions::extensions("HEIC, .txt *.md").into_iter().collect::<Vec<_>>(), ["heic", "md", "txt"]);
        assert_eq!(code_query("  Code: retry  ").as_deref(), Some("retry"));
        assert_eq!(code_query("codex"), None);
        assert_eq!(IndexOptions::default().fingerprint(), "photo140 shot280 page280 frame70/3.0s audio30.0/25.0s");
    }
}
