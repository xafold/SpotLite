//! Rich Text Format, read for its words: control words dropped (paragraphs and lines kept as line breaks), escapes
//! decoded, destination groups (fonts, colors, styles, info, pictures) skipped. Enough for search.

const SKIPPED_GROUPS: [&str; 26] = [
    "fonttbl", "colortbl", "stylesheet", "info", "pict", "header", "footer", "headerl", "headerr", "footerl",
    "footerr", "listtable", "listoverridetable", "rsidtbl", "generator", "themedata", "colorschememapping",
    "datastore", "xmlnstbl", "latentstyles", "pgdsctbl", "filetbl", "revtbl", "mmathPr", "fldinst", "object",
];

pub fn text(rtf: &str) -> String {
    let chars: Vec<char> = rtf.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut skip_depth = 0;     // inside a skipped group: its nesting depth
    let mut unicode_skip = 1;   // \ucN: characters after \uN to skip
    let mut pending_skip = 0;
    while i < chars.len() {
        let c = chars[i];
        if skip_depth > 0 {
            match c {
                '{' => skip_depth += 1,
                '}' => skip_depth -= 1,
                '\\' => i += 1,
                _ => {}
            }
            i += 1;
            continue;
        }
        match c {
            '{' => {
                let mut j = i + 1;
                if j < chars.len() && chars[j] == '\\' {
                    j += 1;
                    if j < chars.len() && chars[j] == '*' {   // {\* …}: a destination readers may ignore
                        skip_depth = 1;
                        i = j + 1;
                        continue;
                    }
                    let mut word = String::new();
                    while j < chars.len() && chars[j].is_ascii_alphabetic() && word.len() < 32 {
                        word.push(chars[j]);
                        j += 1;
                    }
                    if SKIPPED_GROUPS.contains(&word.as_str()) {
                        skip_depth = 1;
                        i = j;
                        continue;
                    }
                }
                i += 1;
            }
            '}' => i += 1,
            '\\' => {
                i += 1;
                if i >= chars.len() {
                    break;
                }
                let next = chars[i];
                if next == '\'' {   // \'hh: a byte in the document's code page (Windows-1252 here)
                    let hex: String = chars.get(i + 1..i + 3).map(|h| h.iter().collect()).unwrap_or_default();
                    match u8::from_str_radix(&hex, 16) {
                        Ok(byte) => {
                            if pending_skip > 0 { pending_skip -= 1 } else { out.push(windows_1252(byte)) }
                            i += 3;
                        }
                        Err(_) => i += 1,
                    }
                } else if next.is_ascii_alphabetic() {
                    let (mut word, mut number) = (String::new(), String::new());
                    let mut j = i;
                    while j < chars.len() && chars[j].is_ascii_alphabetic() {
                        word.push(chars[j]);
                        j += 1;
                    }
                    if j < chars.len() && chars[j] == '-' {
                        number.push('-');
                        j += 1;
                    }
                    while j < chars.len() && chars[j].is_ascii_digit() {
                        number.push(chars[j]);
                        j += 1;
                    }
                    if j < chars.len() && chars[j] == ' ' {
                        j += 1;   // the space ending a control word is part of it
                    }
                    match word.as_str() {
                        "par" | "line" | "sect" | "page" | "row" => out.push('\n'),
                        "tab" | "cell" => out.push('\t'),
                        "emdash" => out.push('—'),
                        "endash" => out.push('–'),
                        "lquote" => out.push('‘'),
                        "rquote" => out.push('’'),
                        "ldblquote" => out.push('“'),
                        "rdblquote" => out.push('”'),
                        "bullet" => out.push('•'),
                        "uc" => unicode_skip = number.parse().unwrap_or(1),
                        "u" => {
                            if let Ok(value) = number.parse::<i32>() {
                                let value = if value < 0 { value + 65536 } else { value };
                                if let Some(c) = char::from_u32(value as u32) {
                                    out.push(c);
                                }
                            }
                            pending_skip = unicode_skip;
                        }
                        _ => {}
                    }
                    i = j;
                } else {
                    match next {
                        '\\' | '{' | '}' => out.push(next),
                        '~' => out.push(' '),
                        '\n' | '\r' => out.push('\n'),
                        _ => {}   // \-, \_, \:, \| and other symbols
                    }
                    i += 1;
                }
            }
            '\r' | '\n' => i += 1,
            _ => {
                if pending_skip > 0 { pending_skip -= 1 } else { out.push(c) }
                i += 1;
            }
        }
    }
    out
}

/// Windows-1252's upper half; the rest is Latin-1.
fn windows_1252(byte: u8) -> char {
    let special = match byte {
        0x80 => 0x20AC, 0x82 => 0x201A, 0x83 => 0x0192, 0x84 => 0x201E, 0x85 => 0x2026, 0x86 => 0x2020,
        0x87 => 0x2021, 0x88 => 0x02C6, 0x89 => 0x2030, 0x8A => 0x0160, 0x8B => 0x2039, 0x8C => 0x0152,
        0x8E => 0x017D, 0x91 => 0x2018, 0x92 => 0x2019, 0x93 => 0x201C, 0x94 => 0x201D, 0x95 => 0x2022,
        0x96 => 0x2013, 0x97 => 0x2014, 0x98 => 0x02DC, 0x99 => 0x2122, 0x9A => 0x0161, 0x9B => 0x203A,
        0x9C => 0x0153, 0x9E => 0x017E, 0x9F => 0x0178,
        other => other as u32,
    };
    char::from_u32(special).unwrap_or(' ')
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_words() {
        let rtf = r"{\rtf1\ansi{\fonttbl{\f0 Helvetica;}}\f0 Caf\'e9 \b bold\b0\par next \u8364?}";
        assert_eq!(super::text(rtf), "Café bold\nnext €");
    }
}
