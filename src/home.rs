//! Home view: header, search field, the added folders and the grid of note
//! cards. Inside an added folder it lists that folder's notes instead.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use gtk::prelude::*;
use gtk::{gdk, glib};

use crate::config::{self, Folder};
use crate::scroll;
use crate::storage::{self, Note};
use crate::theme::{self, Icon, THEMES};

type Callback<T, R = ()> = Option<Box<dyn Fn(T) -> R>>;

#[derive(Default)]
struct Callbacks {
    open: Callback<PathBuf>,
    new: Callback<()>,
    theme: Callback<&'static str>,
    delete: Callback<PathBuf>,
    count: Callback<usize>,
    open_folder: Callback<PathBuf>,
    remove_folder: Callback<PathBuf>,
    folder_color: Callback<(PathBuf, Option<&'static str>)>,
    back: Callback<()>,
    /// Gets the typed path; returns true when the folder field can close.
    add_folder: Callback<String, bool>,
}

/// What the card menu acts on.
#[derive(Clone)]
enum Target {
    Note(PathBuf),
    Folder(PathBuf),
}

struct Inner {
    root: gtk::Box,
    back: gtk::Button,
    title: gtk::Label,
    add_button: gtk::Button,
    empty: gtk::Label,
    search: gtk::Text,
    search_box: gtk::Box,
    folder_field: gtk::Text,
    folder_box: gtk::Box,
    folders_grid: gtk::FlowBox,
    grid: gtk::FlowBox,
    palette: gtk::Popover,
    swatches: Vec<(&'static str, gtk::Button)>,
    menu: gtk::Popover,
    /// A folder's colours in the card menu: none, then one per theme.
    tint_row: gtk::Box,
    tints: Vec<(Option<&'static str>, gtk::Button)>,
    menu_button: gtk::Button,
    menu_target: RefCell<Option<Target>>,
    /// Showing an added folder: its notes are listed, the folders are not.
    in_folder: Cell<bool>,
    folders: RefCell<Vec<Folder>>,
    notes: RefCell<Vec<Note>>,
    callbacks: RefCell<Callbacks>,
}

#[derive(Clone)]
pub struct Home(Rc<Inner>);

fn upgrade(weak: &Weak<Inner>) -> Option<Home> {
    weak.upgrade().map(Home)
}

impl Home {
    pub fn new() -> Home {
        // inside an added folder: back to the notes, like the editor's
        let back_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let back_icon = Icon::new(theme::ICON_ARROW_LEFT, 14);
        back_icon.add_css_class("back-icon");
        back_box.append(&back_icon);
        back_box.append(&gtk::Label::new(Some("My notes")));
        let back = gtk::Button::builder()
            .child(&back_box)
            .halign(gtk::Align::Start)
            .focus_on_click(false)
            .can_focus(false)
            .visible(false)
            .css_classes(["back"])
            .build();
        back.set_cursor_from_name(Some("pointer"));

        let title = gtk::Label::builder()
            .label("My notes")
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["title"])
            .build();
        let palette_button = circle(theme::ICON_PALETTE, "soft", "Theme");
        let add_button = circle(theme::ICON_FOLDER_PLUS, "soft", "Add folder");
        let plus_button = circle(theme::ICON_PLUS, "primary", "New note");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.append(&palette_button);
        buttons.append(&add_button);
        buttons.append(&plus_button);
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        head.add_css_class("head");
        head.append(&title);
        head.append(&buttons);

        let search = gtk::Text::builder()
            .placeholder_text("Search…")
            .hexpand(true)
            .build();
        let search_box = field(&search, theme::ICON_SEARCH);

        // takes the search field's place while a folder is being added
        let folder_field = gtk::Text::builder()
            .placeholder_text("Folder to add, like ~/code/app/notes")
            .hexpand(true)
            .build();
        let folder_box = field(&folder_field, theme::ICON_FOLDER_PLUS);
        folder_box.set_visible(false);

        let folders_grid = cards(&["folders"]);
        folders_grid.set_visible(false);
        let grid = cards(&["grid"]);
        // shown instead of cards when there is nothing to list
        let empty = gtk::Label::builder()
            .css_classes(["empty"])
            .halign(gtk::Align::Center)
            .visible(false)
            .build();
        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list.append(&folders_grid);
        list.append(&grid);
        list.append(&empty);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .vexpand(true)
            .build();
        scroll::smooth_wheel(&scroller);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("home");
        root.append(&back);
        root.append(&head);
        root.append(&search_box);
        root.append(&folder_box);
        root.append(&scroller);

        // colour picker: right-aligned under the palette button
        let swatch_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let swatches: Vec<_> = THEMES
            .iter()
            .map(|t| {
                // no tooltips: on Wayland an open tooltip popup keeps the
                // palette popover from getting its grab
                let button = gtk::Button::builder()
                    .css_classes(["swatch", t.name])
                    .focus_on_click(false)
                    .build();
                button.update_property(&[gtk::accessible::Property::Label(t.name)]);
                button.set_cursor_from_name(Some("pointer"));
                swatch_row.append(&button);
                (t.name, button)
            })
            .collect();
        let palette = popover(&["palette"], gtk::PositionType::Bottom);
        palette.set_child(Some(&swatch_row));
        palette.set_parent(&root);

        let tint_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        tint_row.add_css_class("tints");
        let tints: Vec<_> = std::iter::once(None)
            .chain(THEMES.iter().map(|t| Some(t.name)))
            .map(|color| {
                let button = gtk::Button::builder()
                    .css_classes(["swatch", "tint", color.unwrap_or("none")])
                    .focus_on_click(false)
                    .build();
                button.update_property(&[gtk::accessible::Property::Label(
                    color.unwrap_or("no colour"),
                )]);
                button.set_cursor_from_name(Some("pointer"));
                tint_row.append(&button);
                (color, button)
            })
            .collect();
        let menu_button = gtk::Button::builder()
            .label("Delete")
            .focus_on_click(false)
            .build();
        menu_button.set_cursor_from_name(Some("pointer"));
        let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        menu_box.append(&tint_row);
        menu_box.append(&menu_button);
        let menu = popover(&["toolbar"], gtk::PositionType::Bottom);
        menu.set_child(Some(&menu_box));
        // not parented to a grid: FlowBox::remove_all would trip over it
        menu.set_parent(&root);

        let home = Home(Rc::new(Inner {
            root,
            back,
            title,
            add_button,
            empty,
            search,
            search_box,
            folder_field,
            folder_box,
            folders_grid,
            grid,
            palette,
            swatches,
            menu,
            tint_row,
            tints,
            menu_button,
            menu_target: RefCell::new(None),
            in_folder: Cell::new(false),
            folders: RefCell::new(Vec::new()),
            notes: RefCell::new(Vec::new()),
            callbacks: RefCell::new(Callbacks::default()),
        }));

        let weak = Rc::downgrade(&home.0);
        palette_button.connect_clicked(move |button| {
            let Some(home) = upgrade(&weak) else { return };
            if home.0.palette.is_visible() {
                home.0.palette.popdown();
            } else {
                home.0.menu.popdown();
                home.show_palette(button);
            }
        });

        // popovers don't grab (see `popover`), so a click anywhere else in
        // the window closes them
        let outside = gtk::GestureClick::builder()
            .button(0)
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        let weak = Rc::downgrade(&home.0);
        let palette_anchor = palette_button.downgrade();
        outside.connect_pressed(move |_, _, x, y| {
            let Some(home) = upgrade(&weak) else { return };
            let on_anchor = home
                .0
                .root
                .pick(x, y, gtk::PickFlags::DEFAULT)
                .zip(palette_anchor.upgrade())
                .is_some_and(|(hit, anchor)| hit == anchor || hit.is_ancestor(&anchor));
            if !on_anchor {
                home.0.palette.popdown();
            }
            home.0.menu.popdown();
        });
        home.0.root.add_controller(outside);
        let weak = Rc::downgrade(&home.0);
        plus_button.connect_clicked(move |_| {
            if let Some(home) = upgrade(&weak)
                && let Some(f) = &home.0.callbacks.borrow().new
            {
                f(());
            }
        });
        for (name, button) in &home.0.swatches {
            let weak = Rc::downgrade(&home.0);
            let name = *name;
            button.connect_clicked(move |_| {
                if let Some(home) = upgrade(&weak)
                    && let Some(f) = &home.0.callbacks.borrow().theme
                {
                    f(name);
                }
            });
        }
        let weak = Rc::downgrade(&home.0);
        home.0.menu_button.connect_clicked(move |_| {
            let Some(home) = upgrade(&weak) else { return };
            home.0.menu.popdown();
            let target = home.0.menu_target.borrow_mut().take();
            let callbacks = home.0.callbacks.borrow();
            match target {
                Some(Target::Note(path)) => {
                    if let Some(f) = &callbacks.delete {
                        f(path);
                    }
                }
                Some(Target::Folder(path)) => {
                    if let Some(f) = &callbacks.remove_folder {
                        f(path);
                    }
                }
                None => {}
            }
        });
        // a colour applies at once and the menu stays open, so the card
        // behind it can be tried in each
        for (color, button) in &home.0.tints {
            let weak = Rc::downgrade(&home.0);
            let color = *color;
            button.connect_clicked(move |_| {
                let Some(home) = upgrade(&weak) else { return };
                let Some(Target::Folder(path)) = home.0.menu_target.borrow().as_ref().cloned()
                else {
                    return;
                };
                home.mark_tint(color);
                if let Some(f) = &home.0.callbacks.borrow().folder_color {
                    f((path, color));
                }
            });
        }
        let weak = Rc::downgrade(&home.0);
        home.0.back.connect_clicked(move |_| {
            if let Some(home) = upgrade(&weak)
                && let Some(f) = &home.0.callbacks.borrow().back
            {
                f(());
            }
        });

        let weak = Rc::downgrade(&home.0);
        home.0.add_button.connect_clicked(move |_| {
            let Some(home) = upgrade(&weak) else { return };
            if !home.cancel_add_folder() {
                home.start_add_folder();
            }
        });
        let weak = Rc::downgrade(&home.0);
        home.0.folder_field.connect_activate(move |field| {
            let Some(home) = upgrade(&weak) else { return };
            let done = match &home.0.callbacks.borrow().add_folder {
                Some(f) => f(field.text().into()),
                None => true,
            };
            if done {
                home.cancel_add_folder();
            }
        });
        // Tab completes folder names, as in a shell
        let tab = gtk::EventControllerKey::new();
        tab.set_propagation_phase(gtk::PropagationPhase::Capture);
        let target = home.0.folder_field.downgrade();
        tab.connect_key_pressed(move |_, key, _, _| {
            if key != gdk::Key::Tab {
                return glib::Propagation::Proceed;
            }
            if let Some(field) = target.upgrade()
                && let Some(completed) = complete_dir(&field.text())
            {
                field.set_text(&completed);
                field.set_position(-1);
            }
            glib::Propagation::Stop
        });
        home.0.folder_field.add_controller(tab);

        let weak = Rc::downgrade(&home.0);
        home.0.grid.connect_child_activated(move |_, child| {
            let Some(home) = upgrade(&weak) else { return };
            let path = home
                .0
                .notes
                .borrow()
                .get(child.index() as usize)
                .map(|n| n.path.clone());
            if let Some(path) = path
                && let Some(f) = &home.0.callbacks.borrow().open
            {
                f(path);
            }
        });
        let weak = Rc::downgrade(&home.0);
        home.0
            .folders_grid
            .connect_child_activated(move |_, child| {
                let Some(home) = upgrade(&weak) else { return };
                let path = home
                    .0
                    .folders
                    .borrow()
                    .get(child.index() as usize)
                    .map(Folder::dir);
                if let Some(path) = path
                    && let Some(f) = &home.0.callbacks.borrow().open_folder
                {
                    f(path);
                }
            });

        // a menu left open while the cards scroll would end up on top of
        // another card: close it as soon as the list moves
        let weak = Rc::downgrade(&home.0);
        scroller.vadjustment().connect_value_changed(move |_| {
            if let Some(home) = upgrade(&weak) {
                home.close_popovers();
            }
        });

        home.connect_menu(&home.0.grid, |home, index| {
            let notes = home.0.notes.borrow();
            Some(Target::Note(notes.get(index)?.path.clone()))
        });
        home.connect_menu(&home.0.folders_grid, |home, index| {
            let folders = home.0.folders.borrow();
            Some(Target::Folder(folders.get(index)?.dir()))
        });

        let weak = Rc::downgrade(&home.0);
        home.0.grid.set_filter_func(move |child| {
            let Some(home) = upgrade(&weak) else {
                return true;
            };
            let query = home.query();
            home.0
                .notes
                .borrow()
                .get(child.index() as usize)
                .is_none_or(|n| n.matches(&query))
        });
        let weak = Rc::downgrade(&home.0);
        home.0.folders_grid.set_filter_func(move |child| {
            let Some(home) = upgrade(&weak) else {
                return true;
            };
            let query = home.query();
            home.0
                .folders
                .borrow()
                .get(child.index() as usize)
                .is_none_or(|f| folder_matches(&f.dir(), &query))
        });
        let weak = Rc::downgrade(&home.0);
        home.0.search.connect_changed(move |_| {
            if let Some(home) = upgrade(&weak) {
                home.0.folders_grid.invalidate_filter();
                home.0.grid.invalidate_filter();
                home.emit_count();
            }
        });
        let weak = Rc::downgrade(&home.0);
        home.0.search.connect_activate(move |_| {
            if let Some(home) = upgrade(&weak) {
                home.open_first_match();
            }
        });
        home
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.0.root
    }

    pub fn connect_open(&self, f: impl Fn(PathBuf) + 'static) {
        self.0.callbacks.borrow_mut().open = Some(Box::new(f));
    }
    pub fn connect_new(&self, f: impl Fn() + 'static) {
        self.0.callbacks.borrow_mut().new = Some(Box::new(move |()| f()));
    }
    pub fn connect_theme(&self, f: impl Fn(&'static str) + 'static) {
        self.0.callbacks.borrow_mut().theme = Some(Box::new(f));
    }
    pub fn connect_delete(&self, f: impl Fn(PathBuf) + 'static) {
        self.0.callbacks.borrow_mut().delete = Some(Box::new(f));
    }
    /// Called with the number of cards shown whenever it changes.
    pub fn connect_count(&self, f: impl Fn(usize) + 'static) {
        self.0.callbacks.borrow_mut().count = Some(Box::new(f));
    }
    pub fn connect_open_folder(&self, f: impl Fn(PathBuf) + 'static) {
        self.0.callbacks.borrow_mut().open_folder = Some(Box::new(f));
    }
    /// "Remove from Kludd" on a folder card: forget it, never touch it.
    pub fn connect_remove_folder(&self, f: impl Fn(PathBuf) + 'static) {
        self.0.callbacks.borrow_mut().remove_folder = Some(Box::new(f));
    }
    /// A colour picked for a folder: a theme name, or `None` for plain.
    pub fn connect_folder_color(&self, f: impl Fn(PathBuf, Option<&'static str>) + 'static) {
        self.0.callbacks.borrow_mut().folder_color =
            Some(Box::new(move |(path, color)| f(path, color)));
    }
    /// Enter in the folder field. Return false to keep the field open (the
    /// path was not a folder).
    pub fn connect_add_folder(&self, f: impl Fn(String) -> bool + 'static) {
        self.0.callbacks.borrow_mut().add_folder = Some(Box::new(f));
    }
    /// "← My notes", shown inside an added folder.
    pub fn connect_back(&self, f: impl Fn() + 'static) {
        self.0.callbacks.borrow_mut().back = Some(Box::new(move |()| f()));
    }

    /// Right-click on a card opens the menu where the click was, for what
    /// `target` says the card at that index stands for.
    fn connect_menu(&self, grid: &gtk::FlowBox, target: fn(&Home, usize) -> Option<Target>) {
        let right_click = gtk::GestureClick::builder()
            .button(gdk::BUTTON_SECONDARY)
            .build();
        let weak = Rc::downgrade(&self.0);
        let grid_ref = grid.downgrade();
        right_click.connect_pressed(move |_, _, x, y| {
            let (Some(home), Some(grid)) = (upgrade(&weak), grid_ref.upgrade()) else {
                return;
            };
            let Some(child) = grid.child_at_pos(x as i32, y as i32) else {
                return;
            };
            let Some(target) = target(&home, child.index() as usize) else {
                return;
            };
            match &target {
                Target::Note(_) => home.0.menu_button.set_label("Delete"),
                Target::Folder(path) => {
                    home.0.menu_button.set_label("Remove from Kludd");
                    let folders = home.0.folders.borrow();
                    let color = folders
                        .iter()
                        .find(|f| f.dir() == *path)
                        .and_then(folder_color);
                    drop(folders);
                    home.mark_tint(color);
                }
            }
            home.0
                .tint_row
                .set_visible(matches!(target, Target::Folder(_)));
            home.0.menu_target.replace(Some(target));
            let at = grid
                .compute_point(&home.0.root, &gtk::graphene::Point::new(x as f32, y as f32))
                .unwrap_or_else(|| gtk::graphene::Point::new(x as f32, y as f32));
            let rect = gdk::Rectangle::new(at.x() as i32, at.y() as i32, 1, 1);
            home.0.palette.popdown();
            home.0.menu.set_pointing_to(Some(&rect));
            home.0.menu.popup();
        });
        grid.add_controller(right_click);
    }

    /// The search as matched against notes and folders.
    fn query(&self) -> String {
        self.0.search.text().trim().to_lowercase()
    }

    fn emit_count(&self) {
        let count = self.visible_count();
        let folders = self.visible_folders();
        self.0.folders_grid.set_visible(folders > 0);
        // folder cards that match are an answer to the search too
        let show = count == 0 && (folders == 0 || self.query().is_empty());
        let empty = &self.0.empty;
        if show {
            empty.set_text(if !self.0.notes.borrow().is_empty() {
                "Nothing matches your search."
            } else if self.0.in_folder.get() {
                "No notes here yet. Press + to start one."
            } else {
                "No notes yet. Press + to start one."
            });
        }
        empty.set_visible(show);
        if let Some(f) = &self.0.callbacks.borrow().count {
            f(count);
        }
    }

    pub fn visible_count(&self) -> usize {
        let query = self.query();
        self.0
            .notes
            .borrow()
            .iter()
            .filter(|n| n.matches(&query))
            .count()
    }

    fn visible_folders(&self) -> usize {
        if self.0.in_folder.get() {
            return 0;
        }
        let query = self.query();
        self.0
            .folders
            .borrow()
            .iter()
            .filter(|f| folder_matches(&f.dir(), &query))
            .count()
    }

    /// Rings the colour a folder has in the card menu.
    fn mark_tint(&self, color: Option<&str>) {
        for (tint, button) in &self.0.tints {
            if *tint == color {
                button.add_css_class("on");
            } else {
                button.remove_css_class("on");
            }
        }
    }

    pub fn set_notes(&self, notes: Vec<Note>) {
        let grid = &self.0.grid;
        grid.remove_all();
        for note in &notes {
            grid.append(&card(note));
        }
        self.0.notes.replace(notes);
        grid.invalidate_filter();
        self.emit_count();
    }

    /// The added folders, shown as cards above the notes.
    pub fn set_folders(&self, folders: Vec<Folder>) {
        let grid = &self.0.folders_grid;
        grid.remove_all();
        for folder in &folders {
            grid.append(&folder_card(folder));
        }
        self.0.folders.replace(folders);
        grid.invalidate_filter();
        self.emit_count();
    }

    /// What is listed: the notes folder with the added folders (`None`), or
    /// the notes of one added folder. Starts without a search.
    pub fn set_place(&self, folder: Option<&Path>) {
        self.0.in_folder.set(folder.is_some());
        self.0
            .title
            .set_text(&folder.map_or_else(|| "My notes".into(), folder_name));
        self.0.back.set_visible(folder.is_some());
        self.0.add_button.set_visible(folder.is_none());
        self.close_popovers();
        self.cancel_add_folder();
        if !self.clear_search() {
            self.emit_count();
        }
    }

    /// Re-renders "today 16:12" style dates.
    pub fn refresh_dates(&self) {
        let notes = self.0.notes.borrow().clone();
        self.set_notes(notes);
    }

    pub fn set_theme(&self, name: &str) {
        for (theme, button) in &self.0.swatches {
            if *theme == name {
                button.add_css_class("on");
            } else {
                button.remove_css_class("on");
            }
        }
    }

    pub fn focus_search(&self) {
        self.cancel_add_folder();
        self.0.search.grab_focus();
    }

    /// Is a text field (the search or the folder field) taking the keys?
    pub fn field_is_focused(&self) -> bool {
        self.0.search.has_focus() || self.0.folder_field.has_focus()
    }

    /// Clears the search; returns false if it was already empty.
    pub fn clear_search(&self) -> bool {
        if self.0.search.text().is_empty() {
            return false;
        }
        self.0.search.set_text("");
        true
    }

    /// Typing on the cards goes into the field on show: it starts a search,
    /// or carries on with the folder being added.
    pub fn type_into_field(&self, c: char) {
        let field = if self.0.folder_box.is_visible() {
            &self.0.folder_field
        } else {
            &self.0.search
        };
        field.grab_focus_without_selecting();
        let mut text = field.text().to_string();
        text.push(c);
        field.set_text(&text);
        field.set_position(-1);
    }

    fn start_add_folder(&self) {
        self.close_popovers();
        self.0.search_box.set_visible(false);
        self.0.folder_box.set_visible(true);
        self.0.folder_field.set_text("");
        self.0.folder_field.grab_focus();
    }

    /// Puts the search back in place of the folder field; returns false if
    /// the folder field was not open.
    pub fn cancel_add_folder(&self) -> bool {
        if !self.0.folder_box.is_visible() {
            return false;
        }
        if self.0.folder_field.has_focus()
            && let Some(root) = self.0.root.root()
        {
            root.set_focus(None::<&gtk::Widget>);
        }
        self.0.folder_box.set_visible(false);
        self.0.folder_field.set_text("");
        self.0.search_box.set_visible(true);
        true
    }

    /// Opens the colour picker 8px below the palette button, right edges
    /// aligned, as in the design.
    fn show_palette(&self, button: &gtk::Button) {
        let root = &self.0.root;
        let Some(bounds) = button.compute_bounds(root) else {
            return;
        };
        let width = 6 * 26 + 5 * 8 + 2 * 8;
        let right = (bounds.x() + bounds.width()) as i32;
        let bottom = (bounds.y() + bounds.height()) as i32;
        let rect = gdk::Rectangle::new(right - width, bottom + 8, width, 1);
        self.0.palette.set_pointing_to(Some(&rect));
        self.0.palette.popup();
    }

    /// Closes the palette or card menu; returns false if neither was open.
    pub fn close_popovers(&self) -> bool {
        let open = self.0.palette.is_visible() || self.0.menu.is_visible();
        self.0.palette.popdown();
        self.0.menu.popdown();
        open
    }

    /// Enter in the search: a note whose title matches beats a folder, and
    /// a folder beats a note that only mentions the words somewhere.
    fn open_first_match(&self) {
        let query = self.query();
        let notes = self.0.notes.borrow();
        let by_title = notes
            .iter()
            .find(|n| n.title.to_lowercase().contains(&query))
            .map(|n| n.path.clone());
        let folder = if self.0.in_folder.get() {
            None
        } else {
            let folders = self.0.folders.borrow();
            folders
                .iter()
                .map(Folder::dir)
                .find(|dir| folder_matches(dir, &query))
        };
        let by_text = notes
            .iter()
            .find(|n| n.matches(&query))
            .map(|n| n.path.clone());
        drop(notes);
        let callbacks = self.0.callbacks.borrow();
        match (by_title, folder, by_text) {
            (Some(path), _, _) | (None, None, Some(path)) => {
                if let Some(f) = &callbacks.open {
                    f(path);
                }
            }
            (None, Some(folder), _) => {
                if let Some(f) = &callbacks.open_folder {
                    f(folder);
                }
            }
            (None, None, None) => {}
        }
    }
}

fn circle(icon: &str, class: &str, label: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .child(&Icon::new(icon, 16))
        .css_classes(["circle", class])
        .focus_on_click(false)
        .valign(gtk::Align::Center)
        .build();
    button.update_property(&[gtk::accessible::Property::Label(label)]);
    button.set_cursor_from_name(Some("pointer"));
    button
}

/// A rounded text field with an icon in front; clicking anywhere on it
/// focuses the text.
fn field(text: &gtk::Text, icon: &str) -> gtk::Box {
    text.set_input_hints(gtk::InputHints::NO_EMOJI);
    theme::no_builtin_popups(text);
    let field = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    field.add_css_class("search");
    let icon = Icon::new(icon, 14);
    icon.add_css_class("search-icon");
    field.append(&icon);
    field.append(text);
    let click = gtk::GestureClick::new();
    let target = text.downgrade();
    click.connect_pressed(move |_, _, _, _| {
        if let Some(text) = target.upgrade() {
            text.grab_focus_without_selecting();
        }
    });
    field.add_controller(click);
    field
}

/// Two cards to a row.
fn cards(classes: &[&str]) -> gtk::FlowBox {
    gtk::FlowBox::builder()
        .min_children_per_line(2)
        .max_children_per_line(2)
        .column_spacing(8)
        .row_spacing(8)
        .homogeneous(true)
        .selection_mode(gtk::SelectionMode::None)
        .activate_on_single_click(true)
        .valign(gtk::Align::Start)
        .css_classes(classes)
        .build()
}

/// Popovers never grab the pointer: a grabbing popover captures all input
/// while it is open, and if the compositor refuses the grab (the window is
/// not focused yet) it vanishes at once. Clicks elsewhere close them instead.
fn popover(classes: &[&str], position: gtk::PositionType) -> gtk::Popover {
    let popover = gtk::Popover::builder()
        .has_arrow(false)
        .autohide(false)
        .position(position)
        .build();
    popover.add_css_class("kludd");
    for class in classes {
        popover.add_css_class(class);
    }
    popover
}

fn card(note: &Note) -> gtk::FlowBoxChild {
    let title = gtk::Label::builder()
        .label(if note.title.trim().is_empty() {
            "Untitled"
        } else {
            note.title.as_str()
        })
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .single_line_mode(true)
        .css_classes(["t"])
        .build();
    let date = gtk::Label::builder()
        .label(storage::relative_date(note.modified))
        .xalign(0.0)
        .css_classes(["d"])
        .build();
    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.add_css_class("card");
    body.append(&title);
    body.append(&date);
    let child = gtk::FlowBoxChild::new();
    child.set_child(Some(&body));
    child.set_cursor_from_name(Some("pointer"));
    child
}

/// A folder card: its name, and where it is (the start of a long path is
/// what gets cut; the end tells folders apart), in its colour if it has one.
fn folder_card(folder: &Folder) -> gtk::FlowBoxChild {
    let path = &folder.dir();
    let icon = Icon::new(theme::ICON_FOLDER, 14);
    icon.add_css_class("folder-icon");
    let title = gtk::Label::builder()
        .label(folder_name(path))
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .single_line_mode(true)
        .css_classes(["t"])
        .build();
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    head.append(&icon);
    head.append(&title);
    let place = gtk::Label::builder()
        .label(path.parent().map(config::tilde_path).unwrap_or_default())
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::Start)
        .single_line_mode(true)
        .css_classes(["d"])
        .build();
    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.add_css_class("card");
    body.add_css_class("folder");
    if let Some(color) = folder_color(folder) {
        body.add_css_class(color);
    }
    body.append(&head);
    body.append(&place);
    let child = gtk::FlowBoxChild::new();
    child.set_child(Some(&body));
    child.set_cursor_from_name(Some("pointer"));
    child
}

/// `~/code/app/notes` -> `notes`.
fn folder_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| config::tilde_path(path))
}

/// The theme whose accent colours a folder's card, if it names one.
fn folder_color(folder: &Folder) -> Option<&'static str> {
    let color = folder.color.as_deref()?;
    THEMES.iter().find(|t| t.name == color).map(|t| t.name)
}

