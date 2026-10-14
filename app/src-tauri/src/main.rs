// SpotLite's shell for Windows and Linux (tried out on the Mac first): the tray icon, the hotkey, the search panel and the
// setup window. The work is SpotLite's helper, `spotlite`, run as processes speaking JSON lines (helper.rs): `search-service`
// answers the panel, `index-service` keeps the index current, `download-model` fetches the model, `estimate` sizes up
// folders. DigUp's Mac app (github.com/ARahim3/DigUp), which SpotLite
// grew from, is the reference for behavior.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
mod browser;
mod helper;
mod open_at;
mod options;
mod settings;
mod watcher;

use helper::{IndexService, Launch, ModelDownload, SearchService};
use options::{Options, DEFAULT_HOTKEY};
use serde_json::{json, Value};
use settings::Settings;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, State, WebviewUrl, WebviewWindowBuilder, WindowEvent, Wry};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use tauri_plugin_opener::OpenerExt;
use watcher::FolderWatcher;

const PANEL_WIDTH: f64 = 780.0;
const FIELD_HEIGHT: f64 = 56.0;
/// The panel with results (panel.js's PANEL_HEIGHT). On Linux the window is always this tall (`fit_panel`).
const PANEL_TALLEST: f64 = 480.0;

/// What the commands and events share.
struct Shell {
    options: Options,
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    search: SearchService,
    index: IndexService,
    model: ModelDownload,
    /// The model for the setup window and the tray: {"state": "checking|downloading|waiting|ready|failed", …}.
    model_state: Mutex<Value>,
    watcher: Mutex<Option<FolderWatcher>>,
    /// The tray menu's first line: what's going on.
    status: Mutex<Option<MenuItem<Wry>>>,
    /// The tray's "Index Code" check.
    code_item: Mutex<Option<CheckMenuItem<Wry>>>,
    /// The tray's "Index New Files": greyed out while files are being read.
    index_item: Mutex<Option<MenuItem<Wry>>>,
    /// When the search service last reloaded the index (while indexing, at most every few seconds).
    reloaded: Mutex<Instant>,
    /// Debug runs with setup: the panel's query waits until the first index is in.
    debug_query_sent: AtomicBool,
    logs: PathBuf,
}

impl Shell {
    /// The folders searched: the launch arguments' in dev runs, else the settings'.
    fn folders(&self) -> Vec<String> {
        if !self.options.roots.is_empty() {
            return self.options.roots.iter().map(|root| root.display().to_string()).collect();
        }
        self.settings.lock().unwrap().folders.clone()
    }

    /// The code folders: the launch arguments' in dev runs, else the settings'.
    fn code_roots(&self) -> Vec<String> {
        code_roots(&self.options, &self.settings.lock().unwrap())
    }

    fn is_set_up(&self) -> bool {
        !self.options.roots.is_empty() || self.settings.lock().unwrap().set_up
    }

    fn hotkey(&self) -> String {
        let chosen = self.settings.lock().unwrap().hotkey.clone();
        self.options.hotkey.clone().unwrap_or(if chosen.is_empty() { DEFAULT_HOTKEY.to_string() } else { chosen })
    }

    /// A background helper (indexing, the download), below the person's own programs for the processor.
    fn launch(&self, args: Vec<String>, log: &str) -> Launch {
        Launch {
            program: self.options.helper.clone(),
            args,
            env: self.options.models.iter().map(|models| ("SPOTLITE_MODELS".to_string(), models.display().to_string())).collect(),
            log: Some(self.logs.join(log)),
            background: true,
        }
    }

    fn index_arguments(&self) -> Vec<String> {
        self.options.index.iter().flat_map(|index| ["--index".to_string(), index.display().to_string()]).collect()
    }

    /// `index-service` for the folders as they are now.
    fn index_launch(&self) -> Launch {
        let settings = self.settings.lock().unwrap().clone();
        let mut args = vec!["index-service".to_string(), "--backfill".to_string()];
        args.extend(self.index_arguments());
        for folder in &settings.skipped {
            args.extend(["--exclude".to_string(), folder.clone()]);
        }
        for extension in &settings.skip_types {
            args.extend(["--exclude-ext".to_string(), extension.clone()]);
        }
        for root in self.code_roots() {
            args.extend(["--code-root".to_string(), root]);
        }
        args.extend(self.folders());
        self.launch(args, "index.log")
    }

    fn model_ready(&self) -> bool {
        self.model_state.lock().unwrap()["state"] == "ready"
    }
}

// MARK: Commands

/// The panel's questions for `search-service`.
#[tauri::command]
async fn ask(shell: State<'_, Arc<Shell>>, request: Value) -> Result<Value, String> {
    let op = request.get("op").and_then(Value::as_str).unwrap_or_default();
    if !["search", "embed_ahead", "text", "render", "prewarm", "status"].contains(&op) {
        return Err(format!("not a question the panel asks: {op}"));
    }
    shell.search.ask(request).await
}

