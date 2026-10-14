// Opening a result where it matched, in the app the desktop opens that kind of file with (Linux): a PDF at its page
// (with a word found there, for the viewer to find), a recording at its moment, code at its line, when that app can
// be told where (the apps in `command`); any other app opens the file from the start. And `search:` in the panel:
// Google in the default browser, a Chromium browser in its main profile (the first in its profile list).
// Elsewhere these say they didn't open anything, and the shell opens the file with the system's opener.

use serde::Deserialize;

/// A result, as the panel sends it.
#[derive(Deserialize)]
pub struct Hit {
    pub path: String,
    pub segment: String,
    pub loc: Option<f64>,
    pub focus: Option<f64>,
    /// A word found on the PDF page, for the viewer to find and highlight.
    pub find: Option<String>,
}

#[cfg(not(target_os = "linux"))]
pub fn open(_hit: &Hit) -> Result<bool, String> {
    Ok(false)
}

#[cfg(not(target_os = "linux"))]
pub fn opens_at(_hit: &Hit) -> bool {
    false
}

/// Google's results for `query`, in the system's language (`hl`): Google otherwise goes by where you are, and a
/// browser's language list doesn't change that.
pub fn search_url(query: &str) -> String {
    let mut parameters = vec![("q", query.to_string())];
    parameters.extend(system_language().map(|language| ("hl", language)));
    tauri::Url::parse_with_params("https://www.google.com/search", &parameters).map(String::from)
        .unwrap_or_else(|_| "https://www.google.com/".into())
}

/// "en" for LANG=en_US.UTF-8 (LC_ALL and LC_MESSAGES first, as for messages); none for C or POSIX.
fn system_language() -> Option<String> {
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"].iter().find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))?;
    let language = locale.split(['_', '.', '@']).next()?.to_lowercase();
    (language.len() >= 2 && language.chars().all(|c| c.is_ascii_alphabetic()) && language != "posix").then_some(language)
}

#[cfg(not(target_os = "linux"))]
pub fn open_url(_url: &str) -> Result<bool, String> {
    Ok(false)
}

#[cfg(not(target_os = "linux"))]
pub fn open_here(_url: &str) -> Result<bool, String> {
    Ok(false)
}

#[cfg(not(target_os = "linux"))]
pub fn browser() -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "linux")]
mod linux {
    use super::Hit;
    use gtk::gio;
    use gtk::gio::prelude::AppInfoExt;
    use std::io::Read;
    use std::path::Path;

    /// Where in the file.
    pub enum Place {
        Page(u32, Option<String>),
        Moment(f64),
        Line(u32),
    }

    fn place(hit: &Hit) -> Option<Place> {
        let loc = hit.loc?;
        match hit.segment.as_str() {
            "page" | "page_image" if loc >= 1.0 => Some(Place::Page(loc as u32, hit.find.clone())),
            // Like DigUp's Mac app: a moment under a second opens from the start.
            "frame" | "audio" if loc >= 1.0 => Some(Place::Moment(loc)),
            "lines" => Some(Place::Line(hit.focus.unwrap_or(loc).max(1.0) as u32)),
            _ => None,
        }
    }

    /// Opens `hit` in its default app, at its place when the app can be told. False: no app for it.
    pub fn open(hit: &Hit) -> Result<bool, String> {
        let Some((name, exec)) = app_for(&hit.path) else { return Ok(false) };
        let Some((argv, told)) = command(&exec, &hit.path, place(hit).as_ref()) else { return Ok(false) };
        eprintln!("open: {} in {name}{}: {argv:?}", hit.path, if told { " at its place" } else { "" });
        spawn(&argv).map(|_| true)
    }

    /// The default app for `hit` can be told where in it to open.
    pub fn opens_at(hit: &Hit) -> bool {
        let Some(place) = place(hit) else { return false };
        app_for(&hit.path).and_then(|(_, exec)| command(&exec, &hit.path, Some(&place))).is_some_and(|(_, told)| told)
    }

