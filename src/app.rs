//! The application: one window, two views and a status bar. Kludd stays
//! running with its window hidden; `--toggle` (the hotkey) shows it.

use std::cell::{Cell, RefCell};
use std::ops::ControlFlow;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};

use crate::config::{self, Config, MAX_ZOOM, MIN_ZOOM};
use crate::editor::Editor;
use crate::home::Home;
use crate::storage::{self, Note};
use crate::theme::{self, Styles};

pub const APP_ID: &str = "io.github.loveranmar.Kludd";

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
        }
        return glib::ExitCode::SUCCESS;
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
                k.open_note(&path);
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
    shown_once: Cell<bool>,
    dir_monitor: RefCell<Option<gio::FileMonitor>>,
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
        let editor = Editor::new(notes_dir);
        editor.set_zoom(config.zoom);
        editor.set_accent(theme::by_name(&config.theme).accent_rgba());
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
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Start)
            .build();
        let status_right = gtk::Label::builder().xalign(1.0).build();
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
            shown_once: Cell::new(false),
            dir_monitor: RefCell::new(None),
            file_monitor: RefCell::new(None),
            rescan_timer: RefCell::new(None),
            error: RefCell::new(None),
        });
        kludd.connect(app);
        kludd.watch_dir();
        kludd.rescan();
        kludd.start_clock();
        kludd.update_status();
        kludd
    }

    fn connect(self: &Rc<Self>, app: &gtk::Application) {
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
                if k.page.get() == Page::Home {
                    k.home.refresh_dates();
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

    fn key(&self, key: gdk::Key, code: u32, state: gdk::ModifierType) -> bool {
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
                Page::Home => {
                    if !self.home.close_popovers() && !self.home.clear_search() {
                        self.hide();
                    }
                }
            }
            return true;
        }
        // typing on the home view starts a search
        if self.page.get() == Page::Home
            && !self.home.search_is_focused()
            && (mods.is_empty() || mods == M::SHIFT_MASK)
            && let Some(c) = key.to_unicode()
            && !c.is_control()
            && !c.is_whitespace()
        {
            self.home.type_into_search(c);
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

    fn open_note(self: &Rc<Self>, path: &Path) {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let notes_dir = self.config.borrow().notes_dir();
        let managed = path.parent().is_some_and(|p| same_dir(p, &notes_dir));
        if let Err(e) = self.editor.open(&path, managed) {
            self.show_error(format!("could not open {}: {e}", config::tilde_path(&path)));
            return;
        }
        *self.file_monitor.borrow_mut() = if managed {
            None
        } else {
            self.watch_file(&path)
        };
        self.show_editor();
    }

    fn new_note(&self) {
        self.editor.new_note();
        *self.file_monitor.borrow_mut() = None;
        self.show_editor();
    }

    fn show_editor(&self) {
        self.home.close_popovers();
        self.page.set(Page::Editor);
        self.stack.set_visible_child_name("editor");
        self.statusbar.add_css_class("tall");
        self.editor.focus();
        self.error.replace(None);
        self.update_status();
    }

    // -- settings --------------------------------------------------------------

    fn set_theme(&self, name: &str) {
        let theme = theme::by_name(name);
        let mut config = self.config.borrow_mut();
        config.theme = theme.name.to_string();
        self.styles.update(theme, config.zoom);
        self.editor.set_accent(theme.accent_rgba());
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
                    let dir = config::tilde_path(&self.config.borrow().notes_dir());
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

    fn show_error(&self, message: String) {
        eprintln!("kludd: {message}");
        self.error.replace(Some(message));
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
        let dir = self.config.borrow().notes_dir();
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

    fn watch_dir(self: &Rc<Self>) {
        let dir = gio::File::for_path(self.config.borrow().notes_dir());
        let Ok(monitor) =
            dir.monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        else {
            return;
        };
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
        self.dir_monitor.replace(Some(monitor));
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