/// What indexing these folders would take, one answer per folder (`spotlite estimate --json`): files by kind, the first
/// pass's seconds, the rest of long files, subfolders. For setup.
#[tauri::command]
async fn estimate(shell: State<'_, Arc<Shell>>, folders: Vec<String>) -> Result<Vec<Value>, String> {
    let helper = shell.options.helper.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut command = std::process::Command::new(&helper);
        command.arg("estimate").args(&folders).arg("--json");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let output = command.output().map_err(|error| format!("can't run {}: {error}", helper.display()))?;
        Ok(String::from_utf8_lossy(&output.stdout).lines().filter_map(|line| serde_json::from_str(line).ok()).collect())
    })
    .await
    .map_err(|error| error.to_string())?
}

/// What the setup window starts from: the folders it offers, the hotkey, the model, the folders already chosen.
#[tauri::command]
fn setup_info(app: AppHandle, shell: State<'_, Arc<Shell>>) -> Value {
    let offered: Vec<PathBuf> = match &shell.options.offered {
        Some(folders) => folders.clone(),
        None => {
            let paths = app.path();
            [paths.document_dir(), paths.picture_dir(), paths.video_dir(), paths.audio_dir(), paths.desktop_dir(),
             paths.download_dir()]
                .into_iter()
                .filter_map(Result::ok)
                .filter(|folder| folder.is_dir())
                .collect()
        }
    };
    json!({
        "offered": offered.iter().map(|folder| folder.display().to_string()).collect::<Vec<_>>(),
        "chosen": shell.folders(),
        "hotkey": shortcut_text(&shell.hotkey()),
        "model": shell.model_state.lock().unwrap().clone(),
        "auto": shell.options.debug_setup,
    })
}

/// Folders chosen in the system's folder picker (none when it's cancelled).
#[tauri::command]
async fn pick_folders(app: AppHandle) -> Vec<String> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.dialog().file().set_title("Add Folders").pick_folders(move |folders| {
        let _ = sender.send(folders);
    });
    let picked = receiver.await.ok().flatten().unwrap_or_default();
    picked.into_iter().filter_map(|folder| folder.into_path().ok()).map(|path| path.display().to_string()).collect()
}

/// "Code Folders…" (the tray, and the panel's `code:` hint): the folders picked become the code folders, which are
/// indexed for `code:` (a git repo's ignored files and secrets are left out). Cancelled: nothing changes.
#[tauri::command]
async fn choose_code_folders(app: AppHandle) -> Vec<String> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.dialog().file().set_title("Code Folders (these replace the ones chosen before)").pick_folders(move |folders| {
        let _ = sender.send(folders);
    });
    let picked: Vec<String> = receiver.await.ok().flatten().unwrap_or_default().into_iter()
        .filter_map(|folder| folder.into_path().ok()).map(|path| path.display().to_string()).collect();
    let shell = app.state::<Arc<Shell>>().inner().clone();
    if !picked.is_empty() {
        let saved = {
            let mut settings = shell.settings.lock().unwrap();
            settings.code_roots = picked;
            settings.code_index_off = false;
            settings.save(&shell.settings_path)
        };
        if let Err(error) = saved {
            set_status(&app, &format!("Can't save the settings: {error}"));
        }
        apply_folders(&app);
    }
    shell.code_roots()
}

/// The code folders indexed and searched: none while code indexing is off.
fn code_roots(options: &Options, settings: &Settings) -> Vec<String> {
    if settings.code_index_off {
        return Vec::new();
    }
    if !options.code_roots.is_empty() {
        return options.code_roots.iter().map(|root| root.display().to_string()).collect();
    }
    settings.code_roots.clone()
}

/// The code folders now and whether code indexing is off, for the panel's `code:` hint.
#[tauri::command]
fn code_folders(shell: State<'_, Arc<Shell>>) -> Value {
    json!({"folders": shell.code_roots(), "off": shell.settings.lock().unwrap().code_index_off})
}

/// "Index Code" (the tray, and the panel's `code:` hint): off, the code folders are no longer indexed, watched or
/// searched (code.sqlite stays, so turning it on again reads only what changed). Turned on from the tray with no code
/// folders yet, the folder picker opens.
#[tauri::command]
fn set_code_index(app: AppHandle, on: bool) -> Result<Vec<String>, String> {
    let shell = app.state::<Arc<Shell>>().inner().clone();
    {
        let mut settings = shell.settings.lock().unwrap();
        settings.code_index_off = !on;
        settings.save(&shell.settings_path).map_err(|error| format!("can't save the settings: {error}"))?;
    }
    if let Some(item) = shell.code_item.lock().unwrap().as_ref() {
        let _ = item.set_checked(on);
    }
    apply_folders(&app);
    Ok(shell.code_roots())
}

/// Setup's last step: the folders are saved and indexing starts (once the model is in).
#[tauri::command]
fn finish_setup(app: AppHandle, shell: State<'_, Arc<Shell>>, folders: Vec<String>) -> Result<(), String> {
    {
        let mut settings = shell.settings.lock().unwrap();
        settings.folders = folders;
        settings.set_up = true;
        settings.save(&shell.settings_path).map_err(|error| format!("can't save the settings: {error}"))?;
    }
    apply_folders(&app);
    if let Some(window) = app.get_webview_window("setup") {
        let _ = window.close();
    }
    Ok(())
}

/// "Index new files" (the panel's button and the tray's item): a sync, which queues only the files that are new or
/// changed since the last one (every other file keeps its vectors), then reads those.
#[tauri::command]
fn index_new_files(app: AppHandle) -> Result<(), String> {
    let shell = app.state::<Arc<Shell>>();
    if !shell.is_set_up() || shell.folders().is_empty() {
        return Err("Choose folders first".into());
    }
    if !shell.model_ready() {
        return Err("The model is still downloading".into());
    }
    shell.index.run(&shell.index_launch())
}

