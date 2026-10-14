//! Text the way keyword search keeps it and matches it, so that a word is found however it was typed or extracted.
use unicode_general_category::{GeneralCategory as G, get_general_category};
use unicode_normalization::UnicodeNormalization;

/// A letter (L*) or a mark (M*): marks are part of words (Bengali vowel signs, Arabic harakat).
pub fn is_letter(c: char) -> bool {
    matches!(get_general_category(c),
        G::UppercaseLetter | G::LowercaseLetter | G::TitlecaseLetter | G::ModifierLetter | G::OtherLetter
        | G::NonspacingMark | G::SpacingMark | G::EnclosingMark)
}

/// Letters, marks and numbers (L*, M*, N*): what a word is made of.
pub fn is_word_char(c: char) -> bool {
    is_letter(c) || matches!(get_general_category(c), G::DecimalNumber | G::LetterNumber | G::OtherNumber)
}

/// NFKC (Arabic presentation forms and ligatures become plain letters, full-width becomes ASCII, Bengali's two
/// spellings of "ো" become one), invisible formatting characters dropped (zero-width joiners, direction marks), and
/// Arabic the way people type it: no tatweel, no short-vowel marks, one alef.
pub fn normalized(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.nfkc() {
        match c as u32 {
            0x200B => out.push(' '),   // zero-width space: a word break (Thai, Khmer)
            0x00AD | 0x061C | 0x200C..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x2069 | 0xFEFF => {}
            0x0640 | 0x064B..=0x065F | 0x0670 => {}   // tatweel; harakat and the dagger alef
            0x0622 | 0x0623 | 0x0625 | 0x0671 => out.push('\u{0627}'),   // alef with hamza or madda, wasla → alef
            _ => out.push(c),
        }
    }
    out
}

const INDIC: std::ops::RangeInclusive<u32> = 0x0900..=0x0DFF;
const ARABIC: std::ops::RangeInclusive<u32> = 0x0600..=0x06FF;

/// True for words of scripts that attach endings to words (Bengali, Hindi, Arabic): keyword search matches them as
/// prefixes, so "কুমির" finds "কুমিরও" and "مدرس" finds "مدرسة".
pub fn inflects(word: &str) -> bool {
    word.chars().any(|c| INDIC.contains(&(c as u32)) || ARABIC.contains(&(c as u32)))
}

/// Whether text says something in words: at least a fifth of what isn't space is letters. A data dump or a table of
/// numbers isn't, and gets keyword search only: a meaning vector of digits matches nothing anyone would describe.
pub fn has_words(text: &str) -> bool {
    let (mut letters, mut others) = (0usize, 0usize);
    for c in text.chars().filter(|c| !c.is_whitespace()) {
        if is_letter(c) { letters += 1 } else { others += 1 }
    }
    letters > 0 && letters * 4 >= others
}

/// One character as matching compares it: normalized, lowercased, Latin accents dropped.
fn fold(c: char) -> String {
    let lower = normalized(&c.to_string()).to_lowercase();
    lower.nfd().filter(|c| !(0x0300..=0x036F).contains(&(*c as u32))).nfc().collect()
}

/// Where `word` is in `text` as keyword search would find it, as character ranges of the text as it's written: whole
/// words, and for a word of a language that attaches endings, the whole word it starts. For marking matches.
pub fn matches(word: &str, text: &[char]) -> Vec<(usize, usize)> {
    let needle: String = normalized(word).to_lowercase().chars().map(fold).collect();
    if needle.is_empty() {
        return vec![];
    }
    // The text folded a character at a time, remembering which character each byte came from.
    let mut folded = String::with_capacity(text.len());
    let mut origin: Vec<usize> = Vec::with_capacity(text.len());
    for (index, &c) in text.iter().enumerate() {
        let piece = fold(c);
        origin.extend(std::iter::repeat_n(index, piece.len()));
        folded.push_str(&piece);
    }
    let prefix = inflects(word);
    let mut found: Vec<(usize, usize)> = vec![];
    let mut from = 0;
    while found.len() < 500 {
        let Some(at) = folded[from..].find(&needle) else { break };
        let (hit_start, hit_end) = (from + at, from + at + needle.len());
        from = hit_end;
        let (start, mut end) = (origin[hit_start], origin[hit_end - 1] + 1);
        if start > 0 && is_word_char(text[start - 1]) {
            continue;
        }
        if end < text.len() && is_word_char(text[end]) {
            if !prefix {
                continue;
            }
            while end < text.len() && is_word_char(text[end]) {
                end += 1;
            }
        }
        if found.last().is_some_and(|&(_, last_end)| last_end > start) {
            continue;
        }
        found.push((start, end));
    }
    found
}

