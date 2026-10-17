//! The note editor: a GtkTextView whose lines map 1:1 onto Markdown lines.
//!
//! Inline formatting lives in text tags. Each line's kind (text, heading,
//! bullet, raw) lives in a side table on the view, kept in step with the
//! buffer from its insert/delete signals, and is mirrored into paragraph
//! tags for rendering. GTK's own undo ignores tags, so undo is done here by
//! recording line snapshots before and after every change.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib, graphene, gsk, pango};

use crate::config;
use crate::markdown::{self, Eol, Line, LineKind, Span, Style};
use crate::storage;
use crate::theme::{self, Icon};

/// Indent levels that get their own margin tag; deeper bullets share the last.
const LEVELS: usize = 8;
/// CSS line-height 1.55 expressed as Pango's factor of Fredoka's
/// ascent+descent (1.21 em).
const LINE_HEIGHT: f64 = 1.55 / 1.21;
const SAVE_DELAY: Duration = Duration::from_millis(400);

// ---------------------------------------------------------------------------
// The view: draws its own selection (rounded, glyph height) and bullets.

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct View {
        pub kinds: RefCell<Vec<LineKind>>,
        /// Markdown of each line, kept until the line changes, so saving
        /// a long note only serializes what was edited.
        pub cache: RefCell<Vec<Option<String>>>,
        pub zoom: Cell<f64>,
        /// Selection colour of the current theme.
        pub accent: RefCell<Option<gdk::RGBA>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for View {
        const NAME: &'static str = "KluddView";
        type Type = super::View;
        type ParentType = gtk::TextView;
    }

    impl ObjectImpl for View {
        fn dispose(&self) {
            // GtkTextView only knows how to remove its own children; a
            // popover parented to it would make dispose spin forever
            let mut child = self.obj().first_child();
            while let Some(widget) = child {
                child = widget.next_sibling();
                if widget.is::<gtk::Popover>() {
                    widget.unparent();
                }
            }
        }
    }
    impl WidgetImpl for View {}

    impl TextViewImpl for View {
        fn snapshot_layer(&self, layer: gtk::TextViewLayer, snapshot: gtk::Snapshot) {
            if layer == gtk::TextViewLayer::BelowText {
                let view = self.obj();
                view.draw_selection(&snapshot);
                view.draw_bullets(&snapshot);
            }
            self.parent_snapshot_layer(layer, snapshot);
        }
    }
}

glib::wrapper! {
    pub struct View(ObjectSubclass<imp::View>)
        @extends gtk::TextView, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Scrollable;
}

impl View {
    fn new() -> View {
        let view: View = glib::Object::new();
        view.imp().zoom.set(1.0);
        view.imp().kinds.replace(vec![LineKind::Text]);
        view.imp().cache.replace(vec![None]);
        view
    }

    fn invalidate(&self, first: usize, last: usize) {
        let mut cache = self.imp().cache.borrow_mut();
        let end = (last + 1).min(cache.len());
        for entry in &mut cache[first.min(end)..end] {
            *entry = None;
        }
    }

    fn kind(&self, line: usize) -> LineKind {
        self.imp()
            .kinds
            .borrow()
            .get(line)
            .copied()
            .unwrap_or(LineKind::Text)
    }

    fn zoom(&self) -> f32 {
        self.imp().zoom.get() as f32
    }

    /// Top and height of the glyph box of a character (ascent + descent),
    /// which is what the design highlights rather than the whole line box.
    /// A browser rounds that box outwards, hence the extra pixel.
    fn glyph_box(&self, rect: &gdk::Rectangle) -> (f32, f32) {
        (rect.y() as f32 - 1.0, rect.height() as f32 + 1.0)
    }

    /// The whole line box (glyphs plus line spacing) around a glyph box, so
    /// consecutive selected lines meet without gaps.
    fn line_box(&self, rect: &gdk::Rectangle) -> (f32, f32) {
        let glyphs = rect.height() as f32;
        let full = glyphs * LINE_HEIGHT as f32;
        let top = rect.y() as f32 - (full - glyphs) / 2.0;
        (top.floor(), (top + full).ceil() - top.floor() + 1.0)
    }

    /// Right edge of the text on the display line `[line, content_end)`.
    fn text_right(&self, line: &gtk::TextIter, content_end: &gtk::TextIter) -> i32 {
        if content_end <= line {
            return self.iter_location(line).x();
        }
        let mut last = *content_end;
        last.backward_char();
        let r = self.iter_location(&last);
        r.x() + r.width()
    }