    /// Opens `url` in the default browser (a Chromium browser in its main profile). False: no browser.
    pub fn open_url(url: &str) -> Result<bool, String> {
        let Some(app) = gio::AppInfo::default_for_uri_scheme("https") else { return Ok(false) };
        let Some(exec) = app.commandline() else { return Ok(false) };
        let config = gtk::glib::user_config_dir();
        let Some(argv) = web_command(&exec.to_string_lossy(), url, &config) else { return Ok(false) };
        eprintln!("open in {}: {argv:?}", app.name());
        spawn(&argv).map(|_| true)
    }

    /// Opens `url` in SpotLite's own browser window (browser.rs), a process of its own.
    pub fn open_here(url: &str) -> Result<bool, String> {
        let program = std::env::current_exe().map_err(|error| error.to_string())?;
        let argv = [program.display().to_string(), "--browser".into(), url.to_string()];
        eprintln!("search the web here: {argv:?}");
        spawn(&argv).map(|_| true)
    }

    /// The default browser's name ("Google Chrome").
    pub fn browser() -> Option<String> {
        gio::AppInfo::default_for_uri_scheme("https").map(|app| app.name().to_string())
    }

    /// The default app for the file: its name and its desktop entry's command line.
    fn app_for(path: &str) -> Option<(String, String)> {
        let mut head = Vec::new();
        if let Ok(file) = std::fs::File::open(path) {
            let _ = file.take(4096).read_to_end(&mut head);
        }
        let (kind, _) = gio::content_type_guess(Some(Path::new(path)), &head);
        let app = gio::AppInfo::default_for_type(&kind, false)?;
        Some((app.name().to_string(), app.commandline()?.to_string_lossy().into_owned()))
    }

    /// The desktop entry's command line as words, without its field codes (%U and the like, and flatpak's @@).
    fn words(exec: &str) -> Option<Vec<String>> {
        const FIELD_CODES: [&str; 16] =
            ["%f", "%F", "%u", "%U", "%d", "%D", "%n", "%N", "%i", "%c", "%k", "%v", "%m", "@@", "@@u", "@@a"];
        let mut argv: Vec<String> = gtk::glib::shell_parse_argv(exec).ok()?.into_iter()
            .map(|word| word.to_string_lossy().into_owned()).collect();
        argv.retain(|word| !FIELD_CODES.contains(&word.as_str()));
        (!argv.is_empty()).then_some(argv)
    }

    /// The program the command line runs: "evince" for /usr/bin/evince, and for a flatpak, its --command.
    fn program(argv: &[String]) -> String {
        let base = |word: &str| word.rsplit('/').next().unwrap_or(word).to_lowercase();
        if base(&argv[0]) == "flatpak" {
            return argv.iter().find_map(|word| word.strip_prefix("--command=")).map(base).unwrap_or_default();
        }
        base(&argv[0])
    }

