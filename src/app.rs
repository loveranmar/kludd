//! The application: one window, two views and a status bar. Kludd stays
//! running with its window hidden; `--toggle` (the hotkey) shows it.

use std::cell::{Cell, RefCell};
use std::ops::ControlFlow;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};

use crate::config::{self, Config, Folder, MAX_ZOOM, MIN_ZOOM};
use crate::editor::Editor;
use crate::home::Home;
use crate::storage::{self, Note};
use crate::theme::{self, Styles};

pub const APP_ID: &str = "io.github.loveranmar.Kludd";

/// The widest the status-bar path gets, in characters. Longer paths lose
/// their beginning, so the file name stays and the clock keeps its room.
const STATUS_PATH_CHARS: i32 = 28;

/// The application id, which Wayland also uses as the window class.
/// `KLUDD_APP_ID` overrides it, to test a second copy next to your own.
pub fn app_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        std::env::var("KLUDD_APP_ID")
            .ok()
            .filter(|id| gio::Application::id_is_valid(id))
            .unwrap_or_else(|| APP_ID.to_string())
    })
}

thread_local! {
    static START_HIDDEN: Cell<bool> = const { Cell::new(false) };
}

pub fn run() -> glib::ExitCode {
    // `--quit` must never start Kludd itself: ask a running copy through a
    // launcher-only application, which cannot become the primary instance
    if std::env::args().skip(1).any(|a| a == "--quit") {
        let launcher = gio::Application::new(Some(app_id()), gio::ApplicationFlags::IS_LAUNCHER);
        if launcher.register(gio::Cancellable::NONE).is_ok() && launcher.is_remote() {
            launcher.activate_action("quit", None);
            // the request is queued on D-Bus; send it before this process ends
            if let Some(bus) = launcher.dbus_connection() {
                let _ = bus.flush_sync(gio::Cancellable::NONE);
            }
        }
        return glib::ExitCode::SUCCESS;
    }

    // Hyprland notes the workspace a process is started from (and any
    // `[workspace …]` exec rule) in its environment and moves the first
    // window there without switching to it. Kludd started at login shows
    // its window much later, wherever the hotkey is pressed, so the note
    // would send it to the login workspace, out of sight. Start over without
    // it, before Hyprland reads it from /proc when the window appears.
    const HYPRLAND_TOKENS: [&str; 2] = ["HL_INITIAL_WORKSPACE_TOKEN", "HL_EXEC_RULE_TOKEN"];
    if std::env::args().skip(1).any(|a| a == "--hidden")
        && HYPRLAND_TOKENS
            .iter()
            .any(|t| std::env::var_os(t).is_some())
        && let Ok(exe) = std::env::current_exe()
    {
        let mut restart = std::process::Command::new(exe);
        restart.args(std::env::args_os().skip(1));
        for token in HYPRLAND_TOKENS {
            restart.env_remove(token);
        }
        // only returns if the restart failed; then carry on as we are
        let error = restart.exec();
        eprintln!("kludd: could not drop Hyprland's workspace token: {error}");
    }

    let app = gtk::Application::builder()
        .application_id(app_id())
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    app.add_main_option(
        "hidden",
        glib::Char::from(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Start in the background without showing the window (for exec-once)",
        None,
    );
    app.add_main_option(
        "toggle",
        glib::Char::from(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Show the window, or hide it if it is focused (for a hotkey)",
        None,
    );
    app.add_main_option(
        "quit",
        glib::Char::from(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Save and quit the running instance",
        None,
    );
    app.connect_handle_local_options(|app, options| {
        if options.contains("toggle")
            && app.register(gio::Cancellable::NONE).is_ok()
            && app.is_remote()
        {
            app.activate_action("toggle", None);
            return ControlFlow::Break(glib::ExitCode::SUCCESS);
        }
        if options.contains("hidden") {
            if app.register(gio::Cancellable::NONE).is_ok() && app.is_remote() {
                return ControlFlow::Break(glib::ExitCode::SUCCESS); // already running
            }
            START_HIDDEN.set(true);
        }
        ControlFlow::Continue(())
    });

    let kludd: Rc<RefCell<Option<Rc<Kludd>>>> = Rc::default();
    let slot = kludd.clone();
    app.connect_startup(move |app| {
        slot.replace(Some(Kludd::new(app)));
    });
    let slot = kludd.clone();
    app.connect_activate(move |_| {
        if let Some(k) = slot.borrow().as_ref() {
            k.show();
        }
    });
    let slot = kludd.clone();
    app.connect_open(move |_, files, _| {
        if let Some(k) = slot.borrow().as_ref() {
            if let Some(path) = files.first().and_then(|f| f.path()) {
                if path.is_dir() {
                    k.open_dir(&path);
                } else {
                    k.open_note(&path);
                }
            }
            k.show();
        }
    });
    let slot = kludd.clone();
    app.connect_shutdown(move |_| {
        if let Some(k) = slot.borrow().as_ref() {
            k.editor.save_sync();
        }
    });
    app.run()
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Home,
    Editor,
}

struct Kludd {
    window: gtk::ApplicationWindow,
    stack: gtk::Stack,
    home: Home,
    editor: Editor,
    statusbar: gtk::Box,
    status_left: gtk::Label,
    status_right: gtk::Label,
    styles: Styles,
    config: RefCell<Config>,
    notes: RefCell<Vec<Note>>,
    page: Cell<Page>,
    /// The added folder on show, or `None` for the notes folder.
    folder: RefCell<Option<PathBuf>>,
    shown_once: Cell<bool>,
    dir_monitor: RefCell<Option<gio::FileMonitor>>,
    folder_monitor: RefCell<Option<gio::FileMonitor>>,
    file_monitor: RefCell<Option<gio::FileMonitor>>,
    rescan_timer: RefCell<Option<glib::SourceId>>,
    error: RefCell<Option<String>>,
}

impl Kludd {
    fn new(app: &gtk::Application) -> Rc<Kludd> {
        let config = Config::load();
        let display = gdk::Display::default().expect("no display");
        let styles = Styles::install(&display, theme::by_name(&config.theme), config.zoom);
        let notes_dir = config.notes_dir();
        let _ = std::fs::create_dir_all(&notes_dir);

        let home = Home::new();
        home.set_folders(config.folders.clone());
        let editor = Editor::new();
        editor.set_zoom(config.zoom);
        editor.set_theme(theme::by_name(&config.theme));
        home.set_theme(theme::by_name(&config.theme).name);

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::None)
            .vexpand(true)
            .hhomogeneous(false)
            .vhomogeneous(false)
            .build();
        stack.add_named(home.widget(), Some("home"));
        stack.add_named(editor.widget(), Some("editor"));

        let status_left = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Start)
            .max_width_chars(STATUS_PATH_CHARS)
            .build();
        let status_right = gtk::Label::builder().xalign(1.0).hexpand(true).build();
        let statusbar = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        statusbar.add_css_class("statusbar");
        statusbar.append(&status_left);
        statusbar.append(&status_right);

        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.add_css_class("frame");
        frame.set_overflow(gtk::Overflow::Hidden);
        frame.append(&stack);
        frame.append(&statusbar);

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Kludd")
            .default_width(560)
            .default_height(460)
            .decorated(false)
            .css_classes(["kludd"])
            .child(&frame)
            .build();

        let kludd = Rc::new(Kludd {
            window,
            stack,
            home,
            editor,
            statusbar,
            status_left,
            status_right,
            styles,
            config: RefCell::new(config),
            notes: RefCell::new(Vec::new()),
            page: Cell::new(Page::Home),
            folder: RefCell::new(None),
            shown_once: Cell::new(false),
            dir_monitor: RefCell::new(None),
            folder_monitor: RefCell::new(None),
            file_monitor: RefCell::new(None),
            rescan_timer: RefCell::new(None),
            error: RefCell::new(None),
        });
        kludd.connect(app);
        kludd
            .dir_monitor
            .replace(kludd.watch_dir(&kludd.config.borrow().notes_dir()));
        kludd.rescan();
        kludd.start_clock();
        kludd.update_status();
        kludd
    }

    fn connect(self: &Rc<Self>, app: &gtk::Application) {
        // Left-clicking the path in the status bar copies it.
        self.status_left.set_cursor_from_name(Some("pointer"));
        let click = gtk::GestureClick::builder().button(1).build();
        let weak = Rc::downgrade(self);
        click.connect_released(move |_, _, _, _| {
            if let Some(k) = weak.upgrade() {
                k.copy_status_path();
            }
        });
        self.status_left.add_controller(click);
        let weak = Rc::downgrade(self);
        self.home.connect_open(move |path| {
            if let Some(k) = weak.upgrade() {
                k.open_note(&path);
            }
        });
        let weak = Rc::downgrade(self);
        self.home.connect_new(move || {
            if let Some(k) = weak.upgrade() {
                k.new_note();
            }
        });
        let weak = Rc::downgrade(self);
        self.home.connect_theme(move |name| {
            if let Some(k) = weak.upgrade() {
                k.set_theme(name);
            }
        });
        let weak = Rc::downgrade(self);
        self.home.connect_delete(move |path| {
            if let Some(k) = weak.upgrade() {
                if let Err(e) = storage::trash(&path) {
                    k.show_error(format!(
                        "could not trash {}: {e}",
                        config::tilde_path(&path)
                    ));
                }
                k.rescan();
            }
        });
        let weak = Rc::downgrade(self);
        self.home.connect_open_folder(move |path| {
            if let Some(k) = weak.upgrade() {
                k.open_folder(&path);
            }
        });
        let weak = Rc::downgrade(self);
        self.home
            .connect_add_folder(move |text| match weak.upgrade() {
                Some(k) => k.add_folder(&text),
                None => true,
            });
        let weak = Rc::downgrade(self);
        self.home.connect_remove_folder(move |path| {
            if let Some(k) = weak.upgrade() {
                k.remove_folder(&path);
            }
        });
        let weak = Rc::downgrade(self);
        self.home.connect_folder_color(move |path, color| {
            if let Some(k) = weak.upgrade() {
                k.change_folders(|folders| {
                    for folder in folders.iter_mut().filter(|f| f.dir() == path) {
                        folder.color = color.map(str::to_string);
                    }
                });
            }
        });
        let weak = Rc::downgrade(self);
        self.home.connect_back(move || {
            if let Some(k) = weak.upgrade() {
                k.leave_folder();
            }
        });
        let weak = Rc::downgrade(self);
        self.home.connect_count(move |_| {
            if let Some(k) = weak.upgrade() {
                k.update_status();
            }
        });
        let weak = Rc::downgrade(self);
        self.editor.connect_back(move || {
            if let Some(k) = weak.upgrade() {
                k.go_home();
            }
        });
        let weak = Rc::downgrade(self);
        self.editor.connect_status(move |_| {
            if let Some(k) = weak.upgrade() {
                k.update_status();
            }
        });
        let weak = Rc::downgrade(self);
        self.editor.connect_open_file(move |path| {
            if let Some(k) = weak.upgrade() {
                k.open_note(&path);
            }
        });
        let weak = Rc::downgrade(self);
        self.editor.connect_error(move |message| {
            if let Some(k) = weak.upgrade() {
                k.show_error(message);
            }
        });

        // window-wide shortcuts run before the focused widget sees the key
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, code, state| match weak.upgrade() {
            Some(k) if k.key(key, code, state) => glib::Propagation::Stop,
            _ => glib::Propagation::Proceed,
        });
        self.window.add_controller(keys);

        // closing (Alt+Q, the window's close) only hides: Kludd keeps
        // running so the hotkey shows it again at once
        let weak = Rc::downgrade(self);
        self.window.connect_close_request(move |_| {
            if let Some(k) = weak.upgrade() {
                k.hide();
            }
            glib::Propagation::Stop
        });
        let weak = Rc::downgrade(self);
        self.window.connect_is_active_notify(move |window| {
            let Some(k) = weak.upgrade() else { return };
            if window.is_active() {
                match k.page.get() {
                    Page::Home => k.home.refresh_dates(),
                    Page::Editor => k.editor.refresh_toolbar(),
                }
            } else {
                k.editor.save_sync();
                k.editor.hide_toolbar();
                k.home.close_popovers();
            }
        });

        let quit = gio::SimpleAction::new("quit", None);
        let weak = Rc::downgrade(self);
        let app_weak = app.downgrade();
        quit.connect_activate(move |_, _| {
            if let Some(k) = weak.upgrade() {
                k.editor.save_sync();
            }
            if let Some(app) = app_weak.upgrade() {
                app.quit();
            }
        });
        app.add_action(&quit);

        let toggle = gio::SimpleAction::new("toggle", None);
        let weak = Rc::downgrade(self);
        toggle.connect_activate(move |_, _| {
            if let Some(k) = weak.upgrade() {
                k.toggle();
            }
        });
        app.add_action(&toggle);
    }

    fn key(self: &Rc<Self>, key: gdk::Key, code: u32, state: gdk::ModifierType) -> bool {
        use gdk::ModifierType as M;
        let mods = state & (M::CONTROL_MASK | M::SHIFT_MASK | M::ALT_MASK | M::SUPER_MASK);
        const KEY_0: u32 = 19;
        if mods == M::CONTROL_MASK {
            match key.to_lower() {
                // GTK's emoji picker (Ctrl+. / Ctrl+;) grabs input; swallow it
                gdk::Key::period | gdk::Key::semicolon => {}
                gdk::Key::n => self.new_note(),
                gdk::Key::f => {
                    self.go_home();
                    self.home.focus_search();
                }
                gdk::Key::plus | gdk::Key::equal | gdk::Key::KP_Add => self.zoom(Some(0.1)),
                gdk::Key::minus | gdk::Key::KP_Subtract => self.zoom(Some(-0.1)),
                _ => return false,
            }
            return true;
        }
        if mods == M::CONTROL_MASK | M::SHIFT_MASK {
            match (key, code) {
                (_, KEY_0) => self.zoom(None),
                (gdk::Key::plus | gdk::Key::equal, _) => self.zoom(Some(0.1)),
                _ => return false,
            }
            return true;
        }
        if key == gdk::Key::Escape && mods.is_empty() {
            match self.page.get() {
                Page::Editor => self.go_home(),
                // one step back at a time: a popover, the folder field, the
                // search, the added folder, and then Kludd itself
                Page::Home => {
                    let closed = self.home.close_popovers()
                        || self.home.cancel_add_folder()
                        || self.home.clear_search();
                    if closed {
                        return true;
                    }
                    if self.folder.borrow().is_some() {
                        self.leave_folder();
                    } else {
                        self.hide();
                    }
                }
            }
            return true;
        }
        // typing on the home view starts a search
        if self.page.get() == Page::Home
            && !self.home.field_is_focused()
            && (mods.is_empty() || mods == M::SHIFT_MASK)
            && let Some(c) = key.to_unicode()
            && !c.is_control()
            && !c.is_whitespace()
        {
            self.home.type_into_field(c);
            return true;
        }
        false
    }

    // -- showing and hiding ------------------------------------------------------

    fn show(&self) {
        let first = !self.shown_once.replace(true);
        if first && START_HIDDEN.get() {
            // started at login: stay in the background until asked
            return;
        }
        self.window.present();
        self.focus_page();
        if self.page.get() == Page::Home {
            self.home.refresh_dates();
        }
    }

    fn hide(&self) {
        self.editor.save_sync();
        self.editor.hide_toolbar();
        self.home.close_popovers();
        self.clear_error();
        self.window.set_visible(false);
    }

    /// The hotkey: show Kludd, or hide it when it is already in front.
    fn toggle(&self) {
        self.shown_once.set(true);
        if self.window.is_visible() && self.window.is_active() {
            self.hide();
        } else {
            self.show();
        }
    }

    fn focus_page(&self) {
        match self.page.get() {
            Page::Editor => self.editor.focus(),
            Page::Home => GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>),
        }
    }

    // -- navigation ----------------------------------------------------------------

    fn go_home(&self) {
        self.editor.close();
        *self.file_monitor.borrow_mut() = None;
        self.page.set(Page::Home);
        self.stack.set_visible_child_name("home");
        self.statusbar.remove_css_class("tall");
        self.rescan();
        self.home.refresh_dates();
        self.focus_page();
        self.update_status();
    }

    /// The folder new notes go into and the home view lists.
    fn list_dir(&self) -> PathBuf {
        self.folder
            .borrow()
            .clone()
            .unwrap_or_else(|| self.config.borrow().notes_dir())
    }

    /// Shows the notes of an added folder.
    fn open_folder(self: &Rc<Self>, path: &Path) {
        if !path.is_dir() {
            self.show_error(format!("folder not found: {}", config::tilde_path(path)));
            return;
        }
        self.folder.replace(Some(path.to_path_buf()));
        self.home.set_place(Some(path));
        self.folder_monitor.replace(self.watch_dir(path));
        self.go_home();
    }

    /// Back from an added folder to the notes folder.
    fn leave_folder(self: &Rc<Self>) {
        self.folder.replace(None);
        self.home.set_place(None);
        self.folder_monitor.replace(None);
        self.go_home();
    }

    /// `kludd some/folder`: the notes folder itself, or an added folder
    /// (added now if it is new).
    fn open_dir(self: &Rc<Self>, path: &Path) {
        let path: PathBuf = path.components().collect();
        if same_dir(&path, &self.config.borrow().notes_dir()) {
            self.leave_folder();
        } else if self.add_folder(&path.to_string_lossy()) {
            self.open_folder(&path);
        }
    }

    /// Adds a typed folder (saved in the config) and returns true, or shows
    /// why not and returns false. Nothing typed adds nothing.
    fn add_folder(self: &Rc<Self>, text: &str) -> bool {
        // what was wrong with the last try is old news
        self.clear_error();
        if text.trim().is_empty() {
            return true;
        }
        let path = config::typed_path(text);
        let shown = config::tilde_path(&path);
        if !path.is_dir() {
            self.show_error(format!("not a folder: {shown}"));
            return false;
        }
        if same_dir(&path, &self.config.borrow().notes_dir()) {
            self.show_error(format!("{shown} is where your notes already are"));
            return false;
        }
        let known = self
            .config
            .borrow()
            .folders
            .iter()
            .any(|f| same_dir(&f.dir(), &path));
        if !known {
            self.change_folders(|folders| folders.push(Folder::new(shown)));
        }
        true
    }

    /// Forgets an added folder; the folder and its notes stay as they are.
    fn remove_folder(&self, path: &Path) {
        self.change_folders(|folders| folders.retain(|f| f.dir() != path));
    }

    /// Changes the added folders, saves them and shows the new cards.
    fn change_folders(&self, change: impl FnOnce(&mut Vec<Folder>)) {
        let mut config = self.config.borrow_mut();
        change(&mut config.folders);
        if let Err(e) = config.save() {
            eprintln!("kludd: could not save config: {e}");
        }
        let folders = config.folders.clone();
        drop(config);
        self.home.set_folders(folders);
    }

    fn open_note(self: &Rc<Self>, path: &Path) {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let notes_dir = self.config.borrow().notes_dir();
        let managed = path.parent().is_some_and(|p| same_dir(p, &notes_dir));
        if let Err(e) = self.editor.open(&path, managed) {
            self.show_error(format!("could not open {}: {e}", config::tilde_path(&path)));
            return;
        }
        // the notes folder and the folder on show are watched already
        let watched = path.parent().is_some_and(|p| {
            same_dir(p, &notes_dir)
                || self
                    .folder
                    .borrow()
                    .as_deref()
                    .is_some_and(|f| same_dir(p, f))
        });
        *self.file_monitor.borrow_mut() = if watched {
            None
        } else {
            self.watch_file(&path)
        };
        self.show_editor();
    }

    fn new_note(&self) {
        self.editor.new_note(&self.list_dir());
        *self.file_monitor.borrow_mut() = None;
        self.show_editor();
    }

    fn show_editor(&self) {
        self.home.close_popovers();
        self.home.cancel_add_folder();
        // the search was for finding this note; home starts fresh next time
        self.home.clear_search();
        self.page.set(Page::Editor);
        self.stack.set_visible_child_name("editor");
        self.statusbar.add_css_class("tall");
        self.editor.focus();
        self.clear_error();
    }

    // -- settings --------------------------------------------------------------

    fn set_theme(&self, name: &str) {
        let theme = theme::by_name(name);
        let mut config = self.config.borrow_mut();
        config.theme = theme.name.to_string();
        self.styles.update(theme, config.zoom);
        self.editor.set_theme(theme);
        self.home.set_theme(theme.name);
        if let Err(e) = config.save() {
            eprintln!("kludd: could not save config: {e}");
        }
    }

    /// `None` resets to 100 %.
    fn zoom(&self, delta: Option<f64>) {
        let mut config = self.config.borrow_mut();
        let zoom = match delta {
            Some(d) => ((config.zoom + d) * 10.0).round() / 10.0,
            None => 1.0,
        }
        .clamp(MIN_ZOOM, MAX_ZOOM);
        if zoom == config.zoom {
            return;
        }
        config.zoom = zoom;
        self.styles.update(theme::by_name(&config.theme), zoom);
        self.editor.set_zoom(zoom);
        if let Err(e) = config.save() {
            eprintln!("kludd: could not save config: {e}");
        }
    }

    // -- status bar --------------------------------------------------------------

    fn update_status(&self) {
        if let Some(error) = self.error.borrow().as_ref() {
            self.status_left.set_text(error);
        } else {
            match self.page.get() {
                Page::Home => {
                    let dir = config::tilde_path(&self.list_dir());
                    self.status_left.set_text(&format!("{dir}/"));
                }
                Page::Editor => self.status_left.set_text(&self.editor.status_path()),
            }
        }
        match self.page.get() {
            Page::Home => {
                let n = self.home.visible_count();
                self.status_right
                    .set_text(&format!("{n} {}", if n == 1 { "note" } else { "notes" }));
            }
            Page::Editor => self.status_right.set_text(&clock()),
        }
    }

    /// Copies the absolute path shown in the status bar and flashes a
    /// confirmation. Errors are not paths, so clicking one does nothing.
    fn copy_status_path(self: &Rc<Self>) {
        if self.error.borrow().is_some() {
            return;
        }
        let shown = self.status_left.text();
        let path = config::expand_tilde(shown.trim_end_matches('/'));
        if path.as_os_str().is_empty() {
            return;
        }
        self.window
            .clipboard()
            .set_text(&path.display().to_string());
        self.status_left.set_text("copied path");
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(900), move || {
            if let Some(k) = weak.upgrade() {
                k.update_status();
            }
        });
    }

    /// Shows an error in the status bar for a few seconds. The beginning
    /// ("could not open …") is what matters, so the end gets cut if long.
    fn show_error(self: &Rc<Self>, message: String) {
        eprintln!("kludd: {message}");
        self.error.replace(Some(message.clone()));
        self.status_left
            .set_ellipsize(gtk::pango::EllipsizeMode::End);
        self.status_left.set_max_width_chars(-1);
        self.update_status();
        let weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local_once(6, move || {
            if let Some(k) = weak.upgrade()
                && k.error.borrow().as_deref() == Some(message.as_str())
            {
                k.clear_error();
            }
        });
    }

    fn clear_error(&self) {
        self.error.replace(None);
        self.status_left
            .set_ellipsize(gtk::pango::EllipsizeMode::Start);
        self.status_left.set_max_width_chars(STATUS_PATH_CHARS);
        self.update_status();
    }

    /// Ticks on the minute so the clock never lags.
    fn start_clock(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let now = glib::DateTime::now_local()
            .map(|d| d.second() as u64)
            .unwrap_or(0);
        glib::timeout_add_seconds_local_once((60 - now.min(59)) as u32, move || {
            if let Some(k) = weak.upgrade() {
                k.update_status();
                k.start_clock();
            }
        });
    }

    // -- files ------------------------------------------------------------------

    fn rescan(&self) {
        let dir = self.list_dir();
        let notes = storage::scan(&dir, &self.notes.borrow());
        self.notes.replace(notes.clone());
        self.home.set_notes(notes);
        self.update_status();
    }

    fn schedule_rescan(self: &Rc<Self>) {
        if let Some(id) = self.rescan_timer.borrow_mut().take() {
            id.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(Duration::from_millis(150), move || {
            if let Some(k) = weak.upgrade() {
                k.rescan_timer.borrow_mut().take();
                k.rescan();
            }
        });
        self.rescan_timer.replace(Some(id));
    }

    /// Watches a folder of notes: the notes folder, or the added folder on
    /// show. Changes reload the open note and the list.
    fn watch_dir(self: &Rc<Self>, dir: &Path) -> Option<gio::FileMonitor> {
        let monitor = gio::File::for_path(dir)
            .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
            .ok()?;
        let weak = Rc::downgrade(self);
        monitor.connect_changed(move |_, file, other, _| {
            let Some(k) = weak.upgrade() else { return };
            let touched = [Some(file), other]
                .into_iter()
                .flatten()
                .filter_map(|f| f.path());
            let mut any = false;
            for path in touched.filter(|p| storage::is_note(p)) {
                k.editor.file_changed(&path);
                any = true;
            }
            if any {
                k.schedule_rescan();
            }
        });
        Some(monitor)
    }

    /// Watches a note opened from outside the notes folder.
    fn watch_file(self: &Rc<Self>, path: &Path) -> Option<gio::FileMonitor> {
        let monitor = gio::File::for_path(path)
            .monitor_file(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
            .ok()?;
        let weak = Rc::downgrade(self);
        let path = path.to_path_buf();
        monitor.connect_changed(move |_, _, _, event| {
            if matches!(
                event,
                gio::FileMonitorEvent::ChangesDoneHint
                    | gio::FileMonitorEvent::Created
                    | gio::FileMonitorEvent::MovedIn
            ) && let Some(k) = weak.upgrade()
            {
                k.editor.file_changed(&path);
            }
        });
        Some(monitor)
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

fn clock() -> String {
    glib::DateTime::now_local()
        .map(|d| format!("{:02}:{:02}", d.hour(), d.minute()))
        .unwrap_or_default()
}