    /// Draws the selection as rounded blocks in the theme's accent colour,
    /// one per display line: snug around the glyphs for a one-line
    /// selection (as in the design), full line height when it spans lines
    /// so it reads as one block. Selected line breaks and empty lines get a
    /// small block too.
    fn draw_selection(&self, snapshot: &gtk::Snapshot) {
        let buffer = self.buffer();
        let Some((start, end)) = buffer.selection_bounds() else {
            return;
        };
        let Some(color) = *self.imp().accent.borrow() else {
            return;
        };
        let zoom = self.zoom();
        let radius = 4.0 * zoom;
        let newline = (5.0 * zoom).round() as i32;
        let visible = self.visible_rect();
        let bottom = visible.y() + visible.height();

        let mut probe = start;
        let single = !self.forward_display_line(&mut probe) || probe >= end;

        // first display line that is visible and selected
        let (top, _) = self.line_at_y(visible.y());
        let mut line = if start > top { start } else { top };
        if !self.starts_display_line(&line) {
            self.backward_display_line_start(&mut line);
        }
        loop {
            let mut next = line;
            let last_line = !self.forward_display_line(&mut next) || next <= line;
            if last_line {
                next = buffer.end_iter();
            }
            // text on this display line ends before its line break, if any
            let mut content_end = next;
            if !last_line && next.line() > line.line() {
                content_end = line;
                if !content_end.ends_line() {
                    content_end.forward_to_line_end();
                }
            }
            let loc = self.iter_location(&line);
            if loc.y() > bottom {
                break;
            }
            let a = if start > line { start } else { line };
            let b = if end < next { end } else { next };
            if a < b {
                let x1 = self.iter_location(&a).x();
                let mut x2 = if b < content_end {
                    self.iter_location(&b).x()
                } else {
                    self.text_right(&line, &content_end)
                };
                // the line break itself is selected
                if end > content_end && content_end < next {
                    x2 += newline;
                }
                if x2 > x1 {
                    let glyphs = self.iter_location(&a);
                    let (y, height) = if single {
                        self.glyph_box(&glyphs)
                    } else {
                        self.line_box(&glyphs)
                    };
                    let rect = graphene::Rect::new(x1 as f32, y, (x2 - x1) as f32, height);
                    snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(rect, radius));
                    snapshot.append_color(&color, &rect);
                    snapshot.pop();
                }
            }
            if last_line || next >= end {
                break;
            }
            line = next;
        }
    }

    fn draw_bullets(&self, snapshot: &gtk::Snapshot) {
        let kinds = self.imp().kinds.borrow();
        let zoom = self.zoom();
        let color = self.color();
        let visible = self.visible_rect();
        let (mut iter, _) = self.line_at_y(visible.y());
        loop {
            let (y, _) = self.line_yrange(&iter);
            if y > visible.y() + visible.height() {
                break;
            }
            if let Some(LineKind::Bullet { indent, .. }) = kinds.get(iter.line() as usize) {
                let level = (*indent as usize / 2).min(LEVELS - 1) as f32;
                let rect = self.iter_location(&iter);
                let cx = (5.5 + 20.0 * level) * zoom;
                let cy = rect.y() as f32 + rect.height() as f32 / 2.0 - 0.5 * zoom;
                let builder = gsk::PathBuilder::new();
                builder.add_circle(&graphene::Point::new(cx, cy), 2.5 * zoom);
                snapshot.append_fill(&builder.to_path(), gsk::FillRule::Winding, &color);
            }
            let line = iter.line();
            iter.forward_line();
            if iter.line() == line {
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tags

struct Tags {
    /// On every line: CSS line-height has no effect on a GtkTextView.
    body: gtk::TextTag,
    bold: gtk::TextTag,
    italic: gtk::TextTag,
    underline: gtk::TextTag,
    raw: gtk::TextTag,
    escaped: gtk::TextTag,
    italic_us: gtk::TextTag,
    bold_us: gtk::TextTag,
    h1: gtk::TextTag,
    h2: gtk::TextTag,
    list_start: gtk::TextTag,
    levels: Vec<gtk::TextTag>,
}

impl Tags {
    fn new(table: &gtk::TextTagTable) -> Tags {
        let add = |tag: gtk::TextTag| {
            table.add(&tag);
            tag
        };
        let plain = |name: &str| add(gtk::TextTag::builder().name(name).build());
        let tags = Tags {
            body: add(gtk::TextTag::builder()
                .name("body")
                .line_height(LINE_HEIGHT as f32)
                .build()),
            h1: add(gtk::TextTag::builder()
                .name("h1")
                .scale(20.0 / 15.0)
                .weight(500)
                .build()),
            h2: add(gtk::TextTag::builder()
                .name("h2")
                .scale(17.0 / 15.0)
                .weight(500)
                .build()),
            list_start: plain("list-start"),
            levels: (0..LEVELS).map(|i| plain(&format!("level-{i}"))).collect(),
            bold: add(gtk::TextTag::builder().name("bold").weight(500).build()),
            italic: add(gtk::TextTag::builder()
                .name("italic")
                .style(pango::Style::Italic)
                .build()),
            underline: add(gtk::TextTag::builder()
                .name("underline")
                .underline(pango::Underline::Single)
                .build()),
            raw: plain("raw"),
            escaped: plain("escaped"),
            italic_us: plain("italic-us"),
            bold_us: plain("bold-us"),
        };
        tags.set_zoom(1.0);
        tags
    }

    fn set_zoom(&self, zoom: f64) {
        let px = |v: f64| (v * zoom).round() as i32;
        self.h1.set_pixels_below_lines(px(26.0));
        self.h2.set_pixels_above_lines(px(10.0));
        self.h2.set_pixels_below_lines(px(4.0));
        self.list_start.set_pixels_above_lines(px(4.0));
        for (i, tag) in self.levels.iter().enumerate() {
            tag.set_left_margin(px(20.0 * (i + 1) as f64));
        }
    }

    fn char_tags(&self) -> [(&gtk::TextTag, Style); 7] {
        [
            (&self.bold, Style::BOLD),
            (&self.italic, Style::ITALIC),
            (&self.underline, Style::UNDERLINE),
            (&self.raw, Style::RAW),
            (&self.escaped, Style::ESCAPED),
            (&self.italic_us, Style::ITALIC_US),
            (&self.bold_us, Style::BOLD_US),
        ]
    }

    fn line_tags(&self) -> impl Iterator<Item = &gtk::TextTag> {
        [&self.h1, &self.h2, &self.list_start]
            .into_iter()
            .chain(self.levels.iter())
    }
}

// ---------------------------------------------------------------------------
// Undo

#[derive(Clone)]
struct Step {
    first: usize,
    before: Vec<Line>,
    after: Vec<Line>,
    cursor_before: i32,
    cursor_after: i32,
    /// Typing a single character on one line: merges with the next one.
    typing: bool,
    at: Instant,
}

// ---------------------------------------------------------------------------
// The open document

struct Doc {
    /// `None` for a new note that has not been written yet.
    path: Option<PathBuf>,
    /// Lives in the notes folder, so its file name follows the title.
    managed: bool,
    /// Created in this session; trashed again if left empty.
    created: bool,
    final_eol: Eol,
    /// File contents as last read or written.
    saved: String,
    dirty: bool,
}

#[derive(Default)]
struct State {
    /// Buffer changes made by Kludd itself: signal handlers only keep the
    /// line table in step and record nothing.
    programmatic: bool,
    pending: Option<(usize, Vec<Line>, i32)>,
    typing_style: Option<Style>,
    undo: Vec<Step>,
    redo: Vec<Step>,
    doc: Option<Doc>,
    save_timer: Option<glib::SourceId>,
    toolbar_timer: Option<glib::SourceId>,
    /// A mouse button is down in the text: a selection is being dragged.
    pointer_down: bool,
    writing: bool,
    save_again: bool,
}

#[derive(Default)]
struct Callbacks {
    back: Option<Box<dyn Fn()>>,
    status: Option<Box<dyn Fn(String)>>,
    error: Option<Box<dyn Fn(String)>>,
}

struct Inner {
    root: gtk::Overlay,
    view: View,
    buffer: gtk::TextBuffer,
    tags: Tags,
    toolbar: gtk::Popover,
    notes_dir: RefCell<PathBuf>,
    state: RefCell<State>,
    callbacks: RefCell<Callbacks>,
}

#[derive(Clone)]
pub struct Editor(Rc<Inner>);

impl Editor {
    pub fn new(notes_dir: PathBuf) -> Editor {
        let table = gtk::TextTagTable::new();
        let tags = Tags::new(&table);
        let buffer = gtk::TextBuffer::new(Some(&table));
        buffer.set_enable_undo(false);

        let view = View::new();
        view.set_buffer(Some(&buffer));
        view.add_css_class("editor");
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        view.set_accepts_tab(true);
        view.set_vexpand(true);
        view.set_input_hints(gtk::InputHints::NO_EMOJI);
        theme::no_builtin_popups(&view);

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&view)
            .vexpand(true)
            .build();

        let back_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let back_icon = Icon::new(theme::ICON_ARROW_LEFT, 14);
        back_icon.add_css_class("back-icon");
        back_box.append(&back_icon);
        back_box.append(&gtk::Label::new(Some("Notes")));
        let back = gtk::Button::builder()
            .child(&back_box)
            .halign(gtk::Align::Start)
            .focus_on_click(false)
            .can_focus(false)
            .css_classes(["back"])
            .build();
        back.set_cursor_from_name(Some("pointer"));

        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.add_css_class("editor-view");
        column.append(&back);
        column.append(&scroller);

        let notch = gtk::Box::builder()
            .css_classes(["notch"])
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Start)
            .can_target(false)
            .build();

        let root = gtk::Overlay::new();
        root.set_child(Some(&column));
        root.add_overlay(&notch);

        let toolbar = gtk::Popover::builder()
            .has_arrow(false)
            .autohide(false)
            .can_focus(false)
            .position(gtk::PositionType::Top)
            .css_classes(["kludd", "toolbar"])
            .build();
        toolbar.set_parent(&view);

        let editor = Editor(Rc::new(Inner {
            root,
            view,
            buffer,
            tags,
            toolbar,
            notes_dir: RefCell::new(notes_dir),
            state: RefCell::new(State::default()),
            callbacks: RefCell::new(Callbacks::default()),
        }));
        editor.build_toolbar();
        editor.connect_buffer();
        editor.connect_keys();
        editor.connect_pointer();
        editor.connect_clipboard();
        let weak = editor.downgrade();
        back.connect_clicked(move |_| {
            if let Some(editor) = upgrade(&weak) {
                editor.emit_back();
            }
        });
        editor
    }

    fn downgrade(&self) -> Weak<Inner> {
        Rc::downgrade(&self.0)
    }

    pub fn widget(&self) -> &gtk::Overlay {
        &self.0.root
    }

    pub fn focus(&self) {
        self.0.view.grab_focus();
    }

    pub fn connect_back(&self, f: impl Fn() + 'static) {
        self.0.callbacks.borrow_mut().back = Some(Box::new(f));
    }

    /// Called with the status-bar path whenever it changes.
    pub fn connect_status(&self, f: impl Fn(String) + 'static) {
        self.0.callbacks.borrow_mut().status = Some(Box::new(f));
    }

    pub fn connect_error(&self, f: impl Fn(String) + 'static) {
        self.0.callbacks.borrow_mut().error = Some(Box::new(f));
    }

    fn emit_back(&self) {
        if let Some(f) = &self.0.callbacks.borrow().back {
            f();
        }
    }

    fn emit_status(&self) {
        let text = self.status_path();
        if let Some(f) = &self.0.callbacks.borrow().status {
            f(text);
        }
    }

    fn emit_error(&self, message: String) {
        if let Some(f) = &self.0.callbacks.borrow().error {
            f(message);
        }
    }

    pub fn set_accent(&self, accent: gdk::RGBA) {
        self.0.view.imp().accent.replace(Some(accent));
        self.0.view.queue_draw();
    }

    pub fn set_zoom(&self, zoom: f64) {
        self.0.view.imp().zoom.set(zoom);
        self.0.tags.set_zoom(zoom);
        self.relayout();
    }

    /// After the text changed size: redraw once GTK has laid it out again
    /// (bullets and the selection are drawn from the new line positions),
    /// and move the toolbar to where the selection is now.
    fn relayout(&self) {
        self.0.view.queue_draw();
        for delay in [40, 200] {
            let weak = self.downgrade();
            glib::timeout_add_local_once(Duration::from_millis(delay), move || {
                if let Some(editor) = upgrade(&weak) {
                    editor.0.view.queue_draw();
                }
            });
        }
        if self.0.toolbar.is_visible() {
            self.0.toolbar.popdown();
            self.schedule_toolbar();
        }
    }

    // -- line model ----------------------------------------------------------

    fn line_count(&self) -> usize {
        self.0.buffer.line_count() as usize
    }

    fn line_start(&self, line: usize) -> gtk::TextIter {
        self.0
            .buffer
            .iter_at_line(line as i32)
            .unwrap_or_else(|| self.0.buffer.end_iter())
    }

    fn line_content_end(&self, line: usize) -> gtk::TextIter {
        let mut end = self.line_start(line);
        if !end.ends_line() {
            end.forward_to_line_end();
        }
        end
    }

    /// Start of `line` to the start of the next line (or the buffer end).
    fn line_span(&self, line: usize) -> (gtk::TextIter, gtk::TextIter) {
        let start = self.line_start(line);
        let mut end = start;
        if !end.forward_line() {
            end = self.0.buffer.end_iter();
        }
        (start, end)
    }

    fn style_at(&self, iter: &gtk::TextIter) -> Style {
        self.0
            .tags
            .char_tags()
            .into_iter()
            .filter(|(tag, _)| iter.has_tag(*tag))
            .fold(Style::NONE, |acc, (_, style)| acc.with(style))
    }

    /// One buffer line as a Markdown line.
    fn line_model(&self, line: usize) -> Line {
        let buffer = &self.0.buffer;
        let start = self.line_start(line);
        let end = self.line_content_end(line);
        let mut spans = Vec::new();
        let mut a = start;
        while a < end {
            let mut b = a;
            b.forward_to_tag_toggle(None::<&gtk::TextTag>);
            if b > end {
                b = end;
            }
            spans.push(Span::new(
                buffer.text(&a, &b, true).as_str(),
                self.style_at(&a),
            ));
            a = b;
        }
        Line {
            kind: self.0.view.kind(line),
            spans: markdown::merge(spans),
            eol: self.line_eol(line),
        }
    }

    fn models(&self, first: usize, last: usize) -> Vec<Line> {
        (first..=last.min(self.line_count() - 1))
            .map(|n| self.line_model(n))
            .collect()
    }

    /// The whole note as Markdown.
    pub fn text(&self) -> String {
        let final_eol = self
            .0
            .state
            .borrow()
            .doc
            .as_ref()
            .map_or(Eol::Lf, |d| d.final_eol);
        let count = self.line_count();
        self.check_line_table();
        // line endings straight from the text: one copy instead of thousands
        // of iterator walks
        let buffer = &self.0.buffer;
        let plain = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
        let eols: Vec<Eol> = markdown::split_lines(&plain)
            .into_iter()
            .map(|(_, eol)| eol)
            .collect();
        // a buffer ending in a line break has an empty last line that
        // split_lines does not list; its ending is `final_eol` anyway
        let eols_match = eols.len() + 1 >= count;
        let mut out = String::with_capacity(plain.len() + plain.len() / 8);
        for n in 0..count {
            let cached = self.0.view.imp().cache.borrow()[n].is_some();
            if !cached {
                let line = self.line_model(n);
                let md = markdown::serialize_line(line.kind, &line.spans);
                self.0.view.imp().cache.borrow_mut()[n] = Some(md);
            }
            out.push_str(
                self.0.view.imp().cache.borrow()[n]
                    .as_deref()
                    .unwrap_or_default(),
            );
            let eol = if n + 1 == count {
                final_eol
            } else if eols_match && n < eols.len() {
                eols[n]
            } else {
                self.line_eol(n)
            };
            out.push_str(eol.as_str());
        }
        out
    }

    fn line_eol(&self, line: usize) -> Eol {
        let end = self.line_content_end(line);
        let mut next = end;
        // forward_line() reports false when it lands on the buffer end, even
        // if that is the start of an empty last line
        next.forward_line();
        if next.line() as usize > line {
            match self.0.buffer.text(&end, &next, true).as_str() {
                "\r\n" => Eol::CrLf,
                "\r" => Eol::Cr,
                _ => Eol::Lf,
            }
        } else {
            Eol::None
        }
    }

    /// Text of the first `# ` line.
    fn title(&self) -> Option<String> {
        let kinds = self.0.view.imp().kinds.borrow();
        let line = kinds.iter().position(|k| *k == LineKind::H1)?;
        drop(kinds);
        Some(self.line_model(line).text().trim().to_string())
    }

    // -- tags ------------------------------------------------------------------

    fn apply_style(&self, start: &gtk::TextIter, end: &gtk::TextIter, style: Style) {
        for (tag, flag) in self.0.tags.char_tags() {
            if style.has(flag) {
                self.0.buffer.apply_tag(tag, start, end);
            } else {
                self.0.buffer.remove_tag(tag, start, end);
            }
        }
    }

    /// Re-applies the paragraph tags of `line` from the line table.
    fn apply_line_tags(&self, line: usize) {
        if line >= self.line_count() {
            return;
        }
        let (start, end) = self.line_span(line);
        let buffer = &self.0.buffer;
        let tags = &self.0.tags;
        for tag in tags.line_tags() {
            buffer.remove_tag(tag, &start, &end);
        }
        buffer.apply_tag(&tags.body, &start, &end);
        self.apply_kind_tags(line, &start, &end);
    }

    /// Heading / bullet paragraph tags for one line (nothing removed first).
    fn apply_kind_tags(&self, line: usize, start: &gtk::TextIter, end: &gtk::TextIter) {
        let buffer = &self.0.buffer;
        let tags = &self.0.tags;
        match self.0.view.kind(line) {
            LineKind::H1 => buffer.apply_tag(&tags.h1, start, end),
            LineKind::H2 => buffer.apply_tag(&tags.h2, start, end),
            LineKind::Bullet { indent, .. } => {
                let level = (indent as usize / 2).min(LEVELS - 1);
                buffer.apply_tag(&tags.levels[level], start, end);
                let after_bullet =
                    line > 0 && matches!(self.0.view.kind(line - 1), LineKind::Bullet { .. });
                if !after_bullet {
                    buffer.apply_tag(&tags.list_start, start, end);
                }
            }
            LineKind::Text | LineKind::Raw => {}
        }
    }

    fn refresh_lines(&self, first: usize, last: usize) {
        for n in first..=(last + 1).min(self.line_count().saturating_sub(1)) {
            self.apply_line_tags(n);
        }
        self.0.view.queue_draw();
    }

    fn set_kind(&self, line: usize, kind: LineKind) {
        if let Some(k) = self.0.view.imp().kinds.borrow_mut().get_mut(line) {
            *k = kind;
        }
        self.0.view.invalidate(line, line);
    }

    // -- buffer signals --------------------------------------------------------

    fn connect_buffer(&self) {
        let buffer = &self.0.buffer;

        let weak = self.downgrade();
        buffer.connect_insert_text(move |_, iter, text| {
            if let Some(editor) = upgrade(&weak) {
                editor.before_insert(iter, text);
            }
        });
        let weak = self.downgrade();
        buffer.connect_local("insert-text", true, move |args| {
            let editor = upgrade(&weak)?;
            let iter = args[1].get::<gtk::TextIter>().ok()?;
            let text = args[2].get::<String>().ok()?;
            editor.after_insert(&iter, &text);
            None
        });
        let weak = self.downgrade();
        buffer.connect_delete_range(move |_, start, end| {
            if let Some(editor) = upgrade(&weak) {
                editor.before_delete(start, end);
            }
        });
        let weak = self.downgrade();
        buffer.connect_local("delete-range", true, move |args| {
            let editor = upgrade(&weak)?;
            let start = args[1].get::<gtk::TextIter>().ok()?;
            editor.after_delete(&start);
            None
        });

        // any formatting change makes the line's cached Markdown stale
        for signal in ["apply-tag", "remove-tag"] {
            let weak = self.downgrade();
            buffer.connect_local(signal, true, move |args| {
                let editor = upgrade(&weak)?;
                let start = args[2].get::<gtk::TextIter>().ok()?;
                let end = args[3].get::<gtk::TextIter>().ok()?;
                editor
                    .0
                    .view
                    .invalidate(start.line() as usize, end.line() as usize);
                None
            });
        }

        let weak = self.downgrade();
        buffer.connect_mark_set(move |_, _, mark| {
            let Some(editor) = upgrade(&weak) else { return };
            match mark.name().as_deref() {
                Some("insert") => {
                    if !editor.0.state.borrow().programmatic {
                        editor.0.state.borrow_mut().typing_style = None;
                    }
                    editor.schedule_toolbar();
                    editor.0.view.queue_draw();
                }
                Some("selection_bound") => {
                    editor.schedule_toolbar();
                    editor.0.view.queue_draw();
                }
                _ => {}
            }
        });
    }

    fn before_insert(&self, iter: &gtk::TextIter, text: &str) {
        let line = iter.line() as usize;
        let breaks = line_breaks(text);
        self.0.view.invalidate(line, line);
        let programmatic = self.0.state.borrow().programmatic;
        if !programmatic {
            let before = vec![self.line_model(line)];
            let cursor = self.0.buffer.cursor_position();
            self.0.state.borrow_mut().pending = Some((line, before, cursor));
        }
        if breaks == 0 {
            return;
        }
        let kind = self.0.view.kind(line);
        let next = match kind {
            LineKind::H1 | LineKind::H2 => LineKind::Text,
            other => other,
        };
        {
            let mut cache = self.0.view.imp().cache.borrow_mut();
            let at = (line + 1).min(cache.len());
            cache.splice(at..at, std::iter::repeat_n(None, breaks));
        }
        let mut kinds = self.0.view.imp().kinds.borrow_mut();
        let at = (line + 1).min(kinds.len());
        if iter.line_offset() == 0 && !iter.ends_line() {
            // Enter at the start of a line pushes it down whole: the new
            // lines above become plain, the line keeps its kind.
            kinds[line] = next;
            kinds.splice(at..at, std::iter::repeat_n(next, breaks - 1).chain([kind]));
        } else {
            kinds.splice(at..at, std::iter::repeat_n(next, breaks));
        }
    }

    fn after_insert(&self, end: &gtk::TextIter, text: &str) {
        self.check_line_table();
        let pending = {
            let mut state = self.0.state.borrow_mut();
            if state.programmatic {
                return;
            }
            state.pending.take()
        };
        let Some((line, before, cursor_before)) = pending else {
            return;
        };
        let chars = text.chars().count() as i32;
        let start = self.0.buffer.iter_at_offset(end.offset() - chars);

        // inserted text takes the style being typed
        let style = self.typing_style_at(&start);
        let breaks = line_breaks(text);
        if breaks == 0 {
            self.apply_style(&start, end, style);
        } else {
            for n in line..=line + breaks {
                let raw = self.0.view.kind(n) == LineKind::Raw;
                let (ls, le) = self.line_span(n);
                let a = if ls < start { start } else { ls };
                let b = if le > *end { *end } else { le };
                self.apply_style(&a, &b, if raw { Style::RAW } else { style });
            }
        }
        self.refresh_lines(line, line + breaks);

        let after = self.models(line, line + breaks);
        let typing = breaks == 0 && chars == 1;
        self.push_step(Step {
            first: line,
            before,
            after,
            cursor_before,
            cursor_after: end.offset(),
            typing,
            at: Instant::now(),
        });
        self.changed();
    }

    /// Style for text typed at `iter`: a pending Ctrl+B etc., else the
    /// formatting of the character before it on the same line.
    fn typing_style_at(&self, iter: &gtk::TextIter) -> Style {
        if self.0.view.kind(iter.line() as usize) == LineKind::Raw {
            return Style::RAW;
        }
        if let Some(style) = self.0.state.borrow().typing_style {
            return style;
        }
        if iter.line_offset() == 0 {
            return Style::NONE;
        }
        let mut prev = *iter;
        prev.backward_char();
        let before = self.style_at(&prev);
        let after = if iter.ends_line() {
            Style::NONE
        } else {
            self.style_at(iter)
        };
        let mut style = before.and(Style(
            Style::MARKUP.0 | Style::ITALIC_US.0 | Style::BOLD_US.0,
        ));
        // only extend a raw span (say, a link) when typing inside it
        if before.has(Style::RAW) && after.has(Style::RAW) {
            style = style.with(Style::RAW);
        }
        style
    }

    fn before_delete(&self, start: &gtk::TextIter, end: &gtk::TextIter) {
        let (l1, l2) = (start.line() as usize, end.line() as usize);
        self.0.view.invalidate(l1, l2);
        let programmatic = self.0.state.borrow().programmatic;
        if !programmatic {
            let before = self.models(l1, l2);
            let cursor = self.0.buffer.cursor_position();
            self.0.state.borrow_mut().pending = Some((l1, before, cursor));
        }
        if l2 > l1 {
            // a line emptied from its start takes the kind of what moves in
            let merged = if start.line_offset() == 0 {
                self.0.view.kind(l2)
            } else {
                self.0.view.kind(l1)
            };
            {
                let mut cache = self.0.view.imp().cache.borrow_mut();
                let to = (l2 + 1).min(cache.len());
                cache.drain((l1 + 1).min(to)..to);
            }
            let mut kinds = self.0.view.imp().kinds.borrow_mut();
            let to = (l2 + 1).min(kinds.len());
            kinds.drain((l1 + 1).min(to)..to);
            if let Some(k) = kinds.get_mut(l1) {
                *k = merged;
            }
        }
    }

    fn after_delete(&self, at: &gtk::TextIter) {
        self.check_line_table();
        let pending = {
            let mut state = self.0.state.borrow_mut();
            if state.programmatic {
                return;
            }
            state.pending.take()
        };
        let Some((line, before, cursor_before)) = pending else {
            return;
        };
        self.refresh_lines(line, line);
        let typing = before.len() == 1 && {
            let old: usize = before[0].spans.iter().map(|s| s.text.chars().count()).sum();
            let new = self.line_model(line);
            old == new
                .spans
                .iter()
                .map(|s| s.text.chars().count())
                .sum::<usize>()
                + 1
        };
        let after = self.models(line, line);
        self.push_step(Step {
            first: line,
            before,
            after,
            cursor_before,
            cursor_after: at.offset(),
            typing,
            at: Instant::now(),
        });
        self.changed();
    }

    /// The line table must always have one entry per buffer line. Should a
    /// change ever slip past the handlers, pad or trim instead of crashing.
    fn check_line_table(&self) {
        let count = self.line_count();
        let mut cache = self.0.view.imp().cache.borrow_mut();
        if cache.len() != count {
            cache.resize(count, None);
        }
        let mut kinds = self.0.view.imp().kinds.borrow_mut();
        if kinds.len() != count {
            kinds.resize(count, LineKind::Text);
        }
    }

    // -- edits made by Kludd -----------------------------------------------------

    /// Replaces `count` lines starting at `first` with `lines`.
    fn replace_lines(&self, first: usize, count: usize, lines: &[Line]) {
        self.0.state.borrow_mut().programmatic = true;
        let buffer = &self.0.buffer;
        let total = self.line_count();
        let at_end = first + count >= total;
        let mut start = self.line_start(first);
        let mut end = if at_end {
            buffer.end_iter()
        } else {
            self.line_start(first + count)
        };
        let start_offset = start.offset();
        buffer.delete(&mut start, &mut end);

        let mut text = String::new();
        for (i, line) in lines.iter().enumerate() {
            text.push_str(&line.text());
            let last = i + 1 == lines.len();
            if !(at_end && last) {
                text.push_str(if line.eol == Eol::None {
                    "\n"
                } else {
                    line.eol.as_str()
                });
            }
        }
        // replacing the tail: the line before keeps its line break, so an
        // empty replacement must remove that break too
        if lines.is_empty() && at_end && first > 0 {
            let mut a = self.line_content_end(first - 1);
            let mut b = buffer.end_iter();
            buffer.delete(&mut a, &mut b);
        }
        let mut iter = buffer.iter_at_offset(start_offset);
        buffer.insert(&mut iter, &text);
        self.check_line_table();

        let end = buffer.iter_at_offset(start_offset + text.chars().count() as i32);
        let start = buffer.iter_at_offset(start_offset);
        buffer.remove_all_tags(&start, &end);
        buffer.apply_tag(&self.0.tags.body, &start, &end);
        let mut offset = start_offset;
        for (i, line) in lines.iter().enumerate() {
            let line_start = offset;
            self.set_kind(first + i, line.kind);
            for span in &line.spans {
                let len = span.text.chars().count() as i32;
                if span.style != Style::NONE {
                    let a = buffer.iter_at_offset(offset);
                    let b = buffer.iter_at_offset(offset + len);
                    for (tag, flag) in self.0.tags.char_tags() {
                        if span.style.has(flag) {
                            buffer.apply_tag(tag, &a, &b);
                        }
                    }
                }
                offset += len;
            }
            offset += if line.eol == Eol::None {
                1
            } else {
                line.eol.as_str().len() as i32
            };
            if line.kind != LineKind::Text && line.kind != LineKind::Raw {
                let a = buffer.iter_at_offset(line_start);
                let b = buffer.iter_at_offset(offset);
                self.apply_kind_tags(first + i, &a, &b);
            }
        }
        // the line after may start or continue a list
        self.apply_line_tags(first + lines.len());
        self.0.view.queue_draw();
        self.0.state.borrow_mut().programmatic = false;
    }

    /// Runs a tag/kind change on lines `first..=last` as one undo step.
    fn change(&self, first: usize, last: usize, f: impl FnOnce()) {
        let before = self.models(first, last);
        let cursor_before = self.0.buffer.cursor_position();
        self.0.state.borrow_mut().programmatic = true;
        f();
        self.check_line_table();
        self.0.state.borrow_mut().programmatic = false;
        let last = (first + before.len()).min(self.line_count()) - 1;
        self.refresh_lines(first.saturating_sub(1), last);
        let after = self.models(first, last);
        if after != before {
            self.push_step(Step {
                first,
                before,
                after,
                cursor_before,
                cursor_after: self.0.buffer.cursor_position(),
                typing: false,
                at: Instant::now(),
            });
            self.changed();
            self.relayout();
        }
    }

    /// Replaces lines `first..=last` with new ones as one undo step.
    fn rewrite(&self, first: usize, last: usize, lines: Vec<Line>, cursor_after: i32) {
        let before = self.models(first, last);
        let cursor_before = self.0.buffer.cursor_position();
        self.replace_lines(first, before.len(), &lines);
        self.0
            .buffer
            .place_cursor(&self.0.buffer.iter_at_offset(cursor_after));
        self.0
            .view
            .scroll_mark_onscreen(&self.0.buffer.get_insert());
        self.push_step(Step {
            first,
            before,
            after: lines,
            cursor_before,
            cursor_after,
            typing: false,
            at: Instant::now(),
        });
        self.changed();
        self.relayout();
    }

    fn push_step(&self, step: Step) {
        let mut state = self.0.state.borrow_mut();
        state.redo.clear();
        if let Some(last) = state.undo.last_mut()
            && last.typing
            && step.typing
            && last.first == step.first
            && last.after.len() == 1
            && step.before.len() == 1
            && last.cursor_after == step.cursor_before
            && step.at.duration_since(last.at) < Duration::from_secs(1)
        {
            last.after = step.after;
            last.cursor_after = step.cursor_after;
            last.at = step.at;
            return;
        }
        state.undo.push(step);
        if state.undo.len() > 500 {
            state.undo.remove(0);
        }
    }

    pub fn undo(&self) {
        let Some(step) = self.0.state.borrow_mut().undo.pop() else {
            return;
        };
        self.replace_lines(step.first, step.after.len(), &step.before);
        self.place_cursor(step.cursor_before);
        self.0.state.borrow_mut().redo.push(step);
        self.changed();
        self.relayout();
    }

    pub fn redo(&self) {
        let Some(step) = self.0.state.borrow_mut().redo.pop() else {
            return;
        };
        self.replace_lines(step.first, step.before.len(), &step.after);
        self.place_cursor(step.cursor_after);
        self.0.state.borrow_mut().undo.push(step);
        self.changed();
        self.relayout();
    }

    fn place_cursor(&self, offset: i32) {
        let buffer = &self.0.buffer;
        buffer.place_cursor(&buffer.iter_at_offset(offset));
        self.0.view.scroll_mark_onscreen(&buffer.get_insert());
    }

    /// Lines touched by the selection (or the cursor line).
    fn selected_lines(&self) -> (usize, usize) {
        let buffer = &self.0.buffer;
        match buffer.selection_bounds() {
            Some((start, end)) => {
                let mut last = end.line() as usize;
                if end.line_offset() == 0 && end.line() > start.line() {
                    last -= 1;
                }
                (start.line() as usize, last)
            }
            None => {
                let line = buffer.iter_at_mark(&buffer.get_insert()).line() as usize;
                (line, line)
            }
        }
    }

    // -- formatting commands -------------------------------------------------

    pub fn toggle_style(&self, flag: Style) {
        let buffer = &self.0.buffer;
        let Some((start, end)) = buffer.selection_bounds() else {
            let cursor = buffer.iter_at_mark(&buffer.get_insert());
            let current = self.typing_style_at(&cursor);
            let next = if current.has(flag) {
                current.without(flag)
            } else {
                current.with(flag)
            };
            self.0.state.borrow_mut().typing_style = Some(next);
            return;
        };
        let tag = match flag {
            Style::BOLD => &self.0.tags.bold,
            Style::ITALIC => &self.0.tags.italic,
            _ => &self.0.tags.underline,
        };
        let all = {
            let mut iter = start;
            let mut all = true;
            while iter < end {
                if !iter.ends_line() && !iter.has_tag(tag) {
                    all = false;
                    break;
                }
                iter.forward_char();
            }
            all
        };
        let (first, last) = (start.line() as usize, end.line() as usize);
        self.change(first, last, || {
            if all {
                buffer.remove_tag(tag, &start, &end);
                let us = if flag == Style::BOLD {
                    &self.0.tags.bold_us
                } else {
                    &self.0.tags.italic_us
                };
                buffer.remove_tag(us, &start, &end);
            } else {
                buffer.apply_tag(tag, &start, &end);
            }
        });
    }

    /// H1 / H2 / plain text for the selected lines; choosing the kind they
    /// already have turns them back into text.
    pub fn set_line_kind(&self, kind: LineKind) {
        let (first, last) = self.selected_lines();
        let all = (first..=last).all(|n| self.0.view.kind(n) == kind);
        let target = if all && kind != LineKind::Text {
            LineKind::Text
        } else {
            kind
        };
        self.change(first, last, || {
            for n in first..=last {
                if self.0.view.kind(n) != LineKind::Raw || target != LineKind::Text {
                    self.set_kind(n, target);
                }
            }
        });
    }

    pub fn toggle_bullets(&self) {
        let (first, last) = self.selected_lines();
        let all = (first..=last).all(|n| matches!(self.0.view.kind(n), LineKind::Bullet { .. }));
        self.change(first, last, || {
            for n in first..=last {
                self.set_kind(
                    n,
                    if all {
                        LineKind::Text
                    } else {
                        LineKind::bullet(0)
                    },
                );
            }
        });
    }

    /// Tab / Shift+Tab on bullets. Returns false if no bullet is selected.
    fn indent_bullets(&self, delta: i32) -> bool {
        let (first, last) = self.selected_lines();
        if !(first..=last).any(|n| matches!(self.0.view.kind(n), LineKind::Bullet { .. })) {
            return false;
        }
        self.change(first, last, || {
            for n in first..=last {
                if let LineKind::Bullet { indent, marker } = self.0.view.kind(n) {
                    let indent =
                        (indent as i32 + 2 * delta).clamp(0, 2 * (LEVELS as i32 - 1)) as u8;
                    self.set_kind(
                        n,
                        LineKind::Bullet {
                            indent: indent / 2 * 2,
                            marker,
                        },
                    );
                }
            }
        });
        true
    }

    // -- keys --------------------------------------------------------------------

    fn connect_keys(&self) {
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = self.downgrade();
        keys.connect_key_pressed(move |_, key, code, state| match upgrade(&weak) {
            Some(editor) if editor.key(key, code, state) => glib::Propagation::Stop,
            _ => glib::Propagation::Proceed,
        });
        self.0.view.add_controller(keys);
    }

    fn key(&self, key: gdk::Key, code: u32, state: gdk::ModifierType) -> bool {
        use gdk::ModifierType as M;
        let mods = state & (M::CONTROL_MASK | M::SHIFT_MASK | M::ALT_MASK | M::SUPER_MASK);
        let ctrl = mods == M::CONTROL_MASK;
        let ctrl_shift = mods == M::CONTROL_MASK | M::SHIFT_MASK;
        // hardware keycodes for the digit row, so layouts don't matter
        const KEY_1: u32 = 10;
        const KEY_2: u32 = 11;
        const KEY_8: u32 = 17;
        const KEY_0: u32 = 19;

        if ctrl {
            match key.to_lower() {
                gdk::Key::b => self.toggle_style(Style::BOLD),
                gdk::Key::i => self.toggle_style(Style::ITALIC),
                gdk::Key::u => self.toggle_style(Style::UNDERLINE),
                gdk::Key::z => self.undo(),
                gdk::Key::y => self.redo(),
                _ => match code {
                    KEY_1 => self.set_line_kind(LineKind::H1),
                    KEY_2 => self.set_line_kind(LineKind::H2),
                    KEY_0 => self.set_line_kind(LineKind::Text),
                    _ => return false,
                },
            }
            return true;
        }
        if ctrl_shift {
            match (key.to_lower(), code) {
                (gdk::Key::z, _) => self.redo(),
                (_, KEY_8) => self.toggle_bullets(),
                _ => return false,
            }
            return true;
        }
        if !mods.is_empty() && mods != M::SHIFT_MASK {
            return false;
        }
        match key {
            gdk::Key::Tab if mods.is_empty() => self.indent_bullets(1),
            gdk::Key::ISO_Left_Tab | gdk::Key::Tab => self.indent_bullets(-1),
            gdk::Key::space if mods.is_empty() => self.autoformat(),
            gdk::Key::Return | gdk::Key::KP_Enter => self.enter(),
            gdk::Key::BackSpace if mods.is_empty() => self.backspace(),
            _ => false,
        }
    }

    fn cursor_line(&self) -> Option<(usize, gtk::TextIter)> {
        let buffer = &self.0.buffer;
        if buffer.has_selection() {
            return None;
        }
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        Some((cursor.line() as usize, cursor))
    }

    /// `- ` / `* ` start a bullet, `# ` / `## ` a heading.
    fn autoformat(&self) -> bool {
        let Some((line, cursor)) = self.cursor_line() else {
            return false;
        };
        if self.0.view.kind(line) != LineKind::Text {
            return false;
        }
        let start = self.line_start(line);
        let typed = self.0.buffer.text(&start, &cursor, true);
        let kind = match typed.as_str() {
            "-" | "*" => LineKind::bullet(0),
            "#" => LineKind::H1,
            "##" => LineKind::H2,
            _ => return false,
        };
        self.change(line, line, || {
            let mut a = self.line_start(line);
            let mut b = self
                .0
                .buffer
                .iter_at_offset(a.offset() + typed.chars().count() as i32);
            self.0.buffer.delete(&mut a, &mut b);
            self.set_kind(line, kind);
        });
        true
    }

    /// Enter on an empty bullet ends the list.
    fn enter(&self) -> bool {
        let Some((line, cursor)) = self.cursor_line() else {
            return false;
        };
        let is_bullet = matches!(self.0.view.kind(line), LineKind::Bullet { .. });
        if is_bullet && cursor.line_offset() == 0 && cursor.ends_line() {
            self.change(line, line, || self.set_kind(line, LineKind::Text));
            return true;
        }
        false
    }

    /// Backspace at the start of a bullet outdents it, then turns it into
    /// text; at the start of a heading it turns it into text.
    fn backspace(&self) -> bool {
        let Some((line, cursor)) = self.cursor_line() else {
            return false;
        };
        if cursor.line_offset() != 0 {
            return false;
        }
        match self.0.view.kind(line) {
            LineKind::Bullet { indent, marker } if indent >= 2 => {
                let kind = LineKind::Bullet {
                    indent: indent - 2,
                    marker,
                };
                self.change(line, line, || self.set_kind(line, kind));
            }
            LineKind::Bullet { .. } | LineKind::H1 | LineKind::H2 => {
                self.change(line, line, || self.set_kind(line, LineKind::Text));
            }
            _ => return false,
        }
        true
    }

    // -- clipboard: Markdown in, Markdown out ---------------------------------

    fn connect_clipboard(&self) {
        let view = &self.0.view;
        let weak = self.downgrade();
        view.connect_copy_clipboard(move |view| {
            view.stop_signal_emission_by_name("copy-clipboard");
            if let Some(editor) = upgrade(&weak) {
                editor.copy();
            }
        });
        let weak = self.downgrade();
        view.connect_cut_clipboard(move |view| {
            view.stop_signal_emission_by_name("cut-clipboard");
            if let Some(editor) = upgrade(&weak)
                && editor.copy()
            {
                editor.0.buffer.delete_selection(true, true);
            }
        });
        let weak = self.downgrade();
        view.connect_paste_clipboard(move |view| {
            view.stop_signal_emission_by_name("paste-clipboard");
            let Some(editor) = upgrade(&weak) else { return };
            let clipboard = view.clipboard();
            glib::spawn_future_local(async move {
                if let Ok(Some(text)) = clipboard.read_text_future().await {
                    editor.paste(&text);
                }
            });
        });
    }

    fn copy(&self) -> bool {
        let Some((start, end)) = self.0.buffer.selection_bounds() else {
            return false;
        };
        let mut lines = self.models(start.line() as usize, end.line() as usize);
        let count = lines.len();
        let end_offset = end.line_offset() as usize;
        if let Some(last) = lines.last_mut() {
            truncate_spans(&mut last.spans, end_offset);
            last.eol = Eol::None;
        }
        if count == 1 {
            let start_offset = start.line_offset() as usize;
            lines[0].spans = split_spans(&lines[0].spans, start_offset).1;
        } else {
            let first = &mut lines[0];
            let start_offset = start.line_offset() as usize;
            if start_offset > 0 {
                first.spans = split_spans(&first.spans, start_offset).1;
                if first.kind != LineKind::Raw {
                    first.kind = LineKind::Text;
                }
            }
        }
        if start.line_offset() > 0 && lines[0].kind != LineKind::Raw {
            lines[0].kind = LineKind::Text;
        }
        self.0
            .view
            .clipboard()
            .set_text(&markdown::serialize(&lines));
        true
    }

    /// Pasted text is read as Markdown.
    fn paste(&self, text: &str) {
        let buffer = &self.0.buffer;
        if buffer.has_selection() {
            buffer.delete_selection(true, true);
        }
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        let line = cursor.line() as usize;
        let offset = cursor.line_offset() as usize;
        let current = self.line_model(line);
        let (left, right) = split_spans(&current.spans, offset);
        let mut pasted = markdown::parse(text);
        if pasted.is_empty() {
            return;
        }
        // whitespace kept verbatim by the parser is plain text once pasted
        for line in pasted.iter_mut().filter(|l| l.kind != LineKind::Raw) {
            for span in &mut line.spans {
                if span.text.trim().is_empty() {
                    span.style = span.style.without(Style::RAW);
                }
            }
        }
        // text ending in a line break leaves the cursor on a fresh line
        if pasted.last().is_some_and(|l| l.eol != Eol::None) {
            pasted.push(Line {
                kind: LineKind::Text,
                spans: Vec::new(),
                eol: Eol::None,
            });
        }
        let n = pasted.len();

        // first pasted line joins what is left of the cursor
        let first_kind = if left.is_empty() && current.kind == LineKind::Text {
            pasted[0].kind
        } else {
            current.kind
        };
        let mut first_spans = left;
        first_spans.extend(std::mem::take(&mut pasted[0].spans));
        pasted[0].spans = markdown::merge(first_spans);
        pasted[0].kind = first_kind;

        // the rest of the current line follows the last pasted line
        let cursor_chars: usize = pasted[n - 1]
            .spans
            .iter()
            .map(|s| s.text.chars().count())
            .sum();
        let mut last_spans = std::mem::take(&mut pasted[n - 1].spans);
        last_spans.extend(right);
        pasted[n - 1].spans = markdown::merge(last_spans);
        pasted[n - 1].eol = current.eol;
        for line in pasted.iter_mut().take(n - 1) {
            if line.eol == Eol::None {
                line.eol = Eol::Lf;
            }
        }

        let line_start = self.line_start(line).offset();
        let before_last: i32 = pasted
            .iter()
            .take(n - 1)
            .map(|l| (l.text().chars().count() + l.eol.as_str().len()) as i32)
            .sum();
        let cursor_after = line_start + before_last + cursor_chars as i32;
        self.rewrite(line, line, pasted, cursor_after);
    }

    // -- format toolbar ------------------------------------------------------

    fn build_toolbar(&self) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        let buttons: [(&str, &str); 6] = [
            ("B", "b"),
            ("I", "i"),
            ("U", "u"),
            ("•", "dot"),
            ("H1", "h1"),
            ("H2", "h2"),
        ];
        for (label, class) in buttons {
            let button = gtk::Button::builder()
                .label(label)
                .focus_on_click(false)
                .can_focus(false)
                .css_classes([class])
                .build();
            button.set_cursor_from_name(Some("pointer"));
            let weak = self.downgrade();
            button.connect_clicked(move |_| {
                let Some(editor) = upgrade(&weak) else { return };
                match label {
                    "B" => editor.toggle_style(Style::BOLD),
                    "I" => editor.toggle_style(Style::ITALIC),
                    "U" => editor.toggle_style(Style::UNDERLINE),
                    "•" => editor.toggle_bullets(),
                    "H1" => editor.set_line_kind(LineKind::H1),
                    _ => editor.set_line_kind(LineKind::H2),
                }
            });
            row.append(&button);
        }
        self.0.toolbar.set_child(Some(&row));
    }

    /// The toolbar follows the selection, but never while the mouse is
    /// still dragging one out or the text is scrolling.
    fn connect_pointer(&self) {
        let legacy = gtk::EventControllerLegacy::new();
        legacy.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = self.downgrade();
        legacy.connect_event(move |_, event| {
            if let Some(editor) = upgrade(&weak) {
                match event.event_type() {
                    gdk::EventType::ButtonPress => {
                        editor.0.state.borrow_mut().pointer_down = true;
                        editor.hide_toolbar();
                    }
                    gdk::EventType::ButtonRelease => {
                        editor.0.state.borrow_mut().pointer_down = false;
                        editor.schedule_toolbar();
                    }
                    _ => {}
                }
            }
            glib::Propagation::Proceed
        });
        self.0.view.add_controller(legacy);

        if let Some(adjustment) = self.0.view.vadjustment() {
            let weak = self.downgrade();
            adjustment.connect_value_changed(move |_| {
                if let Some(editor) = upgrade(&weak)
                    && editor.0.toolbar.is_visible()
                {
                    editor.hide_toolbar();
                    editor.schedule_toolbar();
                }
            });
        }
    }

    /// Hides the toolbar at once; shows it again once the selection settles.
    fn schedule_toolbar(&self) {
        if let Some(id) = self.0.state.borrow_mut().toolbar_timer.take() {
            id.remove();
        }
        let dragging = self.0.state.borrow().pointer_down;
        if !self.0.buffer.has_selection() || dragging {
            self.0.toolbar.popdown();
            return;
        }
        let weak = self.downgrade();
        let id = glib::timeout_add_local_once(Duration::from_millis(120), move || {
            if let Some(editor) = upgrade(&weak) {
                editor.0.state.borrow_mut().toolbar_timer = None;
                editor.show_toolbar();
            }
        });
        self.0.state.borrow_mut().toolbar_timer = Some(id);
    }

    fn show_toolbar(&self) {
        let view = &self.0.view;
        let Some((start, end)) = self.0.buffer.selection_bounds() else {
            return;
        };
        if !view.is_mapped() || self.0.state.borrow().pointer_down {
            return;
        }
        // anchor on the first selected line that is on screen
        let visible = view.visible_rect();
        let (top, _) = view.line_at_y(visible.y());
        let first = if start > top { start } else { top };
        if first >= end {
            self.0.toolbar.popdown();
            return;
        }
        let glyphs = view.iter_location(&first);
        let (y, height) = view.glyph_box(&glyphs);
        let (x, y) =
            view.buffer_to_window_coords(gtk::TextWindowType::Widget, glyphs.x(), y as i32);
        if y > view.height() {
            self.0.toolbar.popdown();
            return;
        }
        // a popover measures as 0; its row plus the 6px side padding is the width
        let (width, toolbar_height) = self.0.toolbar.child().map_or((0, 0), |c| {
            (
                c.measure(gtk::Orientation::Horizontal, -1).1 + 12,
                c.measure(gtk::Orientation::Vertical, -1).1 + 8,
            )
        });
        // like the design: just right of the selection start, above it;
        // below the line when there is no room above
        let x = (x + 7).min(view.width() - width).max(0);
        let room_above = y >= toolbar_height + 2;
        let (position, rect) = if room_above {
            (
                gtk::PositionType::Top,
                gdk::Rectangle::new(x, y, width.max(1), 1),
            )
        } else {
            (
                gtk::PositionType::Bottom,
                gdk::Rectangle::new(x, y + height as i32, width.max(1), 1),
            )
        };
        self.0.toolbar.set_position(position);
        self.0.toolbar.set_pointing_to(Some(&rect));
        if self.0.toolbar.is_visible() {
            self.0.toolbar.present();
        } else {
            self.0.toolbar.popup();
        }
    }

    pub fn hide_toolbar(&self) {
        if let Some(id) = self.0.state.borrow_mut().toolbar_timer.take() {
            id.remove();
        }
        self.0.toolbar.popdown();
    }

    // -- documents -------------------------------------------------------------

    /// Shows `lines` with an empty history.
    fn load(&self, lines: &[Line], cursor: i32) {
        self.hide_toolbar();
        let buffer = &self.0.buffer;
        {
            let mut state = self.0.state.borrow_mut();
            state.programmatic = true;
            state.undo.clear();
            state.redo.clear();
            state.typing_style = None;
        }
        buffer.set_text("");
        self.0.view.imp().kinds.replace(vec![LineKind::Text]);
        self.0.view.imp().cache.replace(vec![None]);
        self.0.state.borrow_mut().programmatic = false;
        self.replace_lines(0, 1, lines);
        let cursor = cursor.clamp(0, buffer.char_count());
        buffer.place_cursor(&buffer.iter_at_offset(cursor));
        self.0.view.scroll_mark_onscreen(&buffer.get_insert());
    }

    /// Opens a file. `managed` notes (inside the notes folder) are renamed
    /// after their title; others are edited where they are.
    pub fn open(&self, path: &Path, managed: bool) -> std::io::Result<()> {
        self.close();
        let text = std::fs::read_to_string(path)?;
        let lines = markdown::parse(&text);
        let final_eol = lines.last().map_or(Eol::None, |l| l.eol);
        self.0.state.borrow_mut().doc = Some(Doc {
            path: Some(path.to_path_buf()),
            managed,
            created: false,
            final_eol,
            saved: text.clone(),
            dirty: false,
        });
        self.load(&lines, text.chars().count() as i32);
        // cursor at the end of the text, not after a trailing newline
        let buffer = &self.0.buffer;
        buffer.place_cursor(&buffer.end_iter());
        self.0.view.scroll_mark_onscreen(&buffer.get_insert());
        self.emit_status();
        Ok(())
    }

    /// Starts a new note with an empty title line.
    pub fn new_note(&self) {
        self.close();
        self.0.state.borrow_mut().doc = Some(Doc {
            path: None,
            managed: true,
            created: true,
            final_eol: Eol::Lf,
            saved: String::new(),
            dirty: false,
        });
        let title = Line {
            kind: LineKind::H1,
            spans: Vec::new(),
            eol: Eol::None,
        };
        self.load(&[title], 0);
        self.emit_status();
    }

    /// Saves and forgets the current note. A note created this session and
    /// left empty is removed again (it holds nothing worth a trash entry).
    pub fn close(&self) {
        self.hide_toolbar();
        self.save_sync();
        let doc = self.0.state.borrow_mut().doc.take();
        if let Some(doc) = doc
            && doc.created
            && is_blank(&doc.saved)
            && let Some(path) = doc.path
            && std::fs::read_to_string(&path).is_ok_and(|t| is_blank(&t))
        {
            let _ = std::fs::remove_file(&path);
        }
    }

    pub fn status_path(&self) -> String {
        let state = self.0.state.borrow();
        match state.doc.as_ref() {
            Some(Doc {
                path: Some(path), ..
            }) => config::tilde_path(path),
            Some(_) => {
                drop(state);
                let slug = storage::slugify(&self.title().unwrap_or_default());
                config::tilde_path(&self.0.notes_dir.borrow().join(format!("{slug}.md")))
            }
            None => String::new(),
        }
    }

    fn changed(&self) {
        let mut state = self.0.state.borrow_mut();
        let Some(doc) = state.doc.as_mut() else {
            return;
        };
        doc.dirty = true;
        if let Some(id) = state.save_timer.take() {
            id.remove();
        }
        let weak = self.downgrade();
        state.save_timer = Some(glib::timeout_add_local_once(SAVE_DELAY, move || {
            if let Some(editor) = upgrade(&weak) {
                editor.0.state.borrow_mut().save_timer = None;
                editor.save();
            }
        }));
    }

    /// Decides where the note goes (renaming it after its title) and
    /// returns what to write, or `None` if nothing needs writing.
    fn prepare_save(&self) -> Option<(PathBuf, String)> {
        let text = {
            let state = self.0.state.borrow();
            let doc = state.doc.as_ref()?;
            if !doc.dirty {
                return None;
            }
            drop(state);
            self.text()
        };
        let title = self.title();
        let mut state = self.0.state.borrow_mut();
        let doc = state.doc.as_mut()?;
        doc.dirty = false;
        if text == doc.saved && doc.path.is_some() {
            return None;
        }
        if doc.path.is_none() && is_blank(&text) {
            return None;
        }
        if doc.managed {
            let dir = self.0.notes_dir.borrow().clone();
            let slug = match title.filter(|t| !t.is_empty()) {
                Some(title) => Some(storage::slugify(&title)),
                None if doc.path.is_none() => Some("untitled".to_string()),
                None => None,
            };
            if let Some(slug) = slug {
                let _ = std::fs::create_dir_all(&dir);
                let want = storage::unique_path(&dir, &slug, doc.path.as_deref());
                match &doc.path {
                    Some(old) if *old != want => {
                        if std::fs::rename(old, &want).is_ok() {
                            doc.path = Some(want);
                        }
                    }
                    Some(_) => {}
                    None => doc.path = Some(want),
                }
            }
        }
        doc.saved = text.clone();
        Some((doc.path.clone()?, text))
    }

    /// Autosave: writes off the main thread.
    pub fn save(&self) {
        if self.0.state.borrow().writing {
            self.0.state.borrow_mut().save_again = true;
            return;
        }
        let Some((path, text)) = self.prepare_save() else {
            return;
        };
        self.emit_status();
        self.0.state.borrow_mut().writing = true;
        let weak = self.downgrade();
        glib::spawn_future_local(async move {
            let target = path.clone();
            let result = gio::spawn_blocking(move || storage::write_atomic(&target, &text)).await;
            let Some(editor) = upgrade(&weak) else { return };
            editor.0.state.borrow_mut().writing = false;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => editor.save_failed(&path, e.to_string()),
                Err(_) => editor.save_failed(&path, "writer crashed".into()),
            }
            let again = std::mem::take(&mut editor.0.state.borrow_mut().save_again);
            if again {
                editor.save();
            }
        });
    }

    fn save_failed(&self, path: &Path, error: String) {
        if let Some(doc) = self.0.state.borrow_mut().doc.as_mut() {
            doc.dirty = true;
            doc.saved.clear();
        }
        self.emit_error(format!(
            "save failed: {} ({error})",
            config::tilde_path(path)
        ));
    }

    /// Saves right now, blocking. Used when hiding, closing and quitting.
    pub fn save_sync(&self) {
        if let Some(id) = self.0.state.borrow_mut().save_timer.take() {
            id.remove();
        }
        if let Some((path, text)) = self.prepare_save() {
            if let Err(e) = storage::write_atomic(&path, &text) {
                self.save_failed(&path, e.to_string());
            }
            self.emit_status();
        }
    }

    /// A file changed on disk: reload it if it is the open note and has no
    /// unsaved edits.
    pub fn file_changed(&self, path: &Path) {
        let (cursor, final_eol) = {
            let state = self.0.state.borrow();
            let Some(doc) = state.doc.as_ref() else {
                return;
            };
            if doc.path.as_deref() != Some(path)
                || doc.dirty
                || state.writing
                || state.save_timer.is_some()
            {
                return;
            }
            let Ok(text) = std::fs::read_to_string(path) else {
                return;
            };
            if text == doc.saved {
                return;
            }
            drop(state);
            let lines = markdown::parse(&text);
            let final_eol = lines.last().map_or(Eol::None, |l| l.eol);
            let cursor = self.0.buffer.cursor_position();
            self.load(&lines, cursor);
            if let Some(doc) = self.0.state.borrow_mut().doc.as_mut() {
                doc.saved = text;
            }
            (cursor, final_eol)
        };
        if let Some(doc) = self.0.state.borrow_mut().doc.as_mut() {
            doc.final_eol = final_eol;
        }
        self.place_cursor(cursor);
    }
}

