//! The EmbeddingGemma 2 files: ggml-org's Q8_0 GGUF conversion at one pinned revision (never the F16 files: the
//! model's activations overflow float16). `spotlite download-model [--to DIR]` puts them in DIR (default:
//! $SPOTLITE_MODELS when set, as for finding them, else SpotLite's Models folder). A file already in the Hugging Face
//! cache is copied from there; anything else is downloaded, picking up where an earlier try stopped. Every file is
//! checked (size, SHA-256) before it gets its name.
//!
//! JSON lines on stdout:
//!   {"event":"file","name":…,"bytes":n,"from":"cache|download"}             a file started
//!   {"event":"progress","name":…,"bytes":n,"total":n,"all_bytes":n,"all_total":n}   at most 5 times a second
//!   {"event":"verifying","name":…}                                          the SHA-256 check
//!   {"event":"retry","message":…,"seconds":n}                               a failure it will try again after
//!   {"event":"done","folder":…} | {"event":"error","message":…}             the end (exit 0 | 1)
use crate::emit;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const REPOSITORY: &str = "ggml-org/embeddinggemma-2-GGUF";
pub const REVISION: &str = "bfcd298762cc34d0357ece5ebdd31791a3a374d8";

pub struct ModelFile {
    pub name: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

/// The text model (310 MB): all that queries need.
pub const TEXT: ModelFile = ModelFile {
    name: "embeddinggemma-2-Q8_0.gguf",
    bytes: 309_855_456,
    sha256: "2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135",
};
/// The image and audio encoders (555 MB).
pub const MEDIA: ModelFile = ModelFile {
    name: "mmproj-embeddinggemma-2-Q8_0.gguf",
    bytes: 554_821_024,
    sha256: "c4a8a52691ecef40618438928bdf9e68379b854e24166f292592353db0aab64f",
};
pub const ALL: [&ModelFile; 2] = [&TEXT, &MEDIA];

pub fn total_bytes() -> u64 {
    ALL.iter().map(|f| f.bytes).sum()
}

/// Where SpotLite keeps them.
pub fn installed() -> PathBuf {
    crate::paths::application_support().join("Models")
}

/// The pinned revision's folder in the Hugging Face cache: `$HF_HUB_CACHE`, `$HF_HOME/hub`, or
/// `~/.cache/huggingface/hub`.
pub fn hugging_face_snapshot() -> PathBuf {
    let hub = std::env::var_os("HF_HUB_CACHE").map(PathBuf::from)
        .or_else(|| std::env::var_os("HF_HOME").map(|home| PathBuf::from(home).join("hub")))
        .unwrap_or_else(|| crate::paths::home().join(".cache/huggingface/hub"));
    hub.join(format!("models--{}", REPOSITORY.replace('/', "--"))).join("snapshots").join(REVISION)
}

fn url(file: &ModelFile) -> String {
    format!("https://huggingface.co/{REPOSITORY}/resolve/{REVISION}/{}", file.name)
}

pub fn sha256(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 8 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

fn size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

enum Outcome {
    Downloaded(PathBuf),
    Failed(String, bool),   // message, worth retrying
}

pub fn run(to: Option<String>) -> i32 {
    let to = to.or_else(|| std::env::var("SPOTLITE_MODELS").ok().filter(|chosen| !chosen.is_empty()));
    let folder = to.map(|t| PathBuf::from(crate::paths::folder_path(&t))).unwrap_or_else(installed);
    if let Err(error) = std::fs::create_dir_all(&folder) {
        emit(json!({"event": "error", "message": format!("can't make {}: {error}", folder.display())}));
        return 1;
    }
    let missing: Vec<&ModelFile> = ALL.into_iter().filter(|f| !folder.join(f.name).exists()).collect();
    let mut done = total_bytes() - missing.iter().map(|f| f.bytes).sum::<u64>();
    for file in missing {
        let target = folder.join(file.name);
        let partial = folder.join(format!("{}.partial", file.name));
        let cached = hugging_face_snapshot().join(file.name);
        let cached = std::fs::canonicalize(&cached).unwrap_or(cached);
        if size(&cached) == file.bytes && sha256(&cached).ok().as_deref() == Some(file.sha256) {
            emit(json!({"event": "file", "name": file.name, "bytes": file.bytes, "from": "cache"}));
            let copied = std::fs::copy(&cached, &partial).and_then(|_| std::fs::rename(&partial, &target));
            if let Err(error) = copied {
                emit(json!({"event": "error", "message": format!("can't copy {}: {error}", file.name)}));
                return 1;
            }
            done += file.bytes;
            continue;
        }
        emit(json!({"event": "file", "name": file.name, "bytes": file.bytes, "from": "download"}));
        let mut failures = 0u32;
        loop {
            match fetch(file, &partial, done) {
                Outcome::Downloaded(path) => {
                    emit(json!({"event": "verifying", "name": file.name}));
                    if sha256(&path).ok().as_deref() != Some(file.sha256) {
                        let _ = std::fs::remove_file(&path);
                        failures += 1;
                        if failures >= 3 {
                            emit(json!({"event": "error", "message": format!("{} arrived damaged, three times", file.name)}));
                            return 1;
                        }
                        emit(json!({"event": "retry", "message": format!("{} arrived damaged", file.name), "seconds": 0}));
                        continue;
                    }
                    if let Err(error) = std::fs::rename(&path, &target) {
                        emit(json!({"event": "error", "message": format!("can't save {}: {error}", file.name)}));
                        return 1;
                    }
                    done += file.bytes;
                    break;
                }
                Outcome::Failed(message, retry) => {
                    failures += 1;
                    if !retry || failures > 20 {
                        emit(json!({"event": "error", "message": message}));
                        return 1;
                    }
                    // A flaky link resumes where it stopped: 2, 4, 8 … seconds apart, a minute at most.
                    let seconds = 60.min(1u64 << failures.min(6));
                    emit(json!({"event": "retry", "message": message, "seconds": seconds}));
                    std::thread::sleep(Duration::from_secs(seconds));
                }
            }
        }
    }
    emit(json!({"event": "done", "folder": folder.display().to_string()}));
    0
}

/// One try at one file into `partial`, resuming from what's there (an HTTP range, carried across Hugging Face's
/// redirect to its CDN, which ureq would drop).
fn fetch(file: &ModelFile, partial: &Path, before: u64) -> Outcome {
    let mut written = size(partial);
    if written > file.bytes {
        let _ = std::fs::remove_file(partial);
        written = 0;
    }
    if written == file.bytes {
        return Outcome::Downloaded(partial.to_path_buf());
    }
    let agent = ureq::AgentBuilder::new().redirects(0).timeout_read(Duration::from_secs(60)).build();
    let mut address = url(file);
    let response = loop {
        let mut request = agent.get(&address);
        if written > 0 {
            request = request.set("Range", &format!("bytes={written}-"));
        }
        match request.call() {
            Ok(response) if (300..400).contains(&response.status()) => match response.header("location") {
                Some(location) if location.starts_with("http") => address = location.to_string(),
                Some(location) => address = format!("https://huggingface.co{location}"),
                None => return Outcome::Failed("a redirect without a place".into(), true),
            },
            Ok(response) => break response,
            Err(ureq::Error::Status(status, _)) => {
                return Outcome::Failed(format!("Hugging Face answered {status}"), status == 429 || status >= 500);
            }
            Err(error) => return Outcome::Failed(error.to_string(), true),
        }
    };
    match response.status() {
        206 => {
            let range = response.header("content-range").unwrap_or_default();
            if !range.starts_with(&format!("bytes {written}-")) {
                let _ = std::fs::remove_file(partial);
                return Outcome::Failed("The download couldn't resume".into(), true);
            }
        }
        200 => written = 0,
        status => return Outcome::Failed(format!("Hugging Face answered {status}"), status >= 500),
    }
    let mut out = match std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(partial) {
        Ok(out) => out,
        Err(error) => return Outcome::Failed(format!("Can't save the model: {error}"), false),
    };
    if out.set_len(written).is_err() {
        return Outcome::Failed("Can't save the model".into(), false);
    }
    use std::io::Seek;
    let _ = out.seek(std::io::SeekFrom::End(0));
    let mut reader = response.into_reader();
    let mut buffer = vec![0u8; 1 << 20];
    let mut reported = Instant::now() - Duration::from_secs(1);
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) => return Outcome::Failed(error.to_string(), true),
        };
        if let Err(error) = out.write_all(&buffer[..read]) {
            return Outcome::Failed(format!("Can't save the model: {error}"), false);
        }
        written += read as u64;
        if reported.elapsed() >= Duration::from_millis(200) {
            reported = Instant::now();
            emit(json!({"event": "progress", "name": file.name, "bytes": written, "total": file.bytes,
                        "all_bytes": before + written, "all_total": total_bytes()}));
        }
    }
    emit(json!({"event": "progress", "name": file.name, "bytes": written, "total": file.bytes,
                "all_bytes": before + written, "all_total": total_bytes()}));
    if written == file.bytes { Outcome::Downloaded(partial.to_path_buf()) } else {
        Outcome::Failed("The download stopped early".into(), true)
    }
}