/// Opens a result in its default app, where it matched when the app can be told (open_at.rs).
#[tauri::command]
fn open_hit(app: AppHandle, hit: open_at::Hit) -> Result<(), String> {
    hide_panel(app.clone());
    if open_at::open(&hit)? {
        return Ok(());
    }
    app.opener().open_path(hit.path, None::<&str>).map_err(|error| error.to_string())
}

/// The result's app can be told where in it to open: the panel's button says "Open at p. 12".
#[tauri::command]
fn opens_at(hit: open_at::Hit) -> bool {
    open_at::opens_at(&hit)
}

/// `search:` in the panel, out of it: the page the panel shows (else Google's results for `query`) in SpotLite's bigger
/// browser window (`here`, Shift+Enter), or in the default browser (Ctrl+Enter, and where SpotLite has none of its own).
#[tauri::command]
fn open_web(app: AppHandle, query: String, here: bool) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    let url = browser::panel_url().unwrap_or_else(|| open_at::search_url(&query));
    #[cfg(not(target_os = "linux"))]
    let url = open_at::search_url(&query);
    hide_panel(app.clone());
    if here && open_at::open_here(&url)? || open_at::open_url(&url)? {
        return Ok(());
    }
    app.opener().open_url(url, None::<&str>).map_err(|error| error.to_string())
}

/// Where the panel's results area is, in its page's pixels.
#[derive(serde::Deserialize)]
struct Area {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// `search:` in the panel: Google's results for `query` over `area` (browser.rs); none: the web view goes.
#[tauri::command]
fn web_in_panel(app: AppHandle, query: Option<String>, area: Option<Area>) {
    #[cfg(target_os = "linux")]
    let _ = app.run_on_main_thread(move || match (query.filter(|q| !q.trim().is_empty()), area) {
        (Some(query), Some(area)) => browser::show_in_panel(&query, (area.x, area.y, area.width, area.height)),
        _ => browser::hide_in_panel(),
    });
    #[cfg(not(target_os = "linux"))]
    let _ = (app, query, area);
}

/// The default browser's name, for the panel's `search:` hint.
#[tauri::command]
fn web_browser() -> Option<String> {
    open_at::browser()
}

/// "Copy path" (Ctrl+Shift+C): the result's full path on the clipboard. SpotLite keeps running, so it stays there
/// after the panel goes.
#[tauri::command]
async fn copy_path(app: AppHandle, path: String) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        app.run_on_main_thread(move || {
            // Without a seat (a headless compositor) there's no clipboard, and GTK crashes copying to it.
            let seat = gtk::gdk::Display::default().and_then(|display| display.default_seat()).is_some();
            if seat {
                gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD).set_text(&path);
            }
            let _ = sender.send(seat);
        })
        .map_err(|error| error.to_string())?;
        return match receiver.await {
            Ok(true) => Ok(()),
            _ => Err("no clipboard to copy to (no keyboard or mouse)".into()),
        };
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (app, path);
        Err("copying isn't made for this system yet".into())
    }
}

/// Shows a result in the file manager, selected.
#[tauri::command]
fn reveal(app: AppHandle, path: String) -> Result<(), String> {
    hide_panel(app.clone());
    app.opener().reveal_item_in_dir(path).map_err(|error| error.to_string())
}

#[tauri::command]
fn hide_panel(app: AppHandle) {
    if let Some(panel) = app.get_webview_window("panel") {
        let _ = panel.hide();
    }
}

/// The panel grows to show results and shrinks back to its field.
#[tauri::command]
fn panel_height(app: AppHandle, height: f64) {
    if let Some(panel) = app.get_webview_window("panel") {
        fit_panel(&panel, height);
    }
}

/// The panel at this height (its field alone, or with results).
///
/// On Linux under Wayland the panel is a layer-shell surface (`layer_shell`), which takes any height it asks for. A
/// plain window there never changes size on COSMIC (a panel made 56 px tall never showed its results; one left at
/// GTK's natural 200 px cut them off), so without layer shell the window is always the panel's tallest, the page draws
/// the panel only as tall as it is (panel.css), and the rest is marked as shadow (window geometry) that lets clicks
/// through (input region). Elsewhere the window is resized.
fn fit_panel(panel: &tauri::WebviewWindow, height: f64) {
    let height = height.clamp(FIELD_HEIGHT, PANEL_TALLEST);
    #[cfg(target_os = "linux")]
    if let Ok(window) = panel.gtk_window() {
        use gtk::prelude::{GtkWindowExt, WidgetExt};
        if layer_shell::is_on() {
            window.set_size_request(PANEL_WIDTH as i32, height as i32);
            window.resize(PANEL_WIDTH as i32, height as i32);
            return;
        }
        window.set_size_request(PANEL_WIDTH as i32, PANEL_TALLEST as i32);
        if let Some(surface) = window.window() {
            surface.set_shadow_width(0, 0, 0, (PANEL_TALLEST - height) as i32);
            let shown = gtk::cairo::RectangleInt::new(0, 0, PANEL_WIDTH as i32, height as i32);
            surface.input_shape_combine_region(&gtk::cairo::Region::create_rectangle(&shown), 0, 0);
        }
        return;
    }
    let _ = panel.set_size(LogicalSize::new(PANEL_WIDTH, height));
}

