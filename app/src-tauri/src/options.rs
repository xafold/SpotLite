//! What the shell is told at launch. The folders and the hotkey come from the command line until onboarding and the
//! settings window exist.

use std::path::PathBuf;

/// The panel's hotkey unless told otherwise: ⇧⌘Space on a Mac (DigUp's), Ctrl+Shift+Space elsewhere.
pub const DEFAULT_HOTKEY: &str = "CommandOrControl+Shift+Space";

pub struct Options {
    /// SpotLite's helper, `spotlite` (default: beside this program, where the installers put it).
    pub helper: PathBuf,
    /// The index folder. None: the helper's own default (%LOCALAPPDATA%\SpotLite\index on Windows,
    /// ~/.local/share/spotlite/index on Linux).
    pub index: Option<PathBuf>,
    /// Where the model files are (SPOTLITE_MODELS for the helpers); None: the helper looks where it always does.
    pub models: Option<PathBuf>,
    /// The folders to search and keep indexed, instead of the settings' (dev runs; setup is then skipped).
    pub roots: Vec<PathBuf>,
    /// Dev runs: the code folders, instead of the settings'.
    pub code_roots: Vec<PathBuf>,
    /// `--browser URL`: run as `search:`'s browser (browser.rs) instead of the app.
    pub browser: Option<String>,
    /// The hotkey, instead of the settings'.
    pub hotkey: Option<String>,
    /// The settings file, instead of the one in the app's config folder (a fresh first launch in tests).
    pub settings: Option<PathBuf>,
    /// The folders setup offers, instead of the usual ones (Documents, Pictures…): dev runs never offer real folders.
    pub offered: Option<Vec<PathBuf>>,
    /// Debug runs: setup clicks through by itself (all offered folders, the default hotkey).
    pub debug_setup: bool,
    /// Debug runs: the panel types this query in when it loads and reports what it showed (`debug_report`), hidden
    /// unless `debug_show`: a panel on screen takes the keyboard.
    pub debug_query: Option<String>,
    /// Debug runs: quit once the report is in.
    pub debug_quit: bool,
    /// Debug runs: show the panel at launch and keep it up when it loses focus.
    pub debug_show: bool,
    /// Debug runs: the panel goes through these states ("name=query|name=query|…"; an empty query clears the field,
    /// "!weaker" opens the weaker matches) and reports each one's size and what shows (`debug_report`, one line a
    /// state, the shell staying up).
    pub debug_sizes: Option<String>,
    /// Debug runs: the panel clicks "New files" once it's up and reports what the button said (`debug_report`).
    pub debug_index_new: bool,
    /// Show the panel once it's up (`--panel`): what a desktop shortcut runs, so its first press opens the panel too
    /// (a press while SpotLite runs reaches the running one, which toggles it).
    pub panel: bool,
}

impl Options {
    pub fn from_args() -> Result<Options, String> {
        let mut options = Options {
            helper: default_helper(),
            index: None,
            models: None,
            roots: Vec::new(),
            code_roots: Vec::new(),
            browser: None,
            hotkey: None,
            settings: None,
            offered: None,
            debug_setup: false,
            debug_query: None,
            debug_quit: false,
            debug_show: false,
            panel: false,
            debug_index_new: false,
            debug_sizes: None,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--helper" => options.helper = value()?.into(),
                "--index" => options.index = Some(value()?.into()),
                "--models" => options.models = Some(value()?.into()),
                // Separated as PATH is: ":" here, ";" on Windows.
                "--roots" => options.roots = std::env::split_paths(&value()?).collect(),
                "--code-roots" => options.code_roots = std::env::split_paths(&value()?).collect(),
                "--browser" => options.browser = Some(value()?),
                "--hotkey" => options.hotkey = Some(value()?),
                "--settings" => options.settings = Some(value()?.into()),
                "--offer" => options.offered = Some(std::env::split_paths(&value()?).collect()),
                "--debug-setup" => options.debug_setup = true,
                "--debug-query" => options.debug_query = Some(value()?),
                "--debug-quit" => options.debug_quit = true,
                "--debug-show" => options.debug_show = true,
                "--panel" => options.panel = true,
                "--debug-index-new" => options.debug_index_new = true,
                "--debug-sizes" => options.debug_sizes = Some(value()?),
                _ => return Err(format!("unknown argument {arg}")),
            }
        }
        // On a Mac the shell is only tried out: the helper's default index there is DigUp's Mac app's.
        if cfg!(target_os = "macos") && options.index.is_none() {
            return Err("pass --index: on a Mac the default index belongs to the SpotLite app".into());
        }
        Ok(options)
    }
}

impl Options {
    /// The index folder the helpers use: the one given, else the helper's default (which the watcher must pass over).
    pub fn index_folder(&self) -> Option<PathBuf> {
        if self.index.is_some() {
            return self.index.clone();
        }
        if cfg!(windows) {
            return std::env::var_os("LOCALAPPDATA").map(|local| PathBuf::from(local).join("SpotLite").join("index"));
        }
        let data = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()).map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
        data.map(|data| data.join("spotlite").join("index"))
    }
}

/// The helper beside this program, or in its `helper` folder (a test build keeps the helper and its libraries there:
/// on Windows "SpotLite.exe" and "spotlite.exe" can't share a folder).
fn default_helper() -> PathBuf {
    let name = if cfg!(windows) { "spotlite.exe" } else { "spotlite" };
    let Some(folder) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(PathBuf::from)) else {
        return PathBuf::from(name);
    };
    let inside = folder.join("helper").join(name);
    if inside.is_file() { inside } else { folder.join(name) }
}
