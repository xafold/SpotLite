//! What gets indexed. The defaults come from a real developer's machine: TypeScript files that look like movies, one
//! repo holding a 55k-clip dataset, and screenshot tools moving screenshots into code projects.
use crate::model::{FileKind, IndexOptions};
use crate::paths;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::LazyLock;

#[derive(Debug, PartialEq, Eq)]
pub enum Classification {
    Index(FileKind),
    /// A reason worth reporting ("code or data file").
    Skip(&'static str),
    /// A type not handled at all.
    Ignore,
}

/// Folders never worth walking into. Hidden folders are skipped by the walk itself.
const SKIPPED_FOLDER_NAMES: [&str; 8] =
    ["node_modules", "bower_components", "venv", "site-packages", "__pycache__", "DerivedData", "Pods", "Carthage"];

/// Any of these in a folder makes it a code project, where only screenshots are indexed.
const PROJECT_MARKERS: [&str; 14] = [
    ".git", "package.json", "Package.swift", "pyproject.toml", "setup.py", "requirements.txt", "Cargo.toml", "go.mod",
    "pom.xml", "build.gradle", "build.gradle.kts", "Gemfile", "composer.json", "CMakeLists.txt",
];

const DOC_EXTENSIONS: [&str; 10] = ["txt", "text", "md", "markdown", "rtf", "docx", "doc", "odt", "html", "htm"];

/// Source code and data files: off by default (an editor and ripgrep search them better, and they flood an index).
const CODE_EXTENSIONS: &[&str] = &[
    "py", "ipynb", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "m", "mm", "swift", "tsx", "cts", "js", "jsx", "mjs",
    "cjs", "go", "rs", "java", "kt", "kts", "scala", "rb", "php", "cs", "fs", "sh", "bash", "zsh", "fish", "ps1",
    "lua", "pl", "r", "jl", "dart", "sql", "vue", "svelte", "css", "scss", "sass", "less", "json", "jsonl", "yaml",
    "yml", "toml", "xml", "plist", "gradle", "cmake", "mk", "lock", "log", "csv", "tsv", "ini", "cfg", "conf",
    "proto", "graphql", "tf", "nix", "zig", "hs", "ex", "exs", "erl", "clj", "elm", "ml", "vim", "el", "asm",
    "metal", "glsl", "hlsl", "wgsl", "cu", "pyi", "pyc",
];

/// GNOME, KDE, COSMIC, Flameshot and the Mac all start their screenshots' names like this.
const SCREENSHOT_PREFIXES: [&str; 5] = ["Screenshot", "Screen Shot", "CleanShot", "SCR-", "flameshot"];

/// Source files code search reads.
pub const SOURCE_EXTENSIONS: &[&str] = &[
    "py", "pyi", "pyx", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx", "m", "mm", "swift", "ts", "tsx", "mts",
    "cts", "js", "jsx", "mjs", "cjs", "go", "rs", "java", "kt", "kts", "scala", "groovy", "rb", "php", "cs", "fs",
    "sh", "bash", "zsh", "fish", "ps1", "lua", "pl", "pm", "r", "jl", "dart", "sql", "vue", "svelte", "astro",
    "html", "htm", "css", "scss", "sass", "less", "gradle", "cmake", "mk", "proto", "graphql", "gql", "tf", "hcl",
    "nix", "zig", "hs", "ex", "exs", "erl", "clj", "cljs", "elm", "ml", "mli", "vim", "el", "lisp", "asm", "s",
    "metal", "glsl", "hlsl", "wgsl", "cu", "cuh", "ino", "v", "sv", "tex",
];
/// What goes with code: a repo's docs and its settings.
const CODE_TEXT_EXTENSIONS: [&str; 9] = ["md", "markdown", "rst", "adoc", "org", "txt", "yaml", "yml", "toml"];
/// Code known by its name alone.
pub const CODE_FILE_NAMES: [&str; 14] = [
    "Makefile", "makefile", "GNUmakefile", "Dockerfile", "Containerfile", "Justfile", "justfile", "Rakefile",
    "Gemfile", "Podfile", "Brewfile", "Procfile", "Vagrantfile", "Jenkinsfile",
];
/// Folders code search doesn't go into, besides what git ignores: other people's code kept in a repo, and what
/// builds and runs leave.
const CODE_SKIPPED_FOLDER_NAMES: [&str; 23] = [
    "vendor", "Vendor", "vendored", "third_party", "third-party", "thirdparty", "3rdparty", "external", "extern",
    "deps", "build", "Build", "dist", "out", "target", "obj", "coverage", "htmlcov", "generated", "__generated__",
    "wandb", "mlruns", "lightning_logs",
];

/// RAW pictures, which nothing here decodes yet, are still images: they fail with a note that says so.
const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "gif", "bmp", "tif", "tiff", "webp", "heic", "heif", "avif", "psd", "tga",
    "pnm", "ppm", "pgm", "cr2", "nef", "arw", "dng", "raf", "orf", "rw2",
];
const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "m4a", "m4b", "aac", "wav", "wave", "flac", "ogg", "oga", "opus", "aiff", "aif", "wma", "amr", "caf", "mka",
];
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "flv", "3gp", "3g2", "mpg", "mpeg", "m2v", "ogv", "mxf",
];
/// Mac bundles that may sit on a shared or copied disk: skipped whole, by name.
const PACKAGE_EXTENSIONS: &[&str] = &[
    "app", "framework", "bundle", "plugin", "appex", "xpc", "kext", "xcodeproj", "xcworkspace", "playground",
    "photoslibrary", "musiclibrary", "tvlibrary", "imovielibrary", "fcpbundle", "logicx", "band", "pkg", "dmg",
];