/// Debug runs: the query the panel types in when it loads (with setup, it comes later as a "debug-query" event).
#[tauri::command]
fn debug_query(shell: State<'_, Arc<Shell>>) -> Option<String> {
    if shell.options.debug_setup {
        return None;
    }
    shell.options.debug_query.clone()
}

/// Debug runs: whether the panel should go through its states and report their sizes.
#[tauri::command]
fn debug_sizes(shell: State<'_, Arc<Shell>>) -> Option<String> {
    shell.options.debug_sizes.clone()
}

/// Debug runs: whether the panel should click "New files" by itself.
#[tauri::command]
fn debug_index_new(shell: State<'_, Arc<Shell>>) -> bool {
    shell.options.debug_index_new
}

/// Debug runs: the panel hidden and shown again, as the hotkey does it.
#[tauri::command]
fn debug_reshow(app: AppHandle) {
    hide_panel(app.clone());
    show_panel(&app);
}

/// Debug runs: what the page showed for the debug query, printed (and the shell quits, if asked). A state's report
/// gets whether `search:`'s web view really shows (GTK's word, not the page's).
#[tauri::command]
fn debug_report(app: AppHandle, shell: State<'_, Arc<Shell>>, mut report: Value) {
    #[cfg(target_os = "linux")]
    if let Some(fields) = report.as_object_mut().filter(|fields| fields.contains_key("case")) {
        fields.insert("web_view".into(), browser::panel_url().is_some().into());
    }
    println!("{report}");
    if shell.options.debug_quit {
        quit(&app);
    }
}

fn main() {
    let options = match Options::from_args() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("spotlite-app: {error}");
            std::process::exit(2);
        }
    };
    // `search:`'s browser: a process of its own, before (and without) the app and its single-instance check.
    #[cfg(target_os = "linux")]
    if let Some(url) = &options.browser {
        browser::run(url);
        return;
    }
    let app = tauri::Builder::default()
        // Launched again (a desktop shortcut, where the global hotkey can't reach: Wayland): the panel comes or goes.
        .plugin(tauri_plugin_single_instance::init(|app, _, _| toggle_panel(app)))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _, event| {
                    if event.state == ShortcutState::Pressed {
                        toggle_panel(app);
                    }
                })
                .build(),
        )
        .setup(move |app| setup(app, options))
        .invoke_handler(tauri::generate_handler![
            ask, estimate, setup_info, pick_folders, finish_setup, index_new_files, choose_code_folders, code_folders,
            set_code_index, copy_path, open_hit, opens_at, open_web,
            web_in_panel, web_browser, reveal, hide_panel,
            panel_height, debug_query, debug_index_new, debug_sizes, debug_reshow, debug_report
        ])
        .build(tauri::generate_context!())
        .expect("SpotLite couldn't start");
    app.run(|app, event| match event {
        tauri::RunEvent::Exit => stop_helpers(app),
        // Closing the setup window isn't quitting: SpotLite lives in the tray.
        tauri::RunEvent::ExitRequested { code: None, api, .. } => api.prevent_exit(),
        _ => {}
    });
}

fn setup(app: &mut tauri::App, options: Options) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Accessory); // the menu bar only, no Dock icon

    let handle = app.handle().clone();
    let logs = app.path().app_log_dir()?;
    std::fs::create_dir_all(&logs)?;
    let settings_path = match &options.settings {
        Some(path) => path.clone(),
        None => app.path().app_config_dir()?.join("settings.json"),
    };
    let settings = Settings::load(&settings_path);
    let roots: Vec<String> = if options.roots.is_empty() {
        settings.folders.clone()
    } else {
        options.roots.iter().map(|root| root.display().to_string()).collect()
    };
    let mut search_args = vec!["search-service".to_string()];
    search_args.extend(options.index.iter().flat_map(|index| ["--index".to_string(), index.display().to_string()]));
    for root in code_roots(&options, &settings) {
        search_args.extend(["--code-root".to_string(), root]);
    }
    search_args.extend(roots);
    let search = SearchService::new(
        Launch {
            program: options.helper.clone(),
            args: search_args,
            env: options.models.iter().map(|models| ("SPOTLITE_MODELS".to_string(), models.display().to_string())).collect(),
            log: Some(logs.join("search.log")),
            background: false,   // it answers typing
        },
        {
            let app = handle.clone();
            move |event| search_event(&app, event)
        },
    );
    let index = IndexService::new({
        let app = handle.clone();
        move |event| index_event(&app, event)
    });
    let model = ModelDownload::new({
        let app = handle.clone();
        move |event| model_event(&app, event)
    });
    // Previews show files from the chosen folders (and the helper's renders, once it says where the index is).
    for root in &settings.folders {
        let _ = app.asset_protocol_scope().allow_directory(root, true);
    }
    for root in &options.roots {
        let _ = app.asset_protocol_scope().allow_directory(root, true);
    }
    let shell = Arc::new(Shell {
        options,
        settings: Mutex::new(settings),
        settings_path,
        search,
        index,
        model,
        model_state: Mutex::new(json!({"state": "checking"})),
        watcher: Mutex::new(None),
        status: Mutex::new(None),
        code_item: Mutex::new(None),
        index_item: Mutex::new(None),
        reloaded: Mutex::new(Instant::now()),
        debug_query_sent: AtomicBool::new(false),
        logs,
    });
    app.manage(shell.clone());

    make_panel(&handle)?;
    // No tray (a session without a taskbar, a Linux desktop without one): the hotkey and the panel still work.
    if let Err(error) = make_tray(&handle, &shell) {
        eprintln!("spotlite-app: no tray icon: {error}");
    }
    let hotkey = shell.hotkey();
    if let Err(error) = app.global_shortcut().register(hotkey.as_str()) {
        set_status(&handle, &format!("The shortcut {} is taken", shortcut_text(&hotkey)));
        eprintln!("spotlite-app: can't use {hotkey} as the shortcut: {error}");
    }

    // The search service starts now (the first search is then quick); the model is checked (and fetched if it's
    // missing: on first launch that runs while setup is open, as on the Mac); indexing waits for both.
    let warm = shell.clone();
    tauri::async_runtime::spawn(async move {
        let _ = warm.search.ask(json!({"op": "status"})).await;
    });
    let mut download = vec!["download-model".to_string()];
    if let Some(models) = &shell.options.models {
        download.extend(["--to".to_string(), models.display().to_string()]);
    }
    if let Err(error) = shell.model.run(&shell.launch(download, "model.log")) {
        model_event(&handle, json!({"event": "error", "message": error}));
    }
    if shell.is_set_up() {
        watch_folders(&handle);
    } else {
        show_setup(&handle)?;
    }
    if shell.options.debug_show || shell.options.panel {
        show_panel(&handle);
    }
    Ok(())
}