fn folder_matches(path: &Path, query: &str) -> bool {
    query.is_empty() || config::tilde_path(path).to_lowercase().contains(query)
}

/// Tab in the folder field: completes the last part of the typed path to
/// the folders whose names start with it, as far as they agree, and adds
/// the `/` once only one is left. Hidden folders only when asked for.
fn complete_dir(text: &str) -> Option<String> {
    if text == "~" {
        return Some("~/".into());
    }
    let (typed_dir, part) = match text.rfind('/') {
        Some(slash) => text.split_at(slash + 1),
        None => ("", text),
    };
    let mut names: Vec<String> = std::fs::read_dir(config::typed_path(typed_dir))
        .ok()?
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(part) && (part.starts_with('.') || !name.starts_with('.')))
        .collect();
    names.sort();
    let (first, last) = (names.first()?, names.last()?);
    // sorted, so what the first and last share, all of them share
    let shared: usize = first
        .chars()
        .zip(last.chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    let mut completed = format!("{typed_dir}{}", &first[..shared]);
    if names.len() == 1 {
        completed.push('/');
    }
    (completed != text).then_some(completed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_folder_names() {
        let dir = std::env::temp_dir().join(format!("kludd-complete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["notes", "notebook", "docs", ".hidden"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        std::fs::write(dir.join("notes.md"), "").unwrap();
        let at = |rest: &str| format!("{}/{rest}", dir.display());

        assert_eq!(complete_dir(&at("d")), Some(at("docs/")));
        assert_eq!(
            complete_dir(&at("n")),
            Some(at("note")),
            "as far as they agree"
        );
        assert_eq!(complete_dir(&at("note")), None, "nothing more to add");
        assert_eq!(
            complete_dir(&at("notes")),
            Some(at("notes/")),
            "files don't count"
        );
        assert_eq!(complete_dir(&at("x")), None);
        assert_eq!(complete_dir(&at(".h")), Some(at(".hidden/")));
        assert_eq!(
            complete_dir(&at("")),
            None,
            "docs, notebook, notes share nothing"
        );
        assert_eq!(complete_dir("~"), Some("~/".into()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn folder_names_and_search() {
        let path = config::home_dir().join("code/app/notes");
        assert_eq!(folder_name(&path), "notes");
        assert!(folder_matches(&path, "app/no"));
        assert!(folder_matches(&path, ""));
        assert!(!folder_matches(&path, "kludd"));
    }

    #[test]
    fn folder_colours_are_theme_names() {
        let mut folder = Folder::new("~/a".into());
        assert_eq!(folder_color(&folder), None);
        folder.color = Some("lavender".into());
        assert_eq!(folder_color(&folder), Some("lavender"));
        folder.color = Some("chartreuse".into());
        assert_eq!(folder_color(&folder), None, "unknown colours are plain");
    }
}