pub fn is_package_folder(path: &str) -> bool {
    PACKAGE_EXTENSIONS.contains(&paths::extension(path).as_str())
}

/// By extension, as the Mac's types are for files with one.
pub fn classify(path: &str, options: &IndexOptions) -> Classification {
    let ext = paths::extension(path);
    let ext = ext.as_str();
    if options.excluded_extensions.contains(ext) {
        return Classification::Skip("excluded type");
    }
    // .ts / .mts are usually TypeScript. Only a real transport stream is video.
    if ["ts", "mts", "m2ts"].contains(&ext) {
        if is_mpeg_transport_stream(path) {
            return Classification::Index(FileKind::Video);
        }
        return if options.include_code { Classification::Index(FileKind::Doc) } else {
            Classification::Skip("code or data file")
        };
    }
    if DOC_EXTENSIONS.contains(&ext) {
        return Classification::Index(FileKind::Doc);
    }
    if CODE_EXTENSIONS.contains(&ext) || SOURCE_EXTENSIONS.contains(&ext) {
        return if options.include_code { Classification::Index(FileKind::Doc) } else {
            Classification::Skip("code or data file")
        };
    }
    if ext == "pdf" {
        return Classification::Index(FileKind::Pdf);
    }
    if IMAGE_EXTENSIONS.contains(&ext) {
        return Classification::Index(if is_screenshot(path) { FileKind::Screenshot } else { FileKind::Image });
    }
    if VIDEO_EXTENSIONS.contains(&ext) {
        return Classification::Index(FileKind::Video);
    }
    if AUDIO_EXTENSIONS.contains(&ext) {
        return Classification::Index(FileKind::Audio);
    }
    Classification::Ignore
}