/// After setup or a change of folders: the search service, the watcher and indexing follow the new folders.
fn apply_folders(app: &AppHandle) {
    let shell = app.state::<Arc<Shell>>().inner().clone();
    let folders = shell.folders();
    let (skipped, types) = {
        let settings = shell.settings.lock().unwrap();
        (settings.skipped.clone(), settings.skip_types.clone())
    };
    let code_roots = shell.code_roots();
    for folder in &folders {
        let _ = app.asset_protocol_scope().allow_directory(folder, true);
    }
    let search = shell.clone();
    tauri::async_runtime::spawn(async move {
        let request = json!({"op": "folders", "roots": folders, "skipped": skipped, "types": types, "code_roots": code_roots});
        let _ = search.search.ask(request).await;
    });
    watch_folders(app);
}

/// Watches the folders, and indexes them now when the model is in.
fn watch_folders(app: &AppHandle) {
    let shell = app.state::<Arc<Shell>>().inner().clone();
    let folders: Vec<PathBuf> = shell.folders().into_iter().chain(shell.code_roots()).map(PathBuf::from).collect();
    if folders.is_empty() {
        set_status(app, "No folders chosen yet");
        return;
    }
    let changed = {
        let app = app.clone();
        move || run_index(&app)
    };
    *shell.watcher.lock().unwrap() = Some(FolderWatcher::start(&folders, shell.options.index_folder(), changed));
    run_index(app);
}

/// Has the index helper sync and index, once there's a model and a folder.
fn run_index(app: &AppHandle) {
    let shell = app.state::<Arc<Shell>>();
    if !shell.is_set_up() || shell.folders().is_empty() || !shell.model_ready() {
        return;
    }
    if let Err(error) = shell.index.run(&shell.index_launch()) {
        set_status(app, &format!("Indexing couldn't start: {error}"));
    }
}

fn stop_helpers(app: &AppHandle) {
    let shell = app.state::<Arc<Shell>>();
    shell.model.stop();
    shell.index.stop();
    shell.search.stop();
}

fn quit(app: &AppHandle) {
    stop_helpers(app);
    app.exit(0);
}

// MARK: Windows

fn make_panel(app: &AppHandle) -> tauri::Result<()> {
    let panel = WebviewWindowBuilder::new(app, "panel", WebviewUrl::App("index.html".into()))
        .title("SpotLite")
        .inner_size(PANEL_WIDTH, if cfg!(target_os = "linux") { PANEL_TALLEST } else { FIELD_HEIGHT })
        .decorations(false)
        .transparent(true)
        .shadow(true)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        .skip_taskbar(true)
        .visible(false)
        .build()?;
    #[cfg(target_os = "linux")]
    if let Ok(window) = panel.gtk_window() {
        layer_shell::make_overlay(&window);
        // `search:`'s web view goes over the results area (browser.rs). Esc in it hides the panel; Ctrl+L gives
        // the keys back to the query field.
        let (escape, field) = (app.clone(), app.clone());
        browser::attach(&window, move || hide_panel(escape.clone()), move || {
            let _ = field.emit("focus-field", ());
        });
        // A layer-shell surface is never "active", so Tauri reports no Focused(false); GTK still sees the keyboard go.
        if layer_shell::is_on() {
            use gtk::prelude::WidgetExt;
            let app = app.clone();
            window.connect_focus_out_event(move |_, _| {
                if !app.state::<Arc<Shell>>().options.debug_show {
                    hide_panel(app.clone());
                }
                gtk::glib::Propagation::Proceed
            });
        }
    }
    fit_panel(&panel, FIELD_HEIGHT);
    let app = app.clone();
    panel.on_window_event(move |event| {
        // Like Spotlight: it steps away when you click elsewhere.
        if let WindowEvent::Focused(false) = event {
            if !app.state::<Arc<Shell>>().options.debug_show {
                hide_panel(app.clone());
            }
        }
    });
    Ok(())
}