/// One line from `text` around the first of `terms` it has, the terms marked « » as SQLite's snippet() marks them:
/// the excerpt of a keyword match, from the text as it's written. None when none of the terms is in it.
pub fn snippet(text: &str, terms: &[String], words: usize) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut marks: Vec<(usize, usize)> = terms.iter().flat_map(|term| matches(term, &chars)).collect();
    marks.sort_by_key(|mark| mark.0);
    let first = *marks.first()?;
    // About `words` words, starting a few before the first match.
    let (mut start, mut before) = (first.0, 0);
    while start > 0 && before < words / 3 {
        start -= 1;
        if chars[start].is_whitespace() && start < first.0 && !chars[start + 1].is_whitespace() {
            before += 1;
        }
    }
    if start > 0 || chars[start].is_whitespace() {
        start += 1;
    }
    let (mut end, mut after) = (first.1, 0);
    while end < chars.len() && after < words - before {
        if chars[end].is_whitespace() {
            after += 1;
        }
        end += 1;
    }
    let mut out = String::from(if start > 0 { "…" } else { "" });
    let mut cursor = start;
    for &(mark_start, mark_end) in &marks {
        if mark_start >= start && mark_end <= end && mark_start >= cursor {
            out.extend(&chars[cursor..mark_start]);
            out.push('«');
            out.extend(&chars[mark_start..mark_end]);
            out.push('»');
            cursor = mark_end;
        }
    }
    if cursor < end {
        out.extend(&chars[cursor..end]);
    }
    if end < chars.len() {
        out.push('…');
    }
    Some(collapse_spaces(&out).trim_matches(|c: char| c == ' ' || c == '\t').to_string())
}

/// Runs of whitespace as one space.
pub fn collapse_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            space = true;
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    if space {
        out.push(' ');
    }
    out
}

/// One line, at most `limit` characters: for result lists.
pub fn excerpt(text: &str, limit: usize) -> String {
    // Only the start is shown, so only the start is cleaned up (a page or a chunk can be thousands of characters).
    let start: String = text.chars().take(limit * 8).collect();
    let cut = start.len() < text.len();
    let line = collapse_spaces(&start);
    let line = line.trim_matches(|c: char| c == ' ' || c == '\t');
    if line.chars().count() > limit || cut {
        format!("{}…", line.chars().take(limit).collect::<String>())
    } else {
        line.to_string()
    }
}

/// What's wrong with the text read from a PDF page, when its text layer can't be used as it is.
#[derive(PartialEq, Eq, Debug)]
pub enum PageTextProblem {
    /// Bengali or Hindi read in glyph order: vowel signs ahead of their consonants. The page is used as a picture.
    GlyphOrder,
    /// Arabic letters lost where the font drew a lam ligature ("الأردن" reads "ا?ردن").
    LostLetters,
}

impl PageTextProblem {
    pub fn check(text: &str) -> Option<PageTextProblem> {
        let (mut vowel_signs, mut misplaced, mut lost_letters) = (0, 0, 0);
        let (mut previous, mut before_previous) = (' ', ' ');
        for c in text.nfc() {
            let value = c as u32;
            if is_indic_vowel_sign(value) {
                vowel_signs += 1;
                if !is_indic_consonant_or_nukta(previous as u32) {
                    misplaced += 1;
                }
            }
            if before_previous == '\u{0627}' && "?.\u{FFFD}".contains(previous) && ARABIC.contains(&value)
                && c.is_alphabetic() {
                lost_letters += 1;
            }
            before_previous = previous;
            previous = c;
        }
        if vowel_signs >= 10 && misplaced * 20 >= vowel_signs {
            return Some(PageTextProblem::GlyphOrder);
        }
        (lost_letters >= 2).then_some(PageTextProblem::LostLetters)
    }
}

fn is_indic_vowel_sign(value: u32) -> bool {
    matches!(value, 0x09BE..=0x09C4 | 0x09C7 | 0x09C8 | 0x09CB | 0x09CC | 0x09D7 | 0x09E2 | 0x09E3
        | 0x093A | 0x093B | 0x093E..=0x094C | 0x094E | 0x094F | 0x0955..=0x0957 | 0x0962 | 0x0963)
}

fn is_indic_consonant_or_nukta(value: u32) -> bool {
    matches!(value, 0x0995..=0x09B9 | 0x09BC | 0x09CE | 0x09DC | 0x09DD | 0x09DF | 0x09F0 | 0x09F1
        | 0x0915..=0x0939 | 0x093C | 0x0958..=0x095F | 0x0979..=0x097F)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_arabic_and_bengali() {
        assert_eq!(normalized("أَحْمَد"), "احمد");
        assert_eq!(normalized("a\u{200C}b"), "ab");
        assert!(inflects("কুমির"));
        assert!(!inflects("crocodile"));
    }

    #[test]
    fn tells_words_from_numbers() {
        assert!(has_words("The lease says no pets."));
        assert!(!has_words("12.5, 13.75, 99.1, 100.0, 42.42"));
        assert!(has_words("চুক্তিপত্র"));
    }

    #[test]
    fn marks_matches_in_snippets() {
        let snippet = snippet("The tenant may keep one cat in the flat.", &["cat".into()], 12).unwrap();
        assert!(snippet.contains("«cat»"), "{snippet}");
        let chars: Vec<char> = "category cat".chars().collect();
        assert_eq!(matches("cat", &chars), vec![(9, 12)]);
        let chars: Vec<char> = "কুমিরের ছবি".chars().collect();
        assert_eq!(matches("কুমির", &chars), vec![(0, 7)]);
        let chars: Vec<char> = "Café au lait".chars().collect();
        assert_eq!(matches("cafe", &chars), vec![(0, 4)]);
    }

    #[test]
    fn excerpts_are_one_line() {
        assert_eq!(excerpt("a\n\n  b\tc", 240), "a b c");
        assert_eq!(excerpt(&"x".repeat(300), 10), format!("{}…", "x".repeat(10)));
    }
}