    /// The command that opens `path` at `place` in the app with the command line `exec`, and whether the app is told
    /// the place (else it opens the file from the start).
    pub fn command(exec: &str, path: &str, place: Option<&Place>) -> Option<(Vec<String>, bool)> {
        let mut argv = words(exec)?;
        let name = program(&argv);
        let is = |names: &[&str]| names.iter().any(|start| name.starts_with(start));
        let mut target = path.to_string();
        let mut options: Vec<String> = Vec::new();
        let told = match place {
            Some(Place::Page(page, find)) if is(&["evince", "papers", "atril", "xreader"]) => {
                options.push(format!("--page-index={page}"));
                options.extend(find.iter().map(|word| format!("--find={word}")));
                true
            }
            Some(Place::Page(page, find)) if is(&["okular"]) => {
                options.extend(["--page".to_string(), page.to_string()]);
                options.extend(find.iter().flat_map(|word| ["--find".to_string(), word.clone()]));
                true
            }
            Some(Place::Page(page, find)) if is(&["zathura"]) => {
                options.push(format!("--page={page}"));
                options.extend(find.iter().map(|word| format!("--find={word}")));
                true
            }
            Some(Place::Page(page, _)) if is_browser(&name) => {
                target = format!("{}#page={page}", tauri::Url::from_file_path(path).ok()?);
                true
            }
            // Whole seconds: a decimal point would depend on the app's locale.
            Some(Place::Moment(seconds)) if is(&["vlc"]) => {
                options.push(format!("--start-time={}", seconds.floor()));
                true
            }
            Some(Place::Moment(seconds)) if is(&["mpv"]) => {
                options.push(format!("--start={}", seconds.floor()));
                true
            }
            Some(Place::Moment(seconds)) if is(&["celluloid"]) => {
                options.push(format!("--mpv-start={}", seconds.floor()));
                true
            }
            Some(Place::Line(line)) if is(&["code", "codium", "vscodium", "cursor", "windsurf"]) => {
                options.push("-g".into());
                target = format!("{path}:{line}");
                true
            }
            Some(Place::Line(line)) if is(&["gedit", "gnome-text-editor", "pluma", "xed"]) => {
                options.push(format!("+{line}"));
                true
            }
            Some(Place::Line(line)) if is(&["kate", "kwrite"]) => {
                options.extend(["--line".to_string(), line.to_string()]);
                true
            }
            Some(Place::Line(line)) if is(&["zed", "subl", "sublime_text"]) => {
                target = format!("{path}:{line}");
                true
            }
            _ => false,
        };
        // Before a "--" in the command line (mpv's has one): after it, an option would be taken for a file.
        let at = argv.iter().position(|word| word == "--").unwrap_or(argv.len());
        argv.splice(at..at, options);
        argv.push(target);
        Some((argv, told))
    }

    fn is_browser(name: &str) -> bool {
        ["google-chrome", "chrom", "brave", "microsoft-edge", "vivaldi", "opera", "firefox", "librewolf", "waterfox"]
            .iter().any(|start| name.starts_with(start))
    }

    /// The command that opens `url` in the browser with the command line `exec`; a Chromium browser in its main
    /// profile, found in `config` (~/.config).
    pub fn web_command(exec: &str, url: &str, config: &Path) -> Option<Vec<String>> {
        let mut argv = words(exec)?;
        let name = program(&argv);
        let folders = [("google-chrome-beta", "google-chrome-beta"), ("google-chrome-unstable", "google-chrome-unstable"),
                       ("google-chrome", "google-chrome"), ("chromium", "chromium"), ("brave", "BraveSoftware/Brave-Browser"),
                       ("microsoft-edge", "microsoft-edge"), ("vivaldi", "vivaldi")];
        let folder = folders.iter().find(|(start, _)| name.starts_with(start)).map(|(_, folder)| config.join(folder));
        if let Some(profile) = folder.and_then(|folder| main_profile(&folder)) {
            argv.push(format!("--profile-directory={profile}"));
        }
        argv.push(url.to_string());
        Some(argv)
    }

    /// A Chromium browser's main profile: the first in its profile list ("Default" in a new browser; one whose first
    /// profile was deleted has none, and naming it would make a new, empty profile). None: the browser picks.
    fn main_profile(folder: &Path) -> Option<String> {
        let state: serde_json::Value = serde_json::from_slice(&std::fs::read(folder.join("Local State")).ok()?).ok()?;
        let listed = state["profile"]["profiles_order"].as_array().into_iter().flatten().filter_map(|name| name.as_str());
        let profile = listed.chain(["Default"]).find(|name| folder.join(name).is_dir()).map(String::from);
        profile
    }