fn show_panel(app: &AppHandle) {
    if !app.state::<Arc<Shell>>().is_set_up() {
        let _ = show_setup(app);
        return;
    }
    let Some(panel) = app.get_webview_window("panel") else { return };
    // A layer-shell panel goes on the screen in use (the compositor picks it), a fifth of the way down.
    // ponytail: the height is the panel's last screen (or the first); with screens of different heights it can sit a
    // little off a fifth until gtk-layer-shell is told the screen (gtk_layer_set_monitor).
    #[cfg(target_os = "linux")]
    if let Ok(window) = panel.gtk_window() {
        if layer_shell::is_on() {
            let screen = panel.current_monitor().ok().flatten().or_else(|| app.primary_monitor().ok().flatten());
            let height = screen.map_or(1000.0, |screen| screen.size().height as f64 / screen.scale_factor());
            layer_shell::set_top(&window, (height * 0.2) as i32);
        }
    }
    // On the screen with the pointer, centered, a fifth of the way down.
    if let Ok(pointer) = app.cursor_position() {
        if let Ok(Some(screen)) = app.monitor_from_point(pointer.x, pointer.y) {
            let width = (PANEL_WIDTH * screen.scale_factor()) as i32;
            let x = screen.position().x + (screen.size().width as i32 - width) / 2;
            let y = screen.position().y + (screen.size().height as f64 * 0.2) as i32;
            let _ = panel.set_position(PhysicalPosition::new(x, y));
        }
    }
    let _ = panel.show();
    let _ = panel.set_focus();
    let _ = panel.emit("panel-shown", ());
}

fn toggle_panel(app: &AppHandle) {
    match app.get_webview_window("panel") {
        Some(panel) if panel.is_visible().unwrap_or(false) => hide_panel(app.clone()),
        _ => show_panel(app),
    }
}

/// The setup window (first launch, and Choose Folders… later). Debug runs that click through it keep it hidden.
fn show_setup(app: &AppHandle) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window("setup") {
        let _ = window.show();
        let _ = window.set_focus();
        return Ok(());
    }
    let hidden = app.state::<Arc<Shell>>().options.debug_setup;
    WebviewWindowBuilder::new(app, "setup", WebviewUrl::App("setup.html".into()))
        .title("Set Up SpotLite")
        .inner_size(720.0, 600.0)
        .resizable(false)
        .maximizable(false)
        .center()
        .visible(!hidden)
        .build()?;
    Ok(())
}

/// "CommandOrControl+Shift+Space" as people read it here: "Ctrl+Shift+Space", or "⇧⌘Space" on a Mac.
fn shortcut_text(accelerator: &str) -> String {
    let parts: Vec<&str> = accelerator.split('+').collect();
    if cfg!(target_os = "macos") {
        let mut text = String::new();
        for (name, sign) in [("Ctrl", "⌃"), ("Control", "⌃"), ("Alt", "⌥"), ("Option", "⌥"), ("Shift", "⇧"),
                             ("CommandOrControl", "⌘"), ("CmdOrCtrl", "⌘"), ("Cmd", "⌘"), ("Command", "⌘"),
                             ("Super", "⌘")] {
            if parts.iter().any(|part| part.eq_ignore_ascii_case(name)) && !text.contains(sign) {
                text.push_str(sign);
            }
        }
        return text + parts.last().copied().unwrap_or_default();
    }
    parts
        .iter()
        .map(|part| match *part {
            "CommandOrControl" | "CmdOrCtrl" | "Control" => "Ctrl",
            "Super" => "Win",
            other => other,
        })
        .collect::<Vec<_>>()
        .join("+")
}

// MARK: The tray

fn make_tray(app: &AppHandle, shell: &Arc<Shell>) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, "status", "Starting…", false, None::<&str>)?;
    let search = MenuItem::with_id(app, "search", "Search…", true, Some(shell.hotkey().as_str()))?;
    let folders = MenuItem::with_id(app, "folders", "Choose Folders…", true, None::<&str>)?;
    let code = MenuItem::with_id(app, "code", "Code Folders…", true, None::<&str>)?;
    let code_on = !shell.settings.lock().unwrap().code_index_off;
    let code_index = CheckMenuItem::with_id(app, "code-index", "Index Code", true, code_on, None::<&str>)?;
    let index = MenuItem::with_id(app, "index", "Index New Files", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit SpotLite", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[&status, &separator, &search, &folders, &code, &code_index, &index, &PredefinedMenuItem::separator(app)?, &quit_item],
    )?;
    // A Mac's menu bar takes a template (it colors it); trays elsewhere show the app's own small icon.
    #[cfg(target_os = "macos")]
    let icon = tauri::include_image!("icons/tray-template.png");
    #[cfg(not(target_os = "macos"))]
    let icon = tauri::include_image!("icons/tray.png");
    TrayIconBuilder::with_id("spotlite")
        .icon(icon)
        .icon_as_template(cfg!(target_os = "macos"))
        .tooltip("SpotLite")
        .menu(&menu)
        .show_menu_on_left_click(cfg!(target_os = "macos"))
        .on_menu_event(|app, event| match event.id().as_ref() {
            "search" => show_panel(app),
            "folders" => {
                let _ = show_setup(app);
            }
            "code" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move { choose_code_folders(app).await });
            }
            "code-index" => {
                let shell = app.state::<Arc<Shell>>().inner().clone();
                let on = shell.settings.lock().unwrap().code_index_off;   // it was off: now on
                match set_code_index(app.clone(), on) {
                    Ok(folders) if on && folders.is_empty() => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move { choose_code_folders(app).await });
                    }
                    Ok(_) => {}
                    Err(error) => set_status(app, &error),
                }
            }
            "index" => {
                if let Err(error) = index_new_files(app.clone()) {
                    set_status(app, &error);
                }
            }
            "quit" => quit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // Windows and Linux: a click opens the panel, the menu is on the other button.
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                if !cfg!(target_os = "macos") {
                    toggle_panel(tray.app_handle());
                }
            }
        })
        .build(app)?;
    *shell.status.lock().unwrap() = Some(status);
    *shell.code_item.lock().unwrap() = Some(code_index);
    *shell.index_item.lock().unwrap() = Some(index);
    Ok(())
}

