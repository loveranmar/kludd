//! Home view: header, search field and the grid of note cards.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use gtk::gdk;
use gtk::prelude::*;

use crate::storage::{self, Note};
use crate::theme::{self, Icon, THEMES};

type Callback<T> = Option<Box<dyn Fn(T)>>;

#[derive(Default)]
struct Callbacks {
    open: Callback<PathBuf>,
    new: Callback<()>,
    theme: Callback<&'static str>,
    delete: Callback<PathBuf>,
    count: Callback<usize>,
}

struct Inner {
    root: gtk::Box,
    search: gtk::Text,
    grid: gtk::FlowBox,
    palette: gtk::Popover,
    swatches: Vec<(&'static str, gtk::Button)>,
    menu: gtk::Popover,
    menu_target: RefCell<Option<PathBuf>>,
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
        let title = gtk::Label::builder()
            .label("My notes")
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["title"])
            .build();
        let palette_button = circle(theme::ICON_PALETTE, "soft", "Theme");
        let plus_button = circle(theme::ICON_PLUS, "primary", "New note");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.append(&palette_button);
        buttons.append(&plus_button);
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        head.add_css_class("head");
        head.append(&title);
        head.append(&buttons);

        let search = gtk::Text::builder()
            .placeholder_text("Search…")
            .hexpand(true)
            .build();
        search.set_input_hints(gtk::InputHints::NO_EMOJI);
        theme::no_builtin_popups(&search);
        let search_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        search_box.add_css_class("search");
        let search_icon = Icon::new(theme::ICON_SEARCH, 14);
        search_icon.add_css_class("search-icon");
        search_box.append(&search_icon);
        search_box.append(&search);
        let click = gtk::GestureClick::new();
        let target = search.downgrade();
        click.connect_pressed(move |_, _, _, _| {
            if let Some(search) = target.upgrade() {
                search.grab_focus_without_selecting();
            }
        });
        search_box.add_controller(click);

        let grid = gtk::FlowBox::builder()
            .min_children_per_line(2)
            .max_children_per_line(2)
            .column_spacing(8)
            .row_spacing(8)
            .homogeneous(true)
            .selection_mode(gtk::SelectionMode::None)
            .activate_on_single_click(true)
            .valign(gtk::Align::Start)
            .css_classes(["grid"])
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&grid)
            .vexpand(true)
            .build();

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("home");
        root.append(&head);
        root.append(&search_box);
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

        let delete = gtk::Button::builder()
            .label("Delete")
            .focus_on_click(false)
            .build();
        delete.set_cursor_from_name(Some("pointer"));
        let menu = popover(&["toolbar"], gtk::PositionType::Bottom);
        menu.set_child(Some(&delete));
        // not parented to the grid: FlowBox::remove_all would trip over it
        menu.set_parent(&root);

        let home = Home(Rc::new(Inner {
            root,
            search,
            grid,
            palette,
            swatches,
            menu,
            menu_target: RefCell::new(None),
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
        delete.connect_clicked(move |_| {
            let Some(home) = upgrade(&weak) else { return };
            home.0.menu.popdown();
            let target = home.0.menu_target.borrow_mut().take();
            if let Some(path) = target
                && let Some(f) = &home.0.callbacks.borrow().delete
            {
                f(path);
            }
        });

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

        let right_click = gtk::GestureClick::builder()
            .button(gdk::BUTTON_SECONDARY)
            .build();
        let weak = Rc::downgrade(&home.0);
        right_click.connect_pressed(move |_, _, x, y| {
            let Some(home) = upgrade(&weak) else { return };
            let Some(child) = home.0.grid.child_at_pos(x as i32, y as i32) else {
                return;
            };
            let path = home
                .0
                .notes
                .borrow()
                .get(child.index() as usize)
                .map(|n| n.path.clone());
            home.0.menu_target.replace(path);
            let at = home
                .0
                .grid
                .compute_point(&home.0.root, &gtk::graphene::Point::new(x as f32, y as f32))
                .unwrap_or_else(|| gtk::graphene::Point::new(x as f32, y as f32));
            let rect = gdk::Rectangle::new(at.x() as i32, at.y() as i32, 1, 1);
            home.0.menu.set_pointing_to(Some(&rect));
            home.0.menu.popup();
        });
        home.0.grid.add_controller(right_click);

        let weak = Rc::downgrade(&home.0);
        home.0.grid.set_filter_func(move |child| {
            let Some(home) = upgrade(&weak) else {
                return true;
            };
            let query = home.0.search.text().to_lowercase();
            home.0
                .notes
                .borrow()
                .get(child.index() as usize)
                .is_none_or(|n| n.matches(query.trim()))
        });
        let weak = Rc::downgrade(&home.0);
        home.0.search.connect_changed(move |_| {
            if let Some(home) = upgrade(&weak) {
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

    fn emit_count(&self) {
        let count = self.visible_count();
        if let Some(f) = &self.0.callbacks.borrow().count {
            f(count);
        }
    }

    pub fn visible_count(&self) -> usize {
        let query = self.0.search.text().to_lowercase();
        self.0
            .notes
            .borrow()
            .iter()
            .filter(|n| n.matches(query.trim()))
            .count()
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
        self.0.search.grab_focus();
    }

    pub fn search_is_focused(&self) -> bool {
        self.0.search.has_focus()
    }

    /// Clears the search; returns false if it was already empty.
    pub fn clear_search(&self) -> bool {
        if self.0.search.text().is_empty() {
            return false;
        }
        self.0.search.set_text("");
        true
    }

    /// Starts a search with a typed character.
    pub fn type_into_search(&self, c: char) {
        let search = &self.0.search;
        search.grab_focus_without_selecting();
        let mut text = search.text().to_string();
        text.push(c);
        search.set_text(&text);
        search.set_position(-1);
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

    fn open_first_match(&self) {
        let query = self.0.search.text().to_lowercase();
        let path = self
            .0
            .notes
            .borrow()
            .iter()
            .find(|n| n.matches(query.trim()))
            .map(|n| n.path.clone());
        if let Some(path) = path
            && let Some(f) = &self.0.callbacks.borrow().open
        {
            f(path);
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

/// Popovers never grab the pointer: a grabbing xdg_popup is refused when
/// the window is not focused yet (for example right after Hyprland toggles
/// the special workspace), and the popover would vanish at once.
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
