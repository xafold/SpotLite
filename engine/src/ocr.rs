//! Text recognition through Tesseract (the `tesseract` program: `sudo apt install tesseract-ocr`), for keyword search
//! only: the words on screenshots and on scanned PDF pages, so an error code or an invoice number typed exactly finds
//! them. Their meaning stays the picture's vector. Languages: $SPOTLITE_OCR_LANGS as Tesseract names them ("eng+nep",
//! each needs its tesseract-ocr-<lang> package), else English.
use crate::embed::Rgb;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::LazyLock;

static TESSERACT: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path).map(|folder| folder.join("tesseract")).find(|candidate| candidate.is_file())
    })
});

pub fn available() -> bool {
    TESSERACT.is_some()
}

fn languages() -> String {
    std::env::var("SPOTLITE_OCR_LANGS").ok().filter(|l| !l.trim().is_empty()).unwrap_or_else(|| "eng".into())
}

/// Arabic is among the languages read: a PDF page whose text layer lost letters is read again by OCR.
pub fn reads_arabic() -> bool {
    languages().split('+').any(|l| l == "ara")
}

/// The words in the picture file at `path`, read at full resolution (small UI text is what screenshots are full of).
pub fn text_of_file(path: &str) -> String {
    run(path, None)
}

/// The words in a picture in memory (a rendered PDF page), handed to Tesseract as PPM.
pub fn text_of_image(image: &Rgb) -> String {
    let mut ppm = format!("P6\n{} {}\n255\n", image.width, image.height).into_bytes();
    ppm.extend_from_slice(&image.pixels);
    run("stdin", Some(ppm))
}

/// One Tesseract run on one thread (several run side by side, one per picture of a batch). Lines as Tesseract reads
/// them, blank ones dropped; "" when it can't read the picture or isn't installed.
fn run(input: &str, data: Option<Vec<u8>>) -> String {
    let Some(tesseract) = TESSERACT.as_ref() else { return String::new() };
    let child = Command::new(tesseract)
        .args([input, "stdout", "-l", &languages(), "--psm", "3", "quiet"])
        .env("OMP_THREAD_LIMIT", "1")
        .stdin(if data.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return String::new() };
    if let Some(data) = data {
        let mut stdin = child.stdin.take().unwrap();
        // A writer thread: Tesseract may start writing before it has read everything.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let Ok(output) = child.wait_with_output() else { return String::new() };
    String::from_utf8_lossy(&output.stdout).lines().map(str::trim_end).filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_rendered_words() {
        if !super::available() {
            return;
        }
        // "HI" drawn in 5×7 blocks, 8× scaled, black on white.
        let glyphs = ["H...H.HHHHH", "H...H...H..", "H...H...H..", "HHHHH...H..", "H...H...H..", "H...H...H..",
                      "H...H.HHHHH"];
        let (scale, margin) = (8usize, 40usize);
        let (width, height) = (11 * scale + 2 * margin, 7 * scale + 2 * margin);
        let mut pixels = vec![255u8; width * height * 3];
        for (row, line) in glyphs.iter().enumerate() {
            for (column, c) in line.chars().enumerate() {
                if c != 'H' {
                    continue;
                }
                for y in 0..scale {
                    for x in 0..scale {
                        let p = ((margin + row * scale + y) * width + margin + column * scale + x) * 3;
                        pixels[p..p + 3].fill(0);
                    }
                }
            }
        }
        let text = super::text_of_image(&crate::embed::Rgb { width, height, pixels });
        assert!(text.contains("HI"), "read {text:?}");
    }
}
