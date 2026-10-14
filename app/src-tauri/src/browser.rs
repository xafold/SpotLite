// `search:` in the panel: the web in SpotLite. Google's results show in the panel itself (`show_in_panel`), where links
// work as in a browser, and Shift+Enter opens the page in a bigger window of SpotLite's own (`run`: back, forward,
// reload, an address bar that also searches, "Open in <your browser>"), a process of its own.
// Both are WebKit views with WebKit contexts of their own: the websites they show get none of the shell's protocols
// (asset:// would let a page probe for your files) and none of its commands, and their cookies stay apart, in
// ~/.local/share/spotlite/browser (the window) and ~/.local/share/spotlite/panel-web (the panel). Linux only.

use gtk::gdk::keys::constants as key;
use gtk::gdk::ModifierType;
use gtk::glib::Propagation;
use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use webkit2gtk::{CookiePersistentStorage, WebContext, WebView, WebsiteDataManager};
use webkit2gtk::{CookieManagerExt, URIRequestExt, WebContextExt, WebViewExt, WebsiteDataManagerExt};

pub fn run(url: &str) {
    if gtk::init().is_err() {
        eprintln!("spotlite-app: no display for the browser");
        std::process::exit(1);
    }
    let view = web_view("browser");

    let button = |icon: &str, tip: &str| {
        let button = gtk::Button::from_icon_name(Some(icon), gtk::IconSize::Button);
        button.set_tooltip_text(Some(tip));
        button
    };
    let (back, forward, reload) = (button("go-previous-symbolic", "Back (Alt+←)"),
                                   button("go-next-symbolic", "Forward (Alt+→)"),
                                   button("view-refresh-symbolic", "Reload (F5)"));
    let address = gtk::Entry::new();
    address.set_hexpand(true);
    address.set_width_chars(70);
    address.set_placeholder_text(Some("Search Google or type a web address"));
    let outside = gtk::Button::with_label(&format!("Open in {}", crate::open_at::browser().unwrap_or("your browser".into())));
    let bar = gtk::HeaderBar::new();
    bar.set_show_close_button(true);
    bar.pack_start(&back);
    bar.pack_start(&forward);
    bar.pack_start(&reload);
    bar.set_custom_title(Some(&address));
    bar.pack_end(&outside);
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_default_size(1100, 780);
    window.set_title("SpotLite Search");
    window.set_titlebar(Some(&bar));
    window.add(&view);

    back.connect_clicked({ let view = view.clone(); move |_| view.go_back() });
    forward.connect_clicked({ let view = view.clone(); move |_| view.go_forward() });
    reload.connect_clicked({ let view = view.clone(); move |_| view.reload() });
    address.connect_activate({ let view = view.clone(); move |entry| view.load_uri(&target(&entry.text())) });
    outside.connect_clicked({
        let view = view.clone();
        move |_| {
            if let Some(uri) = view.uri() {
                if let Err(error) = crate::open_at::open_url(&uri) {
                    eprintln!("spotlite-app: {error}");
                }
            }
        }
    });
    view.connect_uri_notify({
        let address = address.clone();
        move |view| {
            if !address.has_focus() {
                address.set_text(view.uri().as_deref().unwrap_or_default());
            }
        }
    });
    view.connect_title_notify({
        let window = window.clone();
        move |view| window.set_title(&view.title().map_or("SpotLite Search".into(), |title| format!("{title} · SpotLite")))
    });
    view.connect_load_changed({
        let (back, forward) = (back.clone(), forward.clone());
        move |view, _| {
            back.set_sensitive(view.can_go_back());
            forward.set_sensitive(view.can_go_forward());
        }
    });
    window.connect_key_press_event({
        let (view, address) = (view.clone(), address.clone());
        move |window, event| {
            let (pressed, state) = (event.keyval(), event.state());
            let (control, alt) = (state.contains(ModifierType::CONTROL_MASK), state.contains(ModifierType::MOD1_MASK));
            if control && (pressed == key::w || pressed == key::W) {
                window.close();
            } else if control && (pressed == key::l || pressed == key::L) {
                address.grab_focus();
            } else if pressed == key::F5 || control && (pressed == key::r || pressed == key::R) {
                view.reload();
            } else if alt && pressed == key::Left {
                view.go_back();
            } else if alt && pressed == key::Right {
                view.go_forward();
            } else {
                return Propagation::Proceed;
            }
            Propagation::Stop
        }
    });
    window.connect_destroy(|_| gtk::main_quit());
    view.load_uri(url);
    window.show_all();
    view.grab_focus();
    gtk::main();
}

/// A WebKit view in a context of its own, its data in ~/.local/share/spotlite/`name`. A link meant for a new window or
/// tab opens in it: one view, like the rest of SpotLite.
fn web_view(name: &str) -> WebView {
    let data = gtk::glib::user_data_dir().join("spotlite").join(name);
    let manager = WebsiteDataManager::builder()
        .base_data_directory(data.join("data").display().to_string())
        .base_cache_directory(gtk::glib::user_cache_dir().join("spotlite").join(name).display().to_string())
        .build();
    if let Some(cookies) = manager.cookie_manager() {
        cookies.set_persistent_storage(&data.join("cookies.sqlite").display().to_string(), CookiePersistentStorage::Sqlite);
    }
    let context = WebContext::builder().website_data_manager(&manager).build();
    // Pages in the system's language (LANG=en_US.UTF-8: "en-US", "en"), not the one Google guesses from where you are.
    let languages = languages(&gtk::glib::language_names().iter().map(|name| name.to_string()).collect::<Vec<_>>());
    context.set_preferred_languages(&languages.iter().map(String::as_str).collect::<Vec<_>>());
    let view = WebView::with_context(&context);
    view.connect_create(|view, action| {
        if let Some(uri) = action.clone().request().and_then(|request| request.uri()) {
            view.load_uri(&uri);
        }
        None
    });
    view
}