/// While files are being read, "Index New Files" is greyed out, as the panel's button is: the watcher already syncs new
/// files, and a sync asked for now only waits behind the file being read, so the click looked like it did nothing.
fn enable_index_item(app: &AppHandle, enabled: bool) {
    if let Some(item) = app.state::<Arc<Shell>>().index_item.lock().unwrap().as_ref() {
        let _ = item.set_enabled(enabled);
    }
}

fn set_status(app: &AppHandle, text: &str) {
    if let Some(status) = app.state::<Arc<Shell>>().status.lock().unwrap().as_ref() {
        let _ = status.set_text(text);
    }
    let _ = app.emit("status", text);
}

// MARK: The helpers' events

fn search_event(app: &AppHandle, event: Value) {
    if cfg!(debug_assertions) {
        println!("search: {event}");
    }
    if event["event"] == "ready" {
        if let Some(index) = event["index"].as_str() {
            let _ = app.asset_protocol_scope().allow_directory(Path::new(index).join("previews"), false);
        }
    }
    let _ = app.emit("helper", json!({"from": "search", "event": event}));
}

fn index_event(app: &AppHandle, event: Value) {
    if cfg!(debug_assertions) {
        println!("index: {event}");
    }
    let count = |key: &str| event[key].as_u64().unwrap_or(0);
    match event["event"].as_str().unwrap_or_default() {
        "file" => {
            set_status(app, &format!("Indexing {} of {}", count("done"), count("total")));
            enable_index_item(app, false);
            reload(app, false);
        }
        "rest" => {
            set_status(app, &format!("Reading the rest of {} long files", count("files")));
            enable_index_item(app, false);
            reload(app, false);
        }
        "idle" => {
            set_status(app, &format!("Up to date · {} files", count("searchable")));
            enable_index_item(app, true);
            reload(app, true);
            // A debug run through setup searches once the first index is in.
            let shell = app.state::<Arc<Shell>>();
            if let Some(query) = &shell.options.debug_query {
                if shell.options.debug_setup && !shell.debug_query_sent.swap(true, Ordering::SeqCst) {
                    let _ = app.emit_to("panel", "debug-query", query.clone());
                }
            }
        }
        "waiting" => {
            set_status(app, "Waiting for the model");
            enable_index_item(app, true);
        }
        "error" => set_status(app, &format!("Indexing: {}", event["message"].as_str().unwrap_or("error"))),
        "exit" => {
            enable_index_item(app, true);
            reload(app, true);
            // The watchdog ended the helper on a file it couldn't read (it left a note): a new one marks that file and
            // goes on.
            let shell = app.state::<Arc<Shell>>();
            if shell.options.index_folder().is_some_and(|folder| folder.join("stopped-file.json").exists()) {
                run_index(app);
            }
        }
        _ => {}
    }
    let _ = app.emit("helper", json!({"from": "index", "event": event}));
}

fn model_event(app: &AppHandle, event: Value) {
    if cfg!(debug_assertions) && event["event"] != "progress" {
        println!("model: {event}");
    }
    let shell = app.state::<Arc<Shell>>().inner().clone();
    let state = match event["event"].as_str().unwrap_or_default() {
        "progress" => {
            let (bytes, total) = (event["all_bytes"].as_f64().unwrap_or(0.0), event["all_total"].as_f64().unwrap_or(1.0));
            set_status(app, &format!("Downloading the model · {:.0}%", 100.0 * bytes / total.max(1.0)));
            json!({"state": "downloading", "bytes": bytes, "total": total})
        }
        "file" => json!({"state": "downloading", "from": event["from"]}),
        "verifying" => json!({"state": "checking"}),
        "waiting" => json!({"state": "waiting"}),
        "retry" => json!({"state": "downloading", "problem": event["message"]}),
        "done" => json!({"state": "ready"}),
        "error" => {
            set_status(app, "The model couldn't download");
            json!({"state": "failed", "message": event["message"]})
        }
        _ => return,
    };
    let ready = state["state"] == "ready";
    *shell.model_state.lock().unwrap() = state.clone();
    let _ = app.emit("model", state);
    if ready {
        if shell.folders().is_empty() {
            set_status(app, "No folders chosen yet");
        }
        run_index(app);
    }
}