/// What code search reads in the code folders: source files, a repo's docs and settings, and notebooks. Never data
/// (json, csv, lockfiles, logs), minified bundles, licenses, or anything that looks like a key or a password file.
pub fn classify_code(path: &str, options: &IndexOptions) -> Classification {
    let name = paths::file_name(path);
    let ext = paths::extension(path);
    let ext = ext.as_str();
    if options.excluded_extensions.contains(ext) {
        return Classification::Skip("excluded type");
    }
    if is_secret(name) {
        return Classification::Skip("key or password file");
    }
    let lower = name.to_lowercase();
    if [".min.js", ".min.mjs", ".min.css"].iter().any(|suffix| lower.ends_with(suffix)) {
        return Classification::Skip("minified");
    }
    if ["license", "licence", "copying", "notice"].iter().any(|prefix| lower.starts_with(prefix)) {
        return Classification::Skip("license");
    }
    if (ext == "ts" || ext == "mts") && is_mpeg_transport_stream(path) {
        return Classification::Ignore;   // a video
    }
    if SOURCE_EXTENSIONS.contains(&ext) || CODE_TEXT_EXTENSIONS.contains(&ext) || ext == "ipynb"
        || CODE_FILE_NAMES.contains(&name) || name.starts_with("Dockerfile") {
        return Classification::Index(FileKind::Code);
    }
    if CODE_EXTENSIONS.contains(&ext) {
        return Classification::Skip("data file");
    }
    Classification::Ignore
}

/// Keys, certificates, password and credential files: never read, wherever they are.
pub fn is_secret(name: &str) -> bool {
    let lower = name.to_lowercase();
    const SECRET_TYPES: [&str; 15] = ["pem", "key", "p12", "pfx", "keystore", "jks", "ppk", "kdbx", "gpg", "asc", "crt",
                                      "cer", "der", "p8", "mobileprovision"];
    if SECRET_TYPES.contains(&paths::extension(&lower).as_str()) {
        return true;
    }
    ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", ".env", "secret", "credential", ".netrc", ".npmrc", ".pypirc",
     "htpasswd", ".htpasswd"].iter().any(|prefix| lower.starts_with(prefix))
}

/// Why code search doesn't go into a folder, or None.
pub fn code_skip_reason(folder: &str) -> Option<&'static str> {
    if CODE_SKIPPED_FOLDER_NAMES.contains(&paths::file_name(folder)) {
        return Some("vendored or built code");
    }
    // A submodule or a worktree (".git" is a file there): someone else's code, or a copy of a repo's own.
    if Path::new(&format!("{folder}/.git")).is_file() {
        return Some("submodule or worktree");
    }
    None
}

/// MPEG-TS packets start with 0x47 every 188 bytes (or every 192 with a 4-byte timecode, as in .m2ts / AVCHD .mts).
pub fn is_mpeg_transport_stream(path: &str) -> bool {
    let mut bytes = [0u8; 600];
    let Ok(mut file) = std::fs::File::open(path) else { return false };
    let mut read = 0;
    while read < bytes.len() {
        match file.read(&mut bytes[read..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => read += n,
        }
    }
    read >= 400 && ((bytes[0] == 0x47 && bytes[188] == 0x47 && bytes[376] == 0x47)
        || (bytes[4] == 0x47 && bytes[196] == 0x47 && bytes[388] == 0x47))
}

/// Screenshots are known by their names here.
pub fn is_screenshot(path: &str) -> bool {
    let name = paths::file_name(path);
    SCREENSHOT_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

pub fn skip_reason(folder: &str, home: &str, excluded: &[String]) -> Option<&'static str> {
    let name = paths::file_name(folder);
    if SKIPPED_FOLDER_NAMES.contains(&name) {
        return Some("developer folder");
    }
    if name.ends_with(".noindex") {
        return Some(".noindex folder");
    }
    // Linux's ~/Library: snaps' own data (hidden folders such as ~/.local and ~/.cache are never walked).
    if folder == format!("{home}/snap") {
        return Some("~/snap");
    }
    if excluded.iter().any(|skip| folder == skip || is_inside(folder, skip)) {
        return Some("excluded folder");
    }
    None
}

pub fn is_project_root(folder: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(folder) else { return false };
    let names: HashSet<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    PROJECT_MARKERS.iter().any(|marker| names.contains(*marker))
        || names.iter().any(|name| ["xcodeproj", "xcworkspace"].contains(&paths::extension(name).as_str()))
}

const CAMERA_PREFIXES: [&str; 10] = ["img", "dsc", "pxl", "mvimg", "vid", "gopr", "dji", "photo", "whatsapp", "screenshot"];

static HEX_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{8,}([-_][0-9a-f]{4,})*$").unwrap());
static NUMBERED_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z]{0,16}[-_. ]?\d{3,}([-_.]\d+)*$").unwrap());