// MARK: In the panel

/// The panel's page under an overlay, where the web view goes over its results area.
struct PanelWeb {
    overlay: gtk::Overlay,
    page: gtk::Widget,
    view: Option<WebView>,
    query: String,
    on_escape: Rc<dyn Fn()>,
    on_field: Rc<dyn Fn()>,
}

thread_local! {
    static PANEL: RefCell<Option<PanelWeb>> = const { RefCell::new(None) };
}

/// Puts the panel's page under an overlay for `show_in_panel`. `on_escape`: Esc in the web view (the panel goes);
/// `on_field`: Ctrl+L in it (the query field takes the keys again).
pub fn attach(window: &gtk::ApplicationWindow, on_escape: impl Fn() + 'static, on_field: impl Fn() + 'static) {
    let Some(child) = window.child() else { return };
    let page = child.clone().downcast::<gtk::Container>().ok()
        .and_then(|container| container.children().into_iter().next()).unwrap_or_else(|| child.clone());
    window.remove(&child);
    let overlay = gtk::Overlay::new();
    overlay.add(&child);
    window.add(&overlay);
    overlay.show();
    PANEL.with(|panel| *panel.borrow_mut() = Some(PanelWeb {
        overlay, page, view: None, query: String::new(), on_escape: Rc::new(on_escape), on_field: Rc::new(on_field),
    }));
}

/// Google's results for `query` over the panel's area at (x, y), width × height (its page's pixels); loaded again
/// only for a new query, so the page you moved to stays.
pub fn show_in_panel(query: &str, (x, y, width, height): (f64, f64, f64, f64)) {
    PANEL.with(|panel| {
        let mut panel = panel.borrow_mut();
        let Some(PanelWeb { overlay, page, view, query: shown, on_escape, on_field }) = panel.as_mut() else { return };
        let view = view.get_or_insert_with(|| {
            let view = web_view("panel-web");
            view.set_halign(gtk::Align::Start);
            view.set_valign(gtk::Align::Start);
            // Tauri shows the panel with show_all(), which would show a hidden view again over the next results.
            view.set_no_show_all(true);
            let (page, on_escape, on_field) = (page.clone(), on_escape.clone(), on_field.clone());
            view.connect_key_press_event(move |view, event| {
                let (pressed, alt) = (event.keyval(), event.state().contains(ModifierType::MOD1_MASK));
                let control = event.state().contains(ModifierType::CONTROL_MASK);
                if pressed == key::Escape {
                    on_escape();
                } else if control && (pressed == key::l || pressed == key::L) {
                    page.grab_focus();
                    on_field();
                } else if alt && pressed == key::Left {
                    view.go_back();
                } else if alt && pressed == key::Right {
                    view.go_forward();
                } else {
                    return Propagation::Proceed;
                }
                Propagation::Stop
            });
            overlay.add_overlay(&view);
            view
        });
        view.set_margin_start(x.round() as i32);
        view.set_margin_top(y.round() as i32);
        view.set_size_request(width.round() as i32, height.round() as i32);
        if shown != query {
            *shown = query.to_string();
            view.load_uri(&crate::open_at::search_url(query));
        }
        view.show();
    });
}

pub fn hide_in_panel() {
    PANEL.with(|panel| {
        if let Some(view) = panel.borrow().as_ref().and_then(|panel| panel.view.as_ref()) {
            view.hide();
        }
    });
}

/// The page the panel's web view is on, while it shows.
pub fn panel_url() -> Option<String> {
    PANEL.with(|panel| {
        let panel = panel.borrow();
        let view = panel.as_ref()?.view.as_ref().filter(|view| view.is_visible())?;
        view.uri().map(String::from)
    })
}

/// The system's languages as web pages name them: "en_US.UTF-8", "en_US", "en", "C" → "en-US", "en".
fn languages(locales: &[String]) -> Vec<String> {
    let mut languages: Vec<String> = vec![];
    for locale in locales {
        let language = locale.split(['.', '@']).next().unwrap_or_default().replace('_', "-");
        if !language.is_empty() && language != "C" && language != "POSIX" && !languages.contains(&language) {
            languages.push(language);
        }
    }
    languages
}

/// What the address bar opens: a web address as it is ("https://" added when it's left out), anything else as a
/// Google search.
fn target(typed: &str) -> String {
    let typed = typed.trim();
    if typed.contains("://") {
        typed.to_string()
    } else if !typed.contains(char::is_whitespace) && typed.contains('.') && !typed.ends_with('.') {
        format!("https://{typed}")
    } else {
        crate::open_at::search_url(typed)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_address_bar_opens_addresses_and_searches() {
        assert_eq!(super::target("https://example.com/a"), "https://example.com/a");
        assert_eq!(super::target("wikipedia.org"), "https://wikipedia.org");
        assert!(super::target("best hiking trails").starts_with("https://www.google.com/search?q=best+hiking+trails"));
        assert!(super::target("node.js tutorial").starts_with("https://www.google.com/search?q=node.js+tutorial"));
        let locales = ["en_US.UTF-8", "en_US", "en.UTF-8", "en", "C"].map(String::from);
        assert_eq!(super::languages(&locales), ["en-US", "en"]);
    }
}