fn upgrade(weak: &Weak<Inner>) -> Option<Editor> {
    weak.upgrade().map(Editor)
}

/// Line breaks as GTK counts them (`\r\n` is one).
fn line_breaks(text: &str) -> usize {
    let mut count = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' | '\u{2029}' => count += 1,
            '\r' => {
                count += 1;
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            _ => {}
        }
    }
    count
}

/// A note is blank if it holds nothing but `#`s and whitespace.
fn is_blank(text: &str) -> bool {
    text.chars().all(|c| c == '#' || c.is_whitespace())
}

/// Splits spans at a character offset.
fn split_spans(spans: &[Span], at: usize) -> (Vec<Span>, Vec<Span>) {
    let mut left = Vec::new();
    let mut right = Vec::new();
    let mut pos = 0;
    for span in spans {
        let len = span.text.chars().count();
        if pos + len <= at {
            left.push(span.clone());
        } else if pos >= at {
            right.push(span.clone());
        } else {
            let cut = span
                .text
                .char_indices()
                .nth(at - pos)
                .map_or(span.text.len(), |(i, _)| i);
            left.push(Span::new(&span.text[..cut], span.style));
            right.push(Span::new(&span.text[cut..], span.style));
        }
        pos += len;
    }
    (left, right)
}

fn truncate_spans(spans: &mut Vec<Span>, at: usize) {
    *spans = split_spans(spans, at).0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_breaks_like_gtk() {
        assert_eq!(line_breaks("a\nb"), 1);
        assert_eq!(line_breaks("a\r\nb\rc\n"), 3);
        assert_eq!(line_breaks("plain"), 0);
    }

    #[test]
    fn splits_spans() {
        let spans = vec![
            Span::new("héllo", Style::BOLD),
            Span::new(" world", Style::NONE),
        ];
        let (l, r) = split_spans(&spans, 2);
        assert_eq!(l, vec![Span::new("hé", Style::BOLD)]);
        assert_eq!(
            r,
            vec![
                Span::new("llo", Style::BOLD),
                Span::new(" world", Style::NONE)
            ]
        );
    }

    #[test]
    fn blank_notes() {
        assert!(is_blank("# \n"));
        assert!(is_blank(""));
        assert!(!is_blank("# a"));
    }

    // -- the editor itself, driven like a user would (no window needed) ----------

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kludd-editor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Types like the keyboard does: text goes in at the cursor through the
    /// interactive path, Enter/Space/Backspace get Kludd's key handling.
    fn type_text(editor: &Editor, text: &str) {
        let buffer = &editor.0.buffer;
        for c in text.chars() {
            let handled = match c {
                ' ' => editor.autoformat(),
                '\n' => editor.enter(),
                _ => false,
            };
            if !handled {
                buffer.insert_interactive_at_cursor(&c.to_string(), true);
            }
        }
    }

    fn select(editor: &Editor, from: i32, to: i32) {
        let buffer = &editor.0.buffer;
        buffer.select_range(&buffer.iter_at_offset(from), &buffer.iter_at_offset(to));
    }

    fn find(editor: &Editor, needle: &str) -> i32 {
        let buffer = &editor.0.buffer;
        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
        text[..text.find(needle).unwrap()].chars().count() as i32
    }

    /// The cached Markdown must always equal a fresh serialization.
    fn check_cache(editor: &Editor) {
        let cached = editor.text();
        editor
            .0
            .view
            .imp()
            .cache
            .borrow_mut()
            .iter_mut()
            .for_each(|c| *c = None);
        assert_eq!(cached, editor.text(), "line cache went stale");
    }

    /// GTK needs one thread and a display, so all editor checks share one
    /// test. Without a display (CI) it is skipped.
    #[test]
    fn editor_behaves_like_a_notepad() {
        if gtk::init().is_err() {
            eprintln!("no display: skipping GTK editor test");
            return;
        }
        let dir = temp_dir("notes");
        let editor = Editor::new(dir.clone());

        // a new note starts on an empty title line
        editor.new_note();
        type_text(
            &editor,
            "Things to fix\nCall the dentist before Friday.\n- Buy milk\nPush to GitHub\n\nLater",
        );
        assert_eq!(
            editor.text(),
            "# Things to fix\nCall the dentist before Friday.\n- Buy milk\n- Push to GitHub\nLater\n",
            "Enter after a heading gives text, '- ' starts a bullet, Enter on an empty bullet ends the list"
        );

        // formatting a selection, and Ctrl+2 on a line
        let at = find(&editor, "dentist");
        select(&editor, at, at + 7);
        editor.toggle_style(Style::BOLD);
        let at = find(&editor, "Friday");
        select(&editor, at, at + 6);
        editor.toggle_style(Style::UNDERLINE);
        let at = find(&editor, "Later");
        editor.place_cursor(at);
        editor.set_line_kind(LineKind::H2);
        let expected = "# Things to fix\nCall the **dentist** before <u>Friday</u>.\n- Buy milk\n- Push to GitHub\n## Later\n";
        assert_eq!(editor.text(), expected);

        check_cache(&editor);
        // typing continues the formatting it starts in
        let at = find(&editor, "dentist") + 7;
        editor.place_cursor(at);
        type_text(&editor, "s");
        assert!(editor.text().contains("**dentists**"));
        editor.undo();
        assert_eq!(editor.text(), expected);

        // undo/redo covers formatting, not just text
        editor.undo(); // ## Later -> Later
        editor.undo(); // underline
        assert_eq!(
            editor.text(),
            "# Things to fix\nCall the **dentist** before Friday.\n- Buy milk\n- Push to GitHub\nLater\n"
        );
        editor.redo();
        editor.redo();
        assert_eq!(editor.text(), expected);

        check_cache(&editor);
        // Tab / Shift+Tab nest bullets
        let at = find(&editor, "Push");
        editor.place_cursor(at);
        assert!(editor.indent_bullets(1));
        assert!(editor.text().contains("- Buy milk\n  - Push to GitHub\n"));
        assert!(editor.indent_bullets(-1));
        assert!(editor.backspace(), "Backspace at the start of a bullet");
        assert!(editor.text().contains("- Buy milk\nPush to GitHub\n"));
        editor.undo();
        assert_eq!(editor.text(), expected);

        check_cache(&editor);
        // saving names the file after the title, atomically
        editor.save_sync();
        let path = dir.join("things-to-fix.md");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no temp files left behind"
        );

        // renaming the title renames the file, avoiding collisions
        std::fs::write(dir.join("chores.md"), "# Chores\n").unwrap();
        editor.place_cursor(find(&editor, "Things to fix") + 13);
        for _ in 0..13 {
            editor.0.buffer.backspace(
                &mut editor.0.buffer.iter_at_mark(&editor.0.buffer.get_insert()),
                true,
                true,
            );
        }
        type_text(&editor, "Chores");
        editor.save_sync();
        assert!(!path.exists());
        let renamed = dir.join("chores-2.md");
        assert!(editor.text().starts_with("# Chores\n"));
        assert_eq!(std::fs::read_to_string(&renamed).unwrap(), editor.text());

        check_cache(&editor);
        // pasting Markdown pastes formatting
        editor.place_cursor(editor.0.buffer.char_count());
        editor.paste(" *now*");
        type_text(&editor, "\n");
        editor.paste("one *two*\n- three\n");
        assert!(
            editor
                .text()
                .ends_with("## Later *now*\none *two*\n- three\n\n"),
            "{:?}",
            editor.text()
        );
        editor.undo();
        assert!(
            editor.text().ends_with("## Later *now*\n\n"),
            "{}",
            editor.text()
        );

        check_cache(&editor);
        // an edit made elsewhere (say, nvim) is picked up
        editor.save_sync();
        std::fs::write(&renamed, "# Chores\nchanged outside\n").unwrap();
        editor.file_changed(&renamed);
        assert_eq!(editor.text(), "# Chores\nchanged outside\n");

        // opening an outside file and saving it changes nothing
        let outside = temp_dir("outside").join("README.md");
        let readme = "Intro\r\n\r\n```sh\ncargo build\n```\n\n| a | b |\n|---|---|\n\n1. one\n2. two\n[link](x) and **bold**";
        std::fs::write(&outside, readme).unwrap();
        editor.open(&outside, false).unwrap();
        assert_eq!(editor.text(), readme);
        editor.place_cursor(editor.0.buffer.char_count());
        type_text(&editor, "!");
        editor.save_sync();
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            format!("{readme}!")
        );
        assert!(
            outside.exists() && !dir.join("readme.md").exists(),
            "outside files are never moved"
        );

        check_cache(&editor);
        // a new note left empty leaves nothing behind
        editor.new_note();
        type_text(&editor, "x");
        editor.save_sync();
        assert!(dir.join("x.md").exists());
        editor.undo();
        editor.close();
        assert!(!dir.join("x.md").exists());

        // performance: a 5 000 line note
        let mut big = String::new();
        for i in 0..5_000 {
            match i % 5 {
                0 => big.push_str(&format!("## Section {i}\n")),
                1 => big.push_str(&format!("- item **{i}** with *style*\n")),
                2 => {
                    big.push_str("plain text line that is long enough to wrap a little bit maybe\n")
                }
                3 => big.push_str("`code` and [a link](https://example.com)\n"),
                _ => big.push('\n'),
            }
        }
        let big_path = dir.join("big.md");
        std::fs::write(&big_path, &big).unwrap();
        let t = Instant::now();
        editor.open(&big_path, true).unwrap();
        let open = t.elapsed();
        assert_eq!(editor.text(), big);
        let t = Instant::now();
        editor.place_cursor(find(&editor, "Section 2500"));
        for _ in 0..200 {
            type_text(&editor, "a");
        }
        let per_key = t.elapsed() / 200;
        check_cache(&editor);
        let _ = editor.text();
        let t = Instant::now();
        let _ = editor.text();
        let serialize = t.elapsed();
        eprintln!("5000 lines: open {open:?}, per keystroke {per_key:?}, serialize {serialize:?}");
        assert!(
            per_key < Duration::from_millis(4),
            "typing too slow: {per_key:?}"
        );
        editor.close();
    }
}