    /// Starts the app on its own: in its own process group, and waited for, so it leaves no zombie when it exits
    /// (a browser or a single-window app hands the file to the copy already running and exits at once).
    fn spawn(argv: &[String]) -> Result<(), String> {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let mut child = Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::null())
            .stderr(Stdio::null()).process_group(0).spawn().map_err(|error| format!("{}: {error}", argv[0]))?;
        std::thread::spawn(move || child.wait());
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn run(exec: &str, place: Option<Place>) -> (String, bool) {
            let (argv, told) = command(exec, "/home/u/A b.pdf", place.as_ref()).unwrap();
            (argv.join(" | "), told)
        }

        #[test]
        fn tells_each_app_the_place() {
            let page = || Some(Place::Page(12, Some("circle".into())));
            assert_eq!(run("evince %U", page()), ("evince | --page-index=12 | --find=circle | /home/u/A b.pdf".into(), true));
            assert_eq!(run("okular %U", page()).0, "okular | --page | 12 | --find | circle | /home/u/A b.pdf");
            assert_eq!(run("/usr/bin/google-chrome-stable --ozone-platform=x11 %U", page()).0,
                       "/usr/bin/google-chrome-stable | --ozone-platform=x11 | file:///home/u/A%20b.pdf#page=12");
            assert_eq!(run("flatpak run --branch=stable --command=evince org.gnome.Evince @@u %U @@", page()).0,
                       "flatpak | run | --branch=stable | --command=evince | org.gnome.Evince | --page-index=12 | --find=circle | /home/u/A b.pdf");
            assert_eq!(run("/usr/bin/vlc --started-from-file %U", Some(Place::Moment(65.7))).0,
                       "/usr/bin/vlc | --started-from-file | --start-time=65 | /home/u/A b.pdf");
            assert_eq!(run("mpv --player-operation-mode=pseudo-gui -- %U", Some(Place::Moment(6.0))).0,
                       "mpv | --player-operation-mode=pseudo-gui | --start=6 | -- | /home/u/A b.pdf");
            assert_eq!(run("/usr/share/code/code %F", Some(Place::Line(40))).0, "/usr/share/code/code | -g | /home/u/A b.pdf:40");
            assert_eq!(run("gedit %U", Some(Place::Line(40))).0, "gedit | +40 | /home/u/A b.pdf");
            // Apps that can't be told: the file, from the start.
            assert_eq!(run("cosmic-edit %F", Some(Place::Line(40))), ("cosmic-edit | /home/u/A b.pdf".into(), false));
            assert_eq!(run("totem %U", Some(Place::Moment(30.0))), ("totem | /home/u/A b.pdf".into(), false));
            assert_eq!(run("libreoffice --writer %U", None), ("libreoffice | --writer | /home/u/A b.pdf".into(), false));
        }

        #[test]
        fn a_chromium_browser_opens_its_main_profile() {
            let config = std::env::temp_dir().join(format!("spotlite-open-test-{}", std::process::id()));
            let chrome = config.join("google-chrome");
            std::fs::create_dir_all(chrome.join("Profile 2")).unwrap();
            std::fs::create_dir_all(chrome.join("Profile 3")).unwrap();
            std::fs::write(chrome.join("Local State"),
                           r#"{"profile": {"last_used": "Profile 3", "profiles_order": ["Gone", "Profile 2", "Profile 3"]}}"#).unwrap();
            let url = "https://www.google.com/search?q=a+b";
            assert_eq!(web_command("/usr/bin/google-chrome-stable --ozone-platform=x11 %U", url, &config).unwrap(),
                       ["/usr/bin/google-chrome-stable", "--ozone-platform=x11", "--profile-directory=Profile 2", url]);
            // No profile list and no Default: the browser picks (naming one would make it).
            assert_eq!(web_command("brave-browser %U", url, &config).unwrap(), ["brave-browser", url]);
            assert_eq!(web_command("firefox %u", url, &config).unwrap(), ["firefox", url]);
            std::fs::remove_dir_all(&config).unwrap();
        }
    }
}