/// Machine-made names like "clip_000123", "frame-0042" or "3f9a1c0e-…", but not camera names like "IMG_1234".
pub fn looks_machine_named(stem: &str) -> bool {
    let name = stem.to_lowercase();
    if CAMERA_PREFIXES.iter().any(|prefix| name.starts_with(prefix)) {
        return false;
    }
    HEX_NAME.is_match(&name) || NUMBERED_NAME.is_match(&name)
}

/// Folders with hundreds of same-kind, machine-named media files: training data, not your stuff. They're skipped (and
/// reported). Roots you pick yourself are never datasets.
pub fn dataset_folders(files: &[(String, String, FileKind)], roots: &HashSet<String>) -> HashMap<String, usize> {
    let mut groups: HashMap<(String, FileKind), Vec<&str>> = HashMap::new();
    for (folder, stem, kind) in files {
        if matches!(kind, FileKind::Image | FileKind::Audio | FileKind::Video) && !roots.contains(folder) {
            groups.entry((folder.clone(), *kind)).or_default().push(stem);
        }
    }
    let mut datasets = HashMap::new();
    for ((folder, _), stems) in groups {
        if stems.len() >= 300 {
            let machine = stems.iter().filter(|stem| looks_machine_named(stem)).count();
            if machine as f64 / stems.len() as f64 >= 0.7 {
                *datasets.entry(folder).or_insert(0) += stems.len();
            }
        }
    }
    datasets
}

// MARK: Folder selection: the folders chosen, minus the folders skipped inside them.

pub fn is_inside(path: &str, folder: &str) -> bool {
    if folder.ends_with('/') { path.starts_with(folder) } else {
        path.len() > folder.len() && path.starts_with(folder) && path.as_bytes()[folder.len()] == b'/'
    }
}

/// Whether `path` (a folder or a file) is searched: under a chosen folder and not under a skip inside it. A chosen
/// folder is searched whole even inside a skipped one.
pub fn is_searched(path: &str, folders: &[String], skipped: &[String]) -> bool {
    folders.iter().any(|folder| {
        (path == folder || is_inside(path, folder))
            && !skipped.iter().any(|skip| is_inside(skip, folder) && (path == skip || is_inside(path, skip)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_files() {
        let options = IndexOptions::default();
        assert_eq!(classify("/a/Screenshot from 2026-10-01.png", &options), Classification::Index(FileKind::Screenshot));
        assert_eq!(classify("/a/IMG_1234.HEIC", &options), Classification::Index(FileKind::Image));
        assert_eq!(classify("/a/x.py", &options), Classification::Skip("code or data file"));
        assert_eq!(classify("/a/x.xyz", &options), Classification::Ignore);
        assert_eq!(classify_code("/a/.env.local", &options), Classification::Skip("key or password file"));
        assert_eq!(classify_code("/a/main.rs", &options), Classification::Index(FileKind::Code));
    }

    #[test]
    fn spots_machine_names_and_selection() {
        assert!(looks_machine_named("clip_000123"));
        assert!(looks_machine_named("3f9a1c0e-1234"));
        assert!(!looks_machine_named("IMG_1234"));
        let folders = vec!["/h/Downloads".to_string(), "/h/Downloads/Media/Courses".to_string()];
        let skipped = vec!["/h/Downloads/Media".to_string()];
        assert!(is_searched("/h/Downloads/a.pdf", &folders, &skipped));
        assert!(!is_searched("/h/Downloads/Media/b.mp4", &folders, &skipped));
        assert!(is_searched("/h/Downloads/Media/Courses/c.mp4", &folders, &skipped));
        assert!(!is_inside("/h/Downloads2/x", "/h/Downloads"));
    }
}
