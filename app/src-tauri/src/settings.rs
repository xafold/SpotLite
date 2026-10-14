//! What the person chose: the folders SpotLite searches, what it skips in them, the hotkey, and whether setup is done. A
//! JSON file in the app's config folder (%APPDATA%\com.xafold.spotlite on Windows, ~/.config/com.xafold.spotlite on
//! Linux). Launch arguments (`--roots`, `--code-roots`, `--hotkey`) stand in for it in dev runs.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// The folders searched and kept indexed.
    pub folders: Vec<String>,
    /// Folders inside them that aren't (`FolderSelection` in the engine).
    pub skipped: Vec<String>,
    /// File extensions left out ("heic", "txt").
    pub skip_types: Vec<String>,
    /// The folders of code `code:` searches (its own index, code.sqlite); none: no code search.
    pub code_roots: Vec<String>,
    /// Code indexing and `code:` search are turned off (the code folders and code.sqlite are kept for turning it on).
    pub code_index_off: bool,
    /// The panel's hotkey (Tauri's accelerator form); empty: the default.
    pub hotkey: String,
    /// Onboarding is done: the shell starts in the tray.
    pub set_up: bool,
}

impl Settings {
    /// The file's settings, or the defaults when there's none (or it can't be read).
    pub fn load(path: &Path) -> Settings {
        std::fs::read(path).ok().and_then(|data| serde_json::from_slice(&data).ok()).unwrap_or_default()
    }

    /// Written whole to a temporary file first, so a crash never leaves half of it.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder)?;
        }
        let temporary = path.with_extension("json.new");
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(temporary, path)
    }
}
