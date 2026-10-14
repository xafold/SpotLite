//! Where SpotLite keeps its own files: $XDG_DATA_HOME/spotlite, or ~/.local/share/spotlite (the model and the index).
use std::path::{Path, PathBuf};

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn home_string() -> String {
    home().display().to_string()
}

pub fn application_support() -> PathBuf {
    match std::env::var("XDG_DATA_HOME") {
        Ok(data) if !data.is_empty() => PathBuf::from(data).join("spotlite"),
        _ => home().join(".local/share/spotlite"),
    }
}

pub fn default_index() -> PathBuf {
    application_support().join("index")
}

/// The dev build's llama.cpp libraries (scripts/build-llama.sh); none in a package, which carries them beside itself.
pub fn dev_llama_lib() -> Option<PathBuf> {
    option_env!("SPOTLITE_DEV_VENDOR").map(|vendor| Path::new(vendor).join("llama/lib"))
}

/// "~/x" → "/home/me/x"; then made absolute and tidied (".." and "." resolved by name, not by following links).
pub fn folder_path(path: &str) -> String {
    let expanded = match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{}{rest}", home_string()),
        _ => path.to_string(),
    };
    let absolute = if expanded.starts_with('/') {
        PathBuf::from(expanded)
    } else {
        std::env::current_dir().unwrap_or_default().join(expanded)
    };
    let mut parts: Vec<String> = vec![];
    for part in absolute.to_string_lossy().split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other.to_string()),
        }
    }
    format!("/{}", parts.join("/"))
}

/// The path as a walk of the folder reports it: links resolved.
pub fn real_path(path: &str) -> String {
    std::fs::canonicalize(path).map(|p| p.display().to_string()).unwrap_or_else(|_| path.to_string())
}

/// "/home/me/Documents" → "~/Documents", for printing.
pub fn tilde(path: &str) -> String {
    let home = home_string();
    match path.strip_prefix(&home) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// The last part of a path ("report.pdf").
pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Everything before the last part ("/home/me/Documents").
pub fn parent(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(index) => &path[..index],
        None => "",
    }
}

/// The extension, lowercased ("pdf"), or "".
pub fn extension(path: &str) -> String {
    let name = file_name(path);
    match name.rfind('.') {
        Some(index) if index > 0 => name[index + 1..].to_lowercase(),
        _ => String::new(),
    }
}

/// The file name without its extension.
pub fn stem(path: &str) -> &str {
    let name = file_name(path);
    match name.rfind('.') {
        Some(index) if index > 0 => &name[..index],
        _ => name,
    }
}