/// Has the search service read the index again: at once when indexing settles, else at most every three seconds.
fn reload(app: &AppHandle, now: bool) {
    let shell = app.state::<Arc<Shell>>().inner().clone();
    {
        let mut reloaded = shell.reloaded.lock().unwrap();
        if !now && reloaded.elapsed() < Duration::from_secs(3) {
            return;
        }
        *reloaded = Instant::now();
    }
    tauri::async_runtime::spawn(async move {
        let reply = shell.search.ask(json!({"op": "reload"})).await;
        if cfg!(debug_assertions) {
            println!("search: reload {reply:?}");
        }
    });
}

/// The panel as a layer-shell surface (wlr-layer-shell, what launchers use on COSMIC, KDE, Sway and Hyprland): the
/// compositor gives it the height it asks for and draws no window border. Through gtk-layer-shell, loaded when the
/// shell runs: the copy next to the program (install.sh), the system's, or Vendor's (scripts/fetch-layer-shell.sh)
/// for a build run from the repo. Without it, or on X11 or GNOME, the panel stays a plain window.
#[cfg(target_os = "linux")]
mod layer_shell {
    use gtk::glib::object::ObjectType;
    use gtk::prelude::WidgetExt;
    use std::ffi::{c_char, c_int, c_void, CStr, CString};
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::OnceLock;

    const NAME: &str = "libgtk-layer-shell.so.0";
    const LAYER_OVERLAY: c_int = 3;
    const EDGE_TOP: c_int = 2;
    const KEYBOARD_ON_DEMAND: c_int = 2;

    static ON: AtomicBool = AtomicBool::new(false);

    struct Api {
        init_for_window: unsafe extern "C" fn(*mut c_void),
        set_namespace: unsafe extern "C" fn(*mut c_void, *const c_char),
        set_layer: unsafe extern "C" fn(*mut c_void, c_int),
        set_anchor: unsafe extern "C" fn(*mut c_void, c_int, c_int),
        set_margin: unsafe extern "C" fn(*mut c_void, c_int, c_int),
        set_keyboard_mode: unsafe extern "C" fn(*mut c_void, c_int),
    }

    fn api() -> Option<&'static Api> {
        static API: OnceLock<Option<Api>> = OnceLock::new();
        API.get_or_init(|| unsafe {
            let beside = std::env::current_exe().ok()?.with_file_name(NAME);
            // Beside the app (an install), the system's, or a dev build's (app/src-tauri/build.rs).
            let vendor = option_env!("SPOTLITE_DEV_VENDOR").map(|vendor| PathBuf::from(vendor).join("gtk-layer-shell").join(NAME));
            let library = [Some(beside), Some(PathBuf::from(NAME)), vendor].into_iter().flatten().find_map(|path| {
                let path = CString::new(path.into_os_string().into_vec()).ok()?;
                let library = libc::dlopen(path.as_ptr(), libc::RTLD_NOW);
                (!library.is_null()).then_some(library)
            })?;
            let symbol = |name: &CStr| {
                let symbol = libc::dlsym(library, name.as_ptr());
                (!symbol.is_null()).then_some(symbol)
            };
            let is_supported: unsafe extern "C" fn() -> c_int = std::mem::transmute(symbol(c"gtk_layer_is_supported")?);
            if is_supported() == 0 {
                return None;
            }
            Some(Api {
                init_for_window: std::mem::transmute(symbol(c"gtk_layer_init_for_window")?),
                set_namespace: std::mem::transmute(symbol(c"gtk_layer_set_namespace")?),
                set_layer: std::mem::transmute(symbol(c"gtk_layer_set_layer")?),
                set_anchor: std::mem::transmute(symbol(c"gtk_layer_set_anchor")?),
                set_margin: std::mem::transmute(symbol(c"gtk_layer_set_margin")?),
                set_keyboard_mode: std::mem::transmute(symbol(c"gtk_layer_set_keyboard_mode")?),
            })
        })
        .as_ref()
    }

    /// Makes the (hidden) panel an overlay at the top of the screen that takes the keyboard when shown.
    pub fn make_overlay(window: &gtk::ApplicationWindow) {
        let Some(api) = api() else { return };
        // gtk-layer-shell has to come before the window is realized, and Tauri realizes it when it's built.
        if window.is_realized() {
            window.unrealize();
        }
        let window = window.as_ptr() as *mut c_void;
        unsafe {
            (api.init_for_window)(window);
            (api.set_namespace)(window, c"spotlite".as_ptr());
            (api.set_layer)(window, LAYER_OVERLAY);
            (api.set_anchor)(window, EDGE_TOP, 1);
            (api.set_keyboard_mode)(window, KEYBOARD_ON_DEMAND);
        }
        ON.store(true, Ordering::Relaxed);
    }

    pub fn is_on() -> bool {
        ON.load(Ordering::Relaxed)
    }

    /// How far down the screen the panel sits, in logical pixels.
    pub fn set_top(window: &gtk::ApplicationWindow, margin: i32) {
        if let Some(api) = api() {
            unsafe { (api.set_margin)(window.as_ptr() as *mut c_void, EDGE_TOP, margin) };
        }
    }
}
