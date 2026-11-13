//! The note editor: a GtkTextView whose lines map 1:1 onto Markdown lines.
//!
//! Inline formatting lives in text tags. Each line's kind (text, heading,
//! bullet, raw) lives in a side table on the view, kept in step with the
//! buffer from its insert/delete signals, and is mirrored into paragraph
//! tags for rendering. GTK's own undo ignores tags, so undo is done here by
//! recording line snapshots before and after every change.
//!
//! Code blocks, tables, rules, math, code spans, links, images and the like
//! are raw Markdown underneath; the view only draws them differently
//! (boxes, pills, underlines, hidden syntax), so what is saved never depends
//! on how it looks.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib, graphene, gsk, pango};

use crate::config;
use crate::find;
use crate::markdown::{
    self, Alert, Block, Eol, InlineKind, Line, LineKind, Shape, Span, Style, Task,
};
use crate::scroll;
use crate::stats;
use crate::storage;
use crate::theme::{self, Icon};

/// Indent levels that get their own margin tag; deeper bullets share the last.
const LEVELS: usize = 8;
/// CSS line-height 1.55 expressed as Pango's factor of Fredoka's
/// ascent+descent (1.21 em).
const LINE_HEIGHT: f64 = 1.55 / 1.21;
const SAVE_DELAY: Duration = Duration::from_millis(400);
/// Code and tables: JetBrains Mono at 13px next to 15px text, like GitHub.
const MONO: &str = "JetBrains Mono, monospace";
const MONO_SCALE: f64 = 13.0 / 15.0;
/// Space between a code block or table and the text around it, and inside
/// a code block above and below its fences.
const BLOCK_GAP: f64 = 6.0;
const CODE_PAD: f64 = 4.0;
/// How far the wrapped part of a table row too wide for the window is
/// indented, so it reads as the same row, inside the table's box.
const TABLE_WRAP_INDENT: f64 = 16.0;
/// Room above and below each table row.
const ROW_PAD: f64 = 3.0;
/// How far each quote level is indented, and the room above and below a
/// quote.
const QUOTE_INDENT: f64 = 16.0;
const QUOTE_GAP: f64 = 4.0;
/// A task's box, and the room around pictures shown under their line.
const TASK_BOX: f32 = 13.0;
const PICTURE_GAP: f64 = 8.0;

/// GitHub's alert colours, which read on every theme.
fn alert_color(alert: Alert) -> gdk::RGBA {
    let hex = match alert {
        Alert::Note => "#3B82F6",
        Alert::Tip => "#1F9D60",
        Alert::Important => "#8B5CF6",
        Alert::Warning => "#D08700",
        Alert::Caution => "#E5484D",
    };
    gdk::RGBA::parse(hex).unwrap_or(gdk::RGBA::BLACK)
}

fn alert_icon(alert: Alert) -> &'static str {
    match alert {
        Alert::Note => theme::ICON_NOTE,
        Alert::Tip => theme::ICON_TIP,
        Alert::Important => theme::ICON_IMPORTANT,
        Alert::Warning => theme::ICON_WARNING,
        Alert::Caution => theme::ICON_CAUTION,
    }
}

/// The left margin of a numbered item: room for its list's widest number.
fn number_margin(level: usize, digits: u8, zoom: f64) -> f64 {
    let gutter = (digits.max(1) as f64 * 0.6 + 0.3) * 15.0 + 6.0;
    (20.0 * level as f64 + gutter.max(20.0)) * zoom
}

/// Pictures shown under a line: (texture, width, height).
type Pictures = Vec<(gdk::Texture, f32, f32)>;
type PicturesOf = Box<dyn Fn(usize) -> Pictures>;

// ---------------------------------------------------------------------------
// The view: draws its own selection (rounded, glyph height), list markers,
// quotes, rules, boxes and pictures.

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct View {
        pub kinds: RefCell<Vec<LineKind>>,
        /// Each line's part in a code block or table, kept like `kinds`.
        pub blocks: RefCell<Vec<Block>>,
        /// Each line's list nesting and number, quote edges; kept like
        /// `kinds`.
        pub shapes: RefCell<Vec<Shape>>,
        /// The pictures to show under a line (set by the editor, which
        /// knows where the note is).
        pub pictures: RefCell<Option<PicturesOf>>,
        /// Markdown of each line, kept until the line changes, so saving
        /// a long note only serializes what was edited.
        pub cache: RefCell<Vec<Option<String>>>,
        pub zoom: Cell<f64>,
        /// Selection colour of the current theme.
        pub accent: RefCell<Option<gdk::RGBA>>,
        /// Text colour of the current theme.
        pub ink: RefCell<Option<gdk::RGBA>>,
        /// Find in the note: the matches as `[start, end)` buffer offsets,
        /// in order, and the one gone to.
        pub finds: RefCell<Vec<(i32, i32)>>,
        pub find_current: Cell<Option<usize>>,
        /// The last line's `line_directions`, with what they came from: its
        /// start, its text and the part of it shown.
        pub(super) directions: RefCell<Option<(i32, String, String, super::Directions)>>,
        /// While drawing, where the text cannot change: `line_directions`
        /// of each line asked about, by its start.
        pub(super) drawing: RefCell<Option<super::DrawnLines>>,
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
                self.drawing.replace(Some(Vec::new()));
                view.draw_blocks(&snapshot);
                view.draw_quotes(&snapshot);
                view.draw_boxes(&snapshot);
                view.draw_marks(&snapshot);
                view.draw_finds(&snapshot);
                view.draw_selection(&snapshot);
                view.draw_links(&snapshot);
                view.draw_markers(&snapshot);
                view.draw_pictures(&snapshot);
                view.draw_emoji(&snapshot);
                self.drawing.replace(None);
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
        view.imp().blocks.replace(vec![Block::None]);
        view.imp().shapes.replace(vec![Shape::default()]);
        view.imp().cache.replace(vec![None]);
        view
    }

    fn shape(&self, line: usize) -> Shape {
        self.imp()
            .shapes
            .borrow()
            .get(line)
            .copied()
            .unwrap_or_default()
    }

    fn block(&self, line: usize) -> Block {
        self.imp()
            .blocks
            .borrow()
            .get(line)
            .copied()
            .unwrap_or_default()
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
            return self.cursor_at(line).x();
        }
        // a wrapped row ends where the next one starts, which GTK places
        // at the start of that next row
        if !content_end.ends_line() {
            return self.row_right(line, content_end);
        }
        self.cursor_at(content_end).x()
    }

    /// Right edge of the text on the display row starting at `row`, which
    /// wraps at `next` (the next row's start, on the same line): measured
    /// on the row's last shown character, as GTK's own row end can land
    /// past `next` when hidden text (a link's `[`) starts the next row.
    fn row_right(&self, row: &gtk::TextIter, next: &gtk::TextIter) -> i32 {
        let hidden = |i: &gtk::TextIter| i.tags().iter().any(|t| t.is_invisible());
        let mut last = *next;
        loop {
            if last <= *row || !last.backward_char() {
                return self.cursor_at(row).x();
            }
            if !hidden(&last) {
                break;
            }
        }
        // a row that wraps at a space ends before it
        if last.char().is_whitespace() {
            return self.cursor_at(&last).x();
        }
        let mut after = last;
        after.forward_char();
        if after < *next {
            // hidden text follows on this row: it starts where `last` ends
            self.cursor_at(&after).x()
        } else {
            // wrapped inside a word: the next position is on the next row
            self.cursor_at(&last).x() + self.iter_location(&last).width()
        }
    }

    /// The shown characters of the display row starting at `row`, each
    /// with its left and right edge (see `Row`); only those in `[from,
    /// to)` (offsets) when given.
    fn row_spans(
        &self,
        row: &gtk::TextIter,
        within: Option<(i32, i32)>,
    ) -> Vec<(gtk::TextIter, i32, i32)> {
        let row = Row::new(self, row);
        let shown = &row.shown;
        let buffer = self.buffer();
        let inside = |o: i32| within.is_none_or(|(from, to)| (from..to).contains(&o));
        let Some(first) = shown.iter().position(|(o, _)| inside(*o)) else {
            return Vec::new();
        };
        let last = shown.iter().rposition(|(o, _)| inside(*o)).unwrap_or(first);
        // characters of one direction side by side are drawn side by side:
        // then the first and the last give the edges of all
        let one_way = within.is_some()
            && shown[first..=last]
                .iter()
                .all(|(_, rtl)| *rtl == shown[first].1);
        (first..=last)
            .filter(|&i| !one_way || i == first || i == last)
            .map(|i| {
                let (left, right) = row.span(i);
                (buffer.iter_at_offset(shown[i].0), left, right)
            })
            .collect()
    }

    /// The shown character under `x` on the display row starting at `row`.
    fn row_char_at(&self, row: &gtk::TextIter, x: i32) -> Option<gtk::TextIter> {
        let row = Row::new(self, row);
        let shown = &row.shown;
        let hit = |i: usize| {
            let (left, right) = row.span(i);
            (left..right).contains(&x)
        };
        let one_way = shown
            .iter()
            .all(|(_, rtl)| Some(rtl) == shown.first().map(|c| &c.1));
        let found = if one_way && shown.len() > 8 {
            // in order on screen: halve the row (each look costs, on a
            // long line with much hidden text)
            let rtl = shown[0].1;
            let mut lo = 0;
            let mut hi = shown.len();
            while lo < hi {
                let mid = (lo + hi) / 2;
                let (left, right) = row.span(mid);
                let before_x = if rtl { left > x } else { right <= x };
                if before_x {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            [lo.saturating_sub(1), lo, lo + 1]
                .into_iter()
                .filter(|&i| i < shown.len())
                .find(|&i| hit(i))
        } else {
            (0..shown.len()).find(|&i| hit(i))
        };
        found.map(|i| self.buffer().iter_at_offset(shown[i].0))
    }

    /// Which of the shown characters of `[start, end)` (one line) read
    /// right to left, by their offsets, and whether the line does: as
    /// Unicode's bidi rules resolve them, roughly (Pango's own levels are
    /// not to be had from GTK). The line takes the direction of its first
    /// letter; spaces and marks that of the letters around them when those
    /// agree, else the line's.
    ///
    /// `None` for a line all left to right (see `line_chars`).
    fn line_directions(&self, start: &gtk::TextIter, end: &gtk::TextIter) -> Option<Directions> {
        let chars = self.line_chars(start, end);
        chars.any_rtl.then_some(chars)
    }

    /// The shown characters of a line (`[start, end)`), with their
    /// directions (see `line_directions`). Kept while drawing (asked for
    /// each find match or link on a line) and, between frames, for the last
    /// line asked about, as long as its text and what of it is shown stay
    /// the same.
    fn line_chars(&self, start: &gtk::TextIter, end: &gtk::TextIter) -> Directions {
        let drawn = |dirs: &Directions| {
            if let Some(lines) = self.imp().drawing.borrow_mut().as_mut() {
                lines.push((start.offset(), dirs.clone()));
            }
        };
        if let Some(lines) = &*self.imp().drawing.borrow()
            && let Some((_, dirs)) = lines.iter().find(|(at, _)| *at == start.offset())
        {
            return dirs.clone();
        }
        let shown = start.visible_text(end);
        let text = start.text(end);
        let cache = &self.imp().directions;
        if let Some((at, t, v, dirs)) = &*cache.borrow()
            && *at == start.offset()
            && t.as_str() == text.as_str()
            && v.as_str() == shown.as_str()
        {
            drawn(dirs);
            return dirs.clone();
        }
        let any_rtl = shown.chars().any(is_rtl);
        // the hidden runs (only `conceal` hides), from its toggles
        let mut hidden = Vec::new();
        if let Some(conceal) = self.buffer().tag_table().lookup("conceal") {
            let mut iter = *start;
            if !iter.has_tag(&conceal) {
                iter.forward_to_tag_toggle(Some(&conceal));
            }
            while iter < *end && iter.has_tag(&conceal) {
                let from = iter.offset();
                iter.forward_to_tag_toggle(Some(&conceal));
                hidden.push((from, iter.offset()));
                if !iter.forward_to_tag_toggle(Some(&conceal)) {
                    break;
                }
            }
        }
        let mut runs = hidden.iter().peekable();
        // per character: its offset, its own direction (`None` for spaces
        // and marks), and how it counts for those next to it (numbers lie
        // left to right, but count as right to left there, unless European
        // digits follow left-to-right text)
        // the line's direction: its first letter's (numbers aside)
        let base = any_rtl
            && shown
                .chars()
                .find_map(|c| {
                    if is_rtl(c) {
                        Some(true)
                    } else if c.is_alphabetic() {
                        Some(false)
                    } else {
                        None
                    }
                })
                .unwrap_or(false);
        let mut chars: Vec<(i32, Option<bool>, Option<bool>)> = Vec::new();
        let mut last_strong = base;
        for (i, c) in text.chars().enumerate() {
            let offset = start.offset() + i as i32;
            while runs.next_if(|(_, to)| *to <= offset).is_some() {}
            if runs.peek().is_some_and(|(from, _)| *from <= offset) {
                continue;
            }
            let arabic_digit = matches!(c as u32, 0x0660..=0x0669 | 0x06F0..=0x06F9);
            let (own, around) = if !any_rtl {
                (Some(false), Some(false))
            } else if is_rtl(c) {
                last_strong = true;
                (Some(true), Some(true))
            } else if arabic_digit {
                (Some(false), Some(true))
            } else if c.is_numeric() {
                (Some(false), Some(last_strong))
            } else if c.is_alphanumeric() {
                last_strong = false;
                (Some(false), Some(false))
            } else {
                (None, None)
            };
            chars.push((offset, own, around));
        }
        // what comes after each character, from the end
        let mut after = vec![base; chars.len()];
        let mut next = base;
        for (i, (_, _, around)) in chars.iter().enumerate().rev() {
            after[i] = next;
            if let Some(d) = around {
                next = *d;
            }
        }
        let mut before = base;
        let mut out = Vec::with_capacity(chars.len());
        for (i, (offset, own, around)) in chars.iter().enumerate() {
            let rtl = match own {
                Some(d) => *d,
                None if before == after[i] => before,
                None => base,
            };
            if let Some(d) = around {
                before = *d;
            }
            out.push((*offset, rtl));
        }
        let dirs: Directions = Rc::new(LineChars {
            any_rtl,
            base,
            chars: out,
        });
        cache.replace(Some((
            start.offset(),
            text.into(),
            shown.into(),
            dirs.clone(),
        )));
        drawn(&dirs);
        dirs
    }

    /// The cursor box at `from`, as tall as the text of `[from, to)`: the
    /// height comes from its first character that is shown at full size,
    /// not a code span's tiny hidden backtick (`pad`) or hidden syntax.
    fn glyphs_from(&self, from: &gtk::TextIter, to: &gtk::TextIter) -> gdk::Rectangle {
        let at = self.cursor_at(from);
        let pad = self.buffer().tag_table().lookup("pad");
        let mut iter = *from;
        while iter < *to && !iter.ends_line() {
            let small = pad.as_ref().is_some_and(|p| iter.has_tag(p));
            let hidden = iter.tags().iter().any(|t| t.is_invisible());
            if !small && !hidden {
                let glyphs = self.cursor_at(&iter);
                if glyphs.y() != at.y() && iter > *from && self.starts_display_line(&iter) {
                    break; // on the next row already
                }
                return gdk::Rectangle::new(at.x(), glyphs.y(), at.width(), glyphs.height());
            }
            iter.forward_char();
        }
        at
    }

    /// Where the cursor would be at `iter`. Unlike `iter_location`, right
    /// next to hidden text too (see `range_rects`).
    fn cursor_at(&self, iter: &gtk::TextIter) -> gdk::Rectangle {
        self.cursor_locations(Some(iter)).0
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
            let loc = self.cursor_at(&line);
            if loc.y() > bottom {
                break;
            }
            let a = if start > line { start } else { line };
            let b = if end < next { end } else { next };
            if a < b {
                let x1 = self.cursor_at(&a).x();
                let mut x2 = if b < content_end {
                    self.cursor_at(&b).x()
                } else {
                    self.text_right(&line, &content_end)
                };
                // the line break itself is selected
                if end > content_end && content_end < next {
                    x2 += newline;
                }
                if x2 > x1 {
                    let glyphs = self.glyphs_from(&a, &b);
                    let (mut y, mut height) = if single {
                        self.glyph_box(&glyphs)
                    } else {
                        self.line_box(&glyphs)
                    };
                    // inside a multi-line selection, also cover the spacing
                    // between paragraphs (headings, list starts) so the
                    // block has no stripes
                    if !single {
                        let (py, ph) = self.true_yrange(&line);
                        let (py, pbottom) = (py as f32, (py + ph) as f32);
                        if line.starts_line() && start < line && py < y {
                            height += y - py;
                            y = py;
                        }
                        let ends_paragraph = last_line || next.line() > line.line();
                        if ends_paragraph && end > next && pbottom > y + height {
                            height = pbottom - y;
                        }
                    }
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

    /// The room moved from below the line starting at `start` to the top
    /// of the next one (see `Editor::fix_margins`): what it would have
    /// below without `no-below`.
    fn carried(&self, start: &gtk::TextIter) -> i32 {
        let table = self.buffer().tag_table();
        match table.lookup("no-below") {
            Some(no_below) if start.has_tag(&no_below) => room_below(start),
            _ => 0,
        }
    }

    /// The room moved to the top of line `n`: what the shown line before
    /// it carries (lines hidden whole, like a setext `===`, take no room
    /// and pass it on). For `n` past the last line, what goes below it all.
    fn carried_into(&self, n: usize) -> i32 {
        let buffer = self.buffer();
        for m in (0..n).rev() {
            let Some(start) = buffer.iter_at_line(m as i32) else {
                continue;
            };
            if !self.hidden_whole(&start) {
                return self.carried(&start);
            }
        }
        0
    }

    /// Whether the line starting at `start` is hidden with its line break,
    /// so GTK gives it no height (nor any of its room).
    fn hidden_whole(&self, start: &gtk::TextIter) -> bool {
        let Some(conceal) = self.buffer().tag_table().lookup("conceal") else {
            return false;
        };
        if !start.has_tag(&conceal) {
            return false;
        }
        let mut next = *start;
        if !next.forward_line() {
            return false;
        }
        let mut off = *start;
        off.forward_to_tag_toggle(Some(&conceal));
        off >= next
    }

    /// `line_yrange` of the line at `iter` as it would be without the
    /// room moved between lines: its own room above and below, so boxes
    /// and pictures sit where they always have.
    fn true_yrange(&self, iter: &gtk::TextIter) -> (i32, i32) {
        let mut start = *iter;
        start.set_line_offset(0);
        let (y, h) = self.line_yrange(&start);
        let from_above = self.carried_into(start.line() as usize);
        (y + from_above, h - from_above + self.carried(&start))
    }

    /// The first line to draw for what is on screen: the one at the top
    /// and the shown one before it, whose room may have moved down onto
    /// the screen (see `Editor::fix_margins`).
    fn first_drawn_line(&self) -> gtk::TextIter {
        let (mut iter, _) = self.line_at_y(self.visible_rect().y());
        iter.set_line_offset(0);
        let buffer = self.buffer();
        let mut n = iter.line();
        while n > 0 {
            n -= 1;
            let Some(prev) = buffer.iter_at_line(n) else {
                break;
            };
            iter = prev;
            if !self.hidden_whole(&iter) {
                break;
            }
        }
        iter
    }

    /// Lines `[first, last]` that are on screen (or partly).
    fn visible_lines(&self) -> (usize, usize) {
        let visible = self.visible_rect();
        let top = self.first_drawn_line();
        let (bottom, _) = self.line_at_y(visible.y() + visible.height());
        (top.line() as usize, bottom.line() as usize)
    }

    /// Of `ranges` (in order, as `[start, end)` offsets), those whose text
    /// is on screen: the lines on screen can be one long paragraph, with
    /// hundreds of them off it. Found by halving, as each look at a place
    /// costs on a long line with much hidden text.
    fn on_screen(&self, ranges: &[(i32, i32)]) -> std::ops::Range<usize> {
        if ranges.len() <= 16 {
            return 0..ranges.len();
        }
        let buffer = self.buffer();
        let visible = self.visible_rect();
        // boxes and rings reach a little past their text
        let slack = 40;
        let (top, bottom) = (visible.y() - slack, visible.y() + visible.height() + slack);
        let rect = |offset: i32| self.cursor_at(&buffer.iter_at_offset(offset));
        let first = ranges.partition_point(|&(_, end)| {
            let at = rect(end);
            at.y() + at.height() < top
        });
        let last = first + ranges[first..].partition_point(|&(start, _)| rect(start).y() <= bottom);
        first..last
    }

    /// The ranges of `tag` whose text is on screen, for drawing on it.
    fn tag_ranges_shown(&self, name: &str) -> Vec<(gtk::TextIter, gtk::TextIter)> {
        let ranges = self.tag_ranges(name);
        let offsets: Vec<(i32, i32)> = ranges
            .iter()
            .map(|(a, b)| (a.offset(), b.offset()))
            .collect();
        ranges[self.on_screen(&offsets)].to_vec()
    }

    /// The ranges of `tag` on screen.
    fn tag_ranges(&self, name: &str) -> Vec<(gtk::TextIter, gtk::TextIter)> {
        let mut out = Vec::new();
        let Some(tag) = self.buffer().tag_table().lookup(name) else {
            return out;
        };
        let visible = self.visible_rect();
        let mut iter = self.first_drawn_line();
        let (mut end, _) = self.line_at_y(visible.y() + visible.height());
        end.forward_line();
        if !iter.has_tag(&tag) && !iter.forward_to_tag_toggle(Some(&tag)) {
            return out;
        }
        while iter < end {
            let start = iter;
            iter.forward_to_tag_toggle(Some(&tag));
            out.push((start, iter));
            if !iter.forward_to_tag_toggle(Some(&tag)) {
                break;
            }
        }
        out
    }

    /// Where `[a, b)` is on screen, one (x1, x2, glyphs) per display line it
    /// is on, every row of a wrapped one included. Measured with cursor
    /// positions: GTK places characters next to hidden text wrongly in
    /// `iter_location`, but its cursors right.
    fn range_rects(&self, a: &gtk::TextIter, b: &gtk::TextIter) -> Vec<(f32, f32, gdk::Rectangle)> {
        let mut out = Vec::new();
        if a >= b {
            return out;
        }
        let cursor = |iter: &gtk::TextIter| self.cursor_locations(Some(iter)).0;
        // in a line with right-to-left text, `[from, to)` of a row is not
        // from one cursor to the other: from its leftmost character's left
        // edge to its rightmost's right (all between, at a change of
        // direction)
        let bidi = |row: &gtk::TextIter, from: &gtk::TextIter, to: &gtk::TextIter| {
            let mut start = *row;
            if !self.starts_display_line(&start) {
                self.backward_display_line_start(&mut start);
            }
            let mut line = start;
            line.set_line_offset(0);
            let mut line_end = line;
            if !line_end.ends_line() {
                line_end.forward_to_line_end();
            }
            self.line_directions(&line, &line_end)?;
            self.row_spans(&start, Some((from.offset(), to.offset())))
                .into_iter()
                .fold(None, |acc: Option<(i32, i32)>, (_, l, r)| {
                    Some(acc.map_or((l, r), |(a, b)| (a.min(l), b.max(r))))
                })
                .map(|(l, r)| (l as f32, r as f32))
        };
        let mut row = *a;
        loop {
            let first = self.glyphs_from(&row, b);
            let mut next = row;
            if !self.forward_display_line(&mut next) || next <= row || next > *b {
                let (x1, x2) =
                    bidi(&row, &row, b).unwrap_or((first.x() as f32, cursor(b).x() as f32));
                out.push((x1, x2, first));
                return out;
            }
            // this row ends where it wraps, before the next one starts
            let (x1, x2) = bidi(&row, &row, &next)
                .unwrap_or((first.x() as f32, self.row_right(&row, &next) as f32));
            out.push((x1, x2, first));
            if next == *b {
                // ends right where the row wraps: nothing on the next one
                return out;
            }
            row = next;
        }
    }

    /// The character under a point (buffer coordinates) on a line that has
    /// one of `tags`; `None` elsewhere. Measured with cursor positions:
    /// `iter_at_location` misplaces characters next to hidden text (see
    /// `range_rects`) and finds none at all in parts of a table row.
    fn char_at(&self, x: i32, y: i32, tags: &[&gtk::TextTag]) -> Option<gtk::TextIter> {
        let (line, _) = self.line_at_y(y);
        let tagged = tags.is_empty()
            || tags.iter().any(|tag| {
                let mut probe = line;
                probe.has_tag(*tag)
                    || (probe.forward_to_tag_toggle(Some(*tag)) && probe.line() == line.line())
            });
        if !tagged {
            return None;
        }
        let mut line_end = line;
        if !line_end.ends_line() {
            line_end.forward_to_line_end();
        }
        let scan_row = |row: gtk::TextIter| {
            let start = self.cursor_locations(Some(&row)).0;
            let (top, height) = self.line_box(&start);
            if !(top..top + height).contains(&(y as f32)) {
                return None;
            }
            self.row_char_at(&row, x)
        };
        // the row at the point: each row of the line in turn (not with
        // `iter_at_location`, which aborts on a line whose hidden text
        // changed and is not laid out again yet)
        let mut row = line;
        loop {
            let top = self.line_box(&self.cursor_locations(Some(&row)).0).0;
            if top > y as f32 {
                return None; // past the point
            }
            if let Some(found) = scan_row(row) {
                return Some(found);
            }
            let mut next = row;
            if !self.forward_display_line(&mut next) || next <= row || next > line_end {
                return None;
            }
            row = next;
        }
    }

    /// The end of the line, for a point right of it on its last row when
    /// hidden text is on that row (GTK places the cursor wrongly there).
    fn past_line_end(&self, x: i32, y: i32) -> Option<gtk::TextIter> {
        let (line, _) = self.line_at_y(y);
        let mut end = line;
        if !end.ends_line() {
            end.forward_to_line_end();
        }
        let mut row = end;
        if !self.starts_display_line(&row) {
            self.backward_display_line_start(&mut row);
        }
        let (top, height) = self.line_box(&self.cursor_at(&row));
        if !(top..top + height).contains(&(y as f32)) {
            return None;
        }
        let hidden = |i: &gtk::TextIter| i.tags().iter().any(|t| t.is_invisible());
        let mut any_hidden = false;
        let mut last = None;
        let mut iter = row;
        while iter < end {
            if hidden(&iter) {
                any_hidden = true;
            } else {
                last = Some(iter);
            }
            iter.forward_char();
        }
        let last = last?;
        let mut after = last;
        after.forward_char();
        // right of the last character shown (in left-to-right text: in
        // right-to-left text GTK's own placing is right)
        let after_x = self.cursor_at(&after).x();
        let ltr = self.cursor_at(&last).x() <= after_x;
        (any_hidden && ltr && x >= after_x).then_some(end)
    }

    /// GitHub-style boxes behind code blocks and tables.
    fn draw_blocks(&self, snapshot: &gtk::Snapshot) {
        let blocks = self.imp().blocks.borrow();
        if blocks.iter().all(|b| *b == Block::None) {
            return;
        }
        let zoom = self.zoom();
        let ink = self.color();
        let visible = self.visible_rect();
        let (top, bottom) = self.visible_lines();
        let gap = (BLOCK_GAP as f32 * zoom).round();
        let width = self.width() as f32;
        let mut n = top.min(blocks.len() - 1);
        while n > 0 && continues(blocks[n - 1], blocks[n]) {
            n -= 1;
        }
        while n <= bottom && n < blocks.len() {
            if !blocks[n].is_boxed() {
                n += 1;
                continue;
            }
            let first = n;
            let mut last = n;
            // a table is measured whole (its box is as wide as its widest
            // row, on screen or not); a long code block only on screen
            let whole = blocks[first].is_table();
            while last + 1 < blocks.len()
                && continues(blocks[last], blocks[last + 1])
                && (last <= bottom || whole)
            {
                last += 1;
            }
            n = last + 1;
            let yrange = |line: usize| {
                let iter = self.buffer().iter_at_line(line as i32);
                iter.map(|i| self.true_yrange(&i)).unwrap_or_default()
            };
            let (y, _) = yrange(first);
            let (ly, lh) = yrange(last);
            // a block that goes on above or below the screen gets no edge there
            let top_y = if first < top {
                visible.y() as f32 - 20.0
            } else {
                y as f32 + gap
            };
            let open_end = last + 1 < blocks.len() && continues(blocks[last], blocks[last + 1]);
            let bottom_y = if open_end {
                (visible.y() + visible.height()) as f32 + 20.0
            } else {
                (ly + lh) as f32 - gap
            };
            let radius = 8.0 * zoom;
            if blocks[first].is_code() || blocks[first].is_math() {
                let rect = graphene::Rect::new(0.0, top_y, width, bottom_y - top_y);
                let outline = gsk::RoundedRect::from_rect(rect, radius);
                snapshot.push_rounded_clip(&outline);
                snapshot.append_color(&tint(&ink, 0.06), &rect);
                snapshot.pop();
                snapshot.append_border(&outline, &[1.0; 4], &[tint(&ink, 0.1); 4]);
            } else {
                self.draw_table(snapshot, first, last, top_y, bottom_y);
            }
        }
    }

    /// A table, GitHub style: a box from its first pipes to its last, the
    /// head shaded, a line between rows. A lined-up table not being edited
    /// gets its column lines drawn (its pipes are hidden then); otherwise
    /// the faint pipes are the column lines.
    fn draw_table(
        &self,
        snapshot: &gtk::Snapshot,
        first: usize,
        last: usize,
        top: f32,
        bottom: f32,
    ) {
        let zoom = self.zoom();
        let ink = self.color();
        let buffer = self.buffer();
        let blocks = self.imp().blocks.borrow();
        let cursor = |iter: &gtk::TextIter| self.cursor_locations(Some(iter)).0.x() as f32;
        // the middle of the character at `iter`
        let middle = |iter: &gtk::TextIter| {
            let mut next = *iter;
            next.forward_char();
            (cursor(iter) + cursor(&next)) / 2.0
        };
        let ghost = buffer.tag_table().lookup("ghost");
        let mut left = f32::MAX;
        let mut right = 0.0f32;
        let mut columns = Vec::new();
        // a row wider than the window wraps: the box takes all the width
        let mut wraps = false;
        for n in first..=last {
            let Some(start) = buffer.iter_at_line(n as i32) else {
                continue;
            };
            let mut end = start;
            if !end.ends_line() {
                end.forward_to_line_end();
            }
            if start >= end {
                continue;
            }
            wraps |= self.cursor_locations(Some(&start)).0.y()
                != self.cursor_locations(Some(&end)).0.y();
            let mut last_char = end;
            last_char.backward_char();
            left = left.min(if start.char() == '|' {
                middle(&start)
            } else {
                cursor(&start) - 6.0 * zoom
            });
            right = right.max(if last_char.char() == '|' {
                middle(&last_char)
            } else {
                cursor(&end) + 6.0 * zoom
            });
            // a head with hidden pipes: its columns get lines
            if blocks.get(n) == Some(&Block::TableHead)
                && ghost.as_ref().is_some_and(|g| start.has_tag(g))
            {
                let text = buffer.text(&start, &end, true);
                for pipe in markdown::table_pipes(&text) {
                    columns.push(middle(&buffer.iter_at_offset(start.offset() + pipe as i32)));
                }
            }
        }
        if wraps {
            // its column lines would cross the wrapped text
            right = self.width() as f32 - 1.0;
            columns.clear();
        }
        if left >= right {
            return;
        }
        let rect = graphene::Rect::new(left, top, right - left, bottom - top);
        let outline = gsk::RoundedRect::from_rect(rect, 6.0 * zoom);
        let line_color = tint(&ink, 0.14);
        snapshot.push_rounded_clip(&outline);
        snapshot.append_color(&tint(&ink, 0.02), &rect);
        let mut body_row = 0;
        for n in first..=last {
            let Some(iter) = buffer.iter_at_line(n as i32) else {
                continue;
            };
            let (y, h) = self.true_yrange(&iter);
            let (y, h) = (y as f32, h as f32);
            match blocks.get(n) {
                Some(Block::TableHead) => {
                    let head = graphene::Rect::new(left, top, right - left, y + h - top);
                    snapshot.append_color(&tint(&ink, 0.06), &head);
                }
                Some(Block::TableRule) => {
                    // the head's shading runs to the middle of the rule
                    let middle = (y + h / 2.0).round();
                    let band = graphene::Rect::new(left, y, right - left, middle - y);
                    snapshot.append_color(&tint(&ink, 0.06), &band);
                    let line = graphene::Rect::new(left, middle, right - left, 1.0);
                    snapshot.append_color(&line_color, &line);
                }
                Some(Block::TableRow | Block::TableEnd) => {
                    body_row += 1;
                    if body_row % 2 == 0 {
                        let stripe = graphene::Rect::new(left, y, right - left, h);
                        snapshot.append_color(&tint(&ink, 0.025), &stripe);
                    }
                    if body_row > 1 {
                        let line = graphene::Rect::new(left, y, right - left, 1.0);
                        snapshot.append_color(&tint(&ink, 0.1), &line);
                    }
                }
                _ => {}
            }
        }
        if columns.len() > 2 {
            for x in &columns[1..columns.len() - 1] {
                let line = graphene::Rect::new(x.round() - 0.5, top, 1.0, bottom - top);
                snapshot.append_color(&line_color, &line);
            }
        }
        snapshot.pop();
        snapshot.append_border(&outline, &[1.0; 4], &[line_color; 4]);
    }

    /// Pills behind code spans, key caps behind `<kbd>` keys.
    fn draw_boxes(&self, snapshot: &gtk::Snapshot) {
        let zoom = self.zoom();
        let ink = self.color();
        for (name, key) in [("code-box", false), ("kbd-box", true)] {
            for (a, b) in self.tag_ranges_shown(name) {
                // measured on the text itself: each character's box is as
                // tall as its own font, and the syntax around it is hidden
                // or tiny; the padding is where the syntax sits
                for (x1, x2, glyphs) in self.range_rects(&a, &b) {
                    let (side, edge) = (4.0 * zoom, 2.0 * zoom);
                    let rect = graphene::Rect::new(
                        x1 - side,
                        glyphs.y() as f32 - edge,
                        x2 - x1 + 2.0 * side,
                        glyphs.height() as f32 + 2.0 * edge,
                    );
                    let outline = gsk::RoundedRect::from_rect(rect, 5.0 * zoom);
                    snapshot.push_rounded_clip(&outline);
                    snapshot.append_color(&tint(&ink, if key { 0.04 } else { 0.08 }), &rect);
                    snapshot.pop();
                    let (widths, color) = if key {
                        ([1.0, 1.0, 2.0, 1.0], tint(&ink, 0.25))
                    } else {
                        ([1.0; 4], tint(&ink, 0.1))
                    };
                    snapshot.append_border(&outline, &widths, &[color; 4]);
                }
            }
        }
    }

    /// Links: underlined in the theme's accent.
    fn draw_links(&self, snapshot: &gtk::Snapshot) {
        let Some(accent) = *self.imp().accent.borrow() else {
            return;
        };
        let zoom = self.zoom();
        for (a, b) in self.tag_ranges_shown("link") {
            for (x1, x2, glyphs) in self.range_rects(&a, &b) {
                let (y, height) = self.glyph_box(&glyphs);
                let thick = (2.0 * zoom).round().max(1.0);
                let rect = graphene::Rect::new(x1, y + height - thick - zoom, x2 - x1, thick);
                snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(rect, thick / 2.0));
                snapshot.append_color(&accent, &rect);
                snapshot.pop();
            }
        }
    }

    /// List markers: a dot, the number (as Markdown counts it) or a task's
    /// box, in the margin before the text.
    fn draw_markers(&self, snapshot: &gtk::Snapshot) {
        let kinds = self.imp().kinds.borrow();
        let shapes = self.imp().shapes.borrow();
        if !kinds.iter().any(|k| k.is_list()) {
            return;
        }
        let zoom = self.zoom();
        let color = self.color();
        let visible = self.visible_rect();
        let mut iter = self.first_drawn_line();
        loop {
            let (y, _) = self.true_yrange(&iter);
            if y > visible.y() + visible.height() {
                break;
            }
            let n = iter.line() as usize;
            if let Some(&kind) = kinds.get(n).filter(|k| k.is_list()) {
                let shape = shapes.get(n).copied().unwrap_or_default();
                let level = (shape.level as usize).min(LEVELS - 1);
                let rect = self.cursor_at(&iter);
                let cy = rect.y() as f32 + rect.height() as f32 / 2.0 - 0.5 * zoom;
                let left = 20.0 * level as f32 * zoom;
                match (kind.task(), kind) {
                    (Some(task), _) => self.draw_task_box(snapshot, left, cy, task),
                    (None, LineKind::Numbered { delim, .. }) => {
                        let label = format!("{}{}", shape.number, delim as char);
                        let layout = self.create_pango_layout(Some(&label));
                        let (w, h) = layout.pixel_size();
                        let right = number_margin(level, shape.digits, zoom as f64) as f32;
                        let x = right - 6.0 * zoom - w as f32;
                        let y = rect.y() as f32 + (rect.height() - h) as f32 / 2.0;
                        snapshot.save();
                        snapshot.translate(&graphene::Point::new(x, y));
                        snapshot.append_layout(&layout, &tint(&color, 0.85));
                        snapshot.restore();
                    }
                    _ => {
                        let cx = left + 5.5 * zoom;
                        let builder = gsk::PathBuilder::new();
                        builder.add_circle(&graphene::Point::new(cx, cy), 2.5 * zoom);
                        snapshot.append_fill(&builder.to_path(), gsk::FillRule::Winding, &color);
                    }
                }
            }
            let line = iter.line();
            iter.forward_line();
            if iter.line() == line {
                break;
            }
        }
    }

    /// Where a task's box is drawn on a line: its left edge, and size.
    fn task_box(&self, level: usize) -> (f32, f32) {
        let zoom = self.zoom();
        (20.0 * level as f32 * zoom + 1.0 * zoom, TASK_BOX * zoom)
    }

    /// An open box, or a ticked one filled with the theme's accent.
    fn draw_task_box(&self, snapshot: &gtk::Snapshot, left: f32, cy: f32, task: Task) {
        let zoom = self.zoom();
        let ink = self.color();
        let size = TASK_BOX * zoom;
        let rect = graphene::Rect::new(left + 1.0 * zoom, cy - size / 2.0, size, size);
        let outline = gsk::RoundedRect::from_rect(rect, 4.0 * zoom);
        match task {
            Task::Open => {
                snapshot.append_border(&outline, &[1.5 * zoom; 4], &[tint(&ink, 0.55); 4]);
            }
            Task::Done(_) => {
                let accent = (*self.imp().accent.borrow()).unwrap_or(ink);
                snapshot.push_rounded_clip(&outline);
                snapshot.append_color(&accent, &rect);
                snapshot.pop();
                let builder = gsk::PathBuilder::new();
                let (x, y) = (rect.x(), rect.y());
                builder.move_to(x + size * 0.26, y + size * 0.52);
                builder.line_to(x + size * 0.43, y + size * 0.69);
                builder.line_to(x + size * 0.75, y + size * 0.34);
                let stroke = gsk::Stroke::new(1.8 * zoom);
                stroke.set_line_cap(gsk::LineCap::Round);
                stroke.set_line_join(gsk::LineJoin::Round);
                snapshot.append_stroke(&builder.to_path(), &stroke, &ink);
            }
        }
    }

    /// Quotes: a bar per level down their left side (in an alert's colour,
    /// with its icon by the title); rules across.
    fn draw_quotes(&self, snapshot: &gtk::Snapshot) {
        let kinds = self.imp().kinds.borrow();
        let blocks = self.imp().blocks.borrow();
        let quoted = |n: usize| match (kinds.get(n), blocks.get(n)) {
            (Some(LineKind::Quote { depth, .. }), _) | (_, Some(Block::Quote { depth })) => *depth,
            _ => 0,
        };
        let has_quotes = kinds.iter().any(|k| matches!(k, LineKind::Quote { .. }));
        let has_blocks = blocks
            .iter()
            .any(|b| matches!(b, Block::Quote { .. } | Block::Rule));
        if !has_quotes && !has_blocks {
            return;
        }
        let zoom = self.zoom();
        let ink = self.color();
        let width = self.width() as f32;
        let visible = self.visible_rect();
        let buffer = self.buffer();
        let cursor = buffer.iter_at_mark(&buffer.get_insert()).line() as usize;
        let mut iter = self.first_drawn_line();
        loop {
            let (y, h) = self.true_yrange(&iter);
            if y > visible.y() + visible.height() {
                break;
            }
            let n = iter.line() as usize;
            let (y, h) = (y as f32, h as f32);
            let depth = quoted(n);
            if depth > 0 {
                let shape = self.shape(n);
                let gap = (QUOTE_GAP as f32 * zoom).round();
                let top = if shape.quote_first { y + gap } else { y };
                let bottom = if shape.quote_last { y + h - gap } else { y + h };
                let color = shape.alert.map_or(tint(&ink, 0.22), alert_color);
                let round = graphene::Size::new(1.5 * zoom, 1.5 * zoom);
                let square = graphene::Size::zero();
                let (t, b) = (
                    if shape.quote_first { round } else { square },
                    if shape.quote_last { round } else { square },
                );
                for level in 0..depth {
                    let x = (QUOTE_INDENT as f32 * level as f32 + 1.0) * zoom;
                    let rect = graphene::Rect::new(x, top, 3.0 * zoom, bottom - top);
                    snapshot.push_rounded_clip(&gsk::RoundedRect::new(rect, t, t, b, b));
                    snapshot.append_color(&color, &rect);
                    snapshot.pop();
                }
                // an alert's icon, before its title
                if let Some(alert) = shape.alert
                    && shape.quote_first
                    && matches!(kinds.get(n), Some(LineKind::Quote { .. }))
                {
                    let rect = self.cursor_locations(Some(&iter)).0;
                    let size = 15.0 * zoom;
                    let x = (QUOTE_INDENT as f32 + 1.0) * zoom;
                    let cy = rect.y() as f32 + rect.height() as f32 / 2.0;
                    let stroke = gsk::Stroke::new(2.0);
                    stroke.set_line_cap(gsk::LineCap::Round);
                    stroke.set_line_join(gsk::LineJoin::Round);
                    snapshot.save();
                    snapshot.translate(&graphene::Point::new(x, cy - size / 2.0));
                    snapshot.scale(size / 24.0, size / 24.0);
                    for path in theme::icon_paths(alert_icon(alert)) {
                        snapshot.append_stroke(&path, &stroke, &alert_color(alert));
                    }
                    snapshot.restore();
                }
            }
            // a rule, unless the cursor is on it (then its dashes show)
            if blocks.get(n) == Some(&Block::Rule) && n != cursor {
                let thick = (2.0 * zoom).round().max(1.0);
                let rect =
                    graphene::Rect::new(0.0, (y + h / 2.0 - thick / 2.0).round(), width, thick);
                snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(rect, thick / 2.0));
                snapshot.append_color(&tint(&ink, 0.18), &rect);
                snapshot.pop();
            }
            let line = iter.line();
            iter.forward_line();
            if iter.line() == line {
                break;
            }
        }
    }

    /// Highlights (`==x==`, `<mark>`): the accent behind the text, like
    /// the selection but softer, so the two tell apart.
    fn draw_marks(&self, snapshot: &gtk::Snapshot) {
        let Some(accent) = *self.imp().accent.borrow() else {
            return;
        };
        let zoom = self.zoom();
        for (a, b) in self.tag_ranges_shown("mark") {
            for (x1, x2, glyphs) in self.range_rects(&a, &b) {
                let (y, height) = self.glyph_box(&glyphs);
                let pad = 2.0 * zoom;
                let left = (x1 - pad).max(0.0);
                let rect = graphene::Rect::new(left, y, x2 + pad - left, height);
                snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(rect, 4.0 * zoom));
                snapshot.append_color(&tint(&accent, 0.6), &rect);
                snapshot.pop();
            }
        }
    }

    /// Find in the note: every match on screen ringed in the text colour,
    /// the one gone to filled with the accent as well.
    fn draw_finds(&self, snapshot: &gtk::Snapshot) {
        let finds = self.imp().finds.borrow();
        if finds.is_empty() {
            return;
        }
        let (Some(accent), Some(ink)) = (*self.imp().accent.borrow(), *self.imp().ink.borrow())
        else {
            return;
        };
        let current = self.imp().find_current.get();
        let buffer = self.buffer();
        let visible = self.visible_rect();
        let (top, _) = self.line_at_y(visible.y());
        let (mut bottom, _) = self.line_at_y(visible.y() + visible.height());
        bottom.forward_to_line_end();
        let (top, bottom) = (top.offset(), bottom.offset());
        let first = finds.partition_point(|&(_, end)| end < top);
        let last = first + finds[first..].partition_point(|&(start, _)| start <= bottom);
        let shown = self.on_screen(&finds[first..last]);
        let zoom = self.zoom();
        let radius = 4.0 * zoom;
        let pad = 2.0 * zoom;
        for (i, &(a, b)) in finds
            .iter()
            .enumerate()
            .take(first + shown.end)
            .skip(first + shown.start)
        {
            let (a, b) = (buffer.iter_at_offset(a), buffer.iter_at_offset(b));
            let on = current == Some(i);
            for (x1, x2, glyphs) in self.range_rects(&a, &b) {
                if x2 <= x1 {
                    continue; // hidden syntax
                }
                let (y, height) = self.glyph_box(&glyphs);
                // the view ends at the text's left edge: stay inside it
                let left = (x1 - pad).max(1.0);
                let rect = graphene::Rect::new(left, y, x2 + pad - left, height);
                let rounded = gsk::RoundedRect::from_rect(rect, radius);
                if on {
                    snapshot.push_rounded_clip(&rounded);
                    snapshot.append_color(&accent, &rect);
                    snapshot.pop();
                }
                let width = if on { 2.0 } else { 1.5 } * zoom;
                let ring = tint(&ink, if on { 0.85 } else { 0.4 });
                snapshot.append_border(&rounded, &[width; 4], &[ring; 4]);
            }
        }
    }

    /// Pictures under the lines that show them, in the room left there.
    fn draw_pictures(&self, snapshot: &gtk::Snapshot) {
        let lines: Vec<usize> = {
            let ranges = self.tag_ranges("picture");
            let mut lines: Vec<usize> = ranges.iter().map(|(a, _)| a.line() as usize).collect();
            lines.dedup();
            lines
        };
        if lines.is_empty() {
            return;
        }
        let pictures = self.imp().pictures.borrow();
        let Some(pictures_of) = pictures.as_ref() else {
            return;
        };
        let zoom = self.zoom();
        let gap = (PICTURE_GAP as f32 * zoom).round();
        let ink = self.color();
        for n in lines {
            let shown = pictures_of(n);
            let Some(start) = self.buffer().iter_at_line(n as i32) else {
                continue;
            };
            let (y, h) = self.true_yrange(&start);
            let total: f32 = shown.iter().map(|(_, _, h)| h + gap).sum::<f32>() + gap;
            let x = self.cursor_locations(Some(&start)).0.x() as f32;
            let mut top = (y + h) as f32 - total + gap;
            for (texture, w, h) in shown {
                let rect = graphene::Rect::new(x, top, w, h);
                let outline = gsk::RoundedRect::from_rect(rect, 8.0 * zoom);
                snapshot.push_rounded_clip(&outline);
                snapshot.append_scaled_texture(&texture, gsk::ScalingFilter::Trilinear, &rect);
                snapshot.pop();
                snapshot.append_border(&outline, &[1.0; 4], &[tint(&ink, 0.1); 4]);
                top += h + gap;
            }
        }
    }

    /// `:shortcode:` emoji, in the room their first colon was given.
    fn draw_emoji(&self, snapshot: &gtk::Snapshot) {
        let buffer = self.buffer();
        for (a, _) in self.tag_ranges("emoji") {
            let mut close = a;
            close.forward_char();
            if !close.forward_find_char(|c| c == ':', None) {
                continue;
            }
            let mut name_start = a;
            name_start.forward_char();
            let name = buffer.text(&name_start, &close, true);
            let Some(emoji) = markdown::emoji(&name) else {
                continue;
            };
            // where the colon is, measured by the cursor (see `range_rects`)
            let rect = self.cursor_locations(Some(&a)).0;
            let layout = self.create_pango_layout(Some(emoji));
            let (_, logical) = layout.pixel_extents();
            let y = rect.y() as f32 + (rect.height() - logical.height()) as f32 / 2.0;
            snapshot.save();
            snapshot.translate(&graphene::Point::new(rect.x() as f32 + self.zoom(), y));
            snapshot.append_layout(&layout, &self.color());
            snapshot.restore();
        }
    }
}

use markdown::continues;

/// Whether `tag` is on any of `[start, end)`.
/// The room a line has below it (`start` is its start) from its tags, as
/// GTK takes it: from the strongest tag that sets it, `no-below` aside.
fn room_below(start: &gtk::TextIter) -> i32 {
    strongest(start, |t| {
        (t.is_pixels_below_lines_set() && t.name().as_deref() != Some("no-below"))
            .then(|| t.pixels_below_lines())
    })
}

/// The room a line has above it from its own tags (not a `lift`).
fn room_above(start: &gtk::TextIter) -> i32 {
    strongest(start, |t| {
        let lift = t.name().is_some_and(|n| n.starts_with("lift-"));
        (t.is_pixels_above_lines_set() && !lift).then(|| t.pixels_above_lines())
    })
}

/// The value of the strongest (highest priority) tag at `iter` that has
/// one; 0 if none does.
fn strongest(iter: &gtk::TextIter, value: impl Fn(&gtk::TextTag) -> Option<i32>) -> i32 {
    iter.tags()
        .iter()
        .filter_map(|t| value(t).map(|v| (t.priority(), v)))
        .max_by_key(|(priority, _)| *priority)
        .map_or(0, |(_, v)| v)
}

/// The shown characters of a line and their directions (see
/// `View::line_chars`).
struct LineChars {
    /// Whether any reads right to left.
    any_rtl: bool,
    /// Whether the line does.
    base: bool,
    /// Offsets, in order, and whether each reads right to left.
    chars: Vec<(i32, bool)>,
}

type Directions = Rc<LineChars>;

/// `line_chars` of the lines drawn so far in a frame, by their start.
type DrawnLines = Vec<(i32, Directions)>;

/// A display row, to measure its characters with cursor positions: GTK
/// gets those right, unlike `iter_location`, whose places and widths go
/// wrong next to hidden text. Pango puts the trailing edge of the
/// character before a place and the leading edge of the one after it at
/// its two cursors, the strong one being where text of the line's
/// direction goes: which is which tells right-to-left text apart. Each
/// cursor is looked up once, and only when needed: on a long line with
/// much hidden text each look costs.
struct Row<'a> {
    view: &'a View,
    /// The row's shown characters: offsets, and whether each reads right
    /// to left.
    shown: Vec<(i32, bool)>,
    base: bool,
    /// Where the next row starts, if the line goes on there.
    wraps_at: Option<i32>,
    cursors: RefCell<HashMap<i32, (i32, i32)>>,
}

impl<'a> Row<'a> {
    fn new(view: &'a View, row: &gtk::TextIter) -> Row<'a> {
        let mut line = *row;
        line.set_line_offset(0);
        let mut line_end = line;
        if !line_end.ends_line() {
            line_end.forward_to_line_end();
        }
        let chars = view.line_chars(&line, &line_end);
        // where GTK starts the next row: asked once, not at each character
        let mut next = *row;
        let wraps_at = (view.forward_display_line(&mut next) && next > *row && next < line_end)
            .then(|| next.offset());
        let end = wraps_at.unwrap_or(line_end.offset());
        let from = chars.chars.partition_point(|(o, _)| *o < row.offset());
        let to = chars.chars.partition_point(|(o, _)| *o < end);
        Row {
            view,
            shown: chars.chars[from..to].to_vec(),
            base: chars.base,
            wraps_at,
            cursors: RefCell::new(HashMap::new()),
        }
    }

    /// The strong and weak cursor x at `offset`.
    fn cursors_at(&self, offset: i32) -> (i32, i32) {
        *self.cursors.borrow_mut().entry(offset).or_insert_with(|| {
            let place = self.view.buffer().iter_at_offset(offset);
            let (strong, weak) = self.view.cursor_locations(Some(&place));
            (strong.x(), weak.x())
        })
    }

    /// The left and right edge of the `i`th shown character.
    fn span(&self, i: usize) -> (i32, i32) {
        let (offset, rtl) = self.shown[i];
        let base = self.base;
        // a row starts in the line's direction
        let before = match i {
            0 => base,
            _ => self.shown[i - 1].1,
        };
        let (strong, weak) = self.cursors_at(offset);
        let leading = if before == base { weak } else { strong };
        // the trailing edge, at the place after it
        let trailing = |place: i32| {
            let (strong, weak) = self.cursors_at(place);
            if rtl == base { strong } else { weak }
        };
        let trail = match self.shown.get(i + 1) {
            Some((next, _)) => trailing(*next),
            // the row's last: up to the place right after it, unless that
            // is where the next row starts
            None if self.wraps_at == Some(offset + 1) => {
                let place = self.view.buffer().iter_at_offset(offset);
                let width = self.view.iter_location(&place).width();
                if rtl {
                    leading - width
                } else {
                    leading + width
                }
            }
            None => trailing(offset + 1),
        };
        (leading.min(trail), leading.max(trail))
    }
}

/// A letter of a script written right to left (Hebrew, Arabic and the
/// like).
/// Not their digits: Pango lays those out left to right.
fn is_rtl(c: char) -> bool {
    let digit = matches!(c as u32, 0x0660..=0x0669 | 0x06F0..=0x06F9);
    !digit
        && matches!(c as u32,
            0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF)
}

fn has_tag_in(start: &gtk::TextIter, end: &gtk::TextIter, tag: &gtk::TextTag) -> bool {
    let mut iter = *start;
    start.has_tag(tag) || (iter.forward_to_tag_toggle(Some(tag)) && iter < *end)
}

fn tint(color: &gdk::RGBA, alpha: f32) -> gdk::RGBA {
    let mut color = *color;
    color.set_alpha(alpha);
    color
}

// ---------------------------------------------------------------------------
// Tags

struct Tags {
    /// On every line: CSS line-height has no effect on a GtkTextView.
    body: gtk::TextTag,
    bold: gtk::TextTag,
    italic: gtk::TextTag,
    underline: gtk::TextTag,
    strike: gtk::TextTag,
    raw: gtk::TextTag,
    escaped: gtk::TextTag,
    italic_us: gtk::TextTag,
    bold_us: gtk::TextTag,
    /// `#` to `######`.
    headings: [gtk::TextTag; 6],
    list_start: gtk::TextTag,
    levels: Vec<gtk::TextTag>,
    /// A ticked task: faded.
    done: gtk::TextTag,
    /// Quotes: indented a step per level, faded (not an alert's), with room
    /// above and below; an alert's title makes room for its icon.
    quote_levels: Vec<gtk::TextTag>,
    quote_text: gtk::TextTag,
    quote_first: gtk::TextTag,
    quote_last: gtk::TextTag,
    alert_title: gtk::TextTag,
    // code blocks and tables, whole lines
    mono_block: gtk::TextTag,
    code_line: gtk::TextTag,
    fence_open: gtk::TextTag,
    fence_close: gtk::TextTag,
    /// The first and last line of indented code: its box's padding.
    indent_first: gtk::TextTag,
    indent_last: gtk::TextTag,
    table_line: gtk::TextTag,
    table_first: gtk::TextTag,
    table_last: gtk::TextTag,
    table_head: gtk::TextTag,
    table_rule: gtk::TextTag,
    /// Display math, serif and centred; on one line it gets the box's
    /// padding too.
    math_block: gtk::TextTag,
    math_line: gtk::TextTag,
    /// The `===` under a heading: small, like a fence.
    setext_rule: gtk::TextTag,
    // inside a line: code spans, keys, links and their syntax
    mono: gtk::TextTag,
    mono_h1: gtk::TextTag,
    mono_h2: gtk::TextTag,
    kbd: gtk::TextTag,
    code_box: gtk::TextTag,
    kbd_box: gtk::TextTag,
    link: gtk::TextTag,
    /// Formatting written out in raw text (a table cell, HTML): drawn, but
    /// not the line's own formatting (`bold` and the rest are saved).
    deco_bold: gtk::TextTag,
    deco_italic: gtk::TextTag,
    deco_strike: gtk::TextTag,
    deco_underline: gtk::TextTag,
    /// `==marked==`: drawn behind (see `draw_marks`).
    mark: gtk::TextTag,
    sup: gtk::TextTag,
    sub: gtk::TextTag,
    /// `$math$`: serif, like a formula.
    math: gtk::TextTag,
    /// A footnote reference: raised; a footnote's own line is smaller.
    footnote: gtk::TextTag,
    footnote_line: gtk::TextTag,
    /// `<!-- comments -->`: faint italic.
    comment: gtk::TextTag,
    /// An alert's title (`[!NOTE]` reads NOTE), one per alert colour.
    alert_labels: [gtk::TextTag; 5],
    /// A line showing pictures: what to draw is found by this tag.
    picture: gtk::TextTag,
    /// Markdown syntax on show: faint.
    syntax: gtk::TextTag,
    /// Syntax kept as a sliver of empty space: the padding of a pill.
    pad: gtk::TextTag,
    /// Syntax hidden while the cursor is elsewhere.
    conceal: gtk::TextTag,
    /// Syntax that is not drawn but keeps its room (a lined-up table's
    /// pipes, whose lines are drawn instead).
    ghost: gtk::TextTag,
    /// The first colon of an emoji's `:code:`: unseen, as wide as the
    /// emoji drawn there.
    emoji: gtk::TextTag,
    /// On every line `decorate_line` put something on: lines without it
    /// have nothing to take off.
    decorated: gtk::TextTag,
    /// No room below a line that has hidden text: its room moves to the
    /// top of the next line (`lift`). See `Editor::fix_margins`.
    no_below: gtk::TextTag,
    /// Made when first needed: numbered items' margins by (level, digits),
    /// the room under a line for its pictures by height, and the room a
    /// line has above it with the room moved down from the line before.
    table: gtk::TextTagTable,
    numbers: RefCell<HashMap<(usize, u8), gtk::TextTag>>,
    picture_room: RefCell<HashMap<i32, gtk::TextTag>>,
    lifts: RefCell<HashMap<i32, gtk::TextTag>>,
    zoom: Cell<f64>,
}

impl Tags {
    fn new(table: &gtk::TextTagTable) -> Tags {
        let add = |tag: gtk::TextTag| {
            table.add(&tag);
            tag
        };
        let plain = |name: &str| add(gtk::TextTag::builder().name(name).build());
        let heading = |level: usize, size: f64, weight: i32| {
            add(gtk::TextTag::builder()
                .name(format!("h{level}"))
                .scale(size / 15.0)
                .weight(weight)
                .build())
        };
        let tags = Tags {
            body: add(gtk::TextTag::builder()
                .name("body")
                .line_height(LINE_HEIGHT as f32)
                // so the view's own room above reaches only an empty last
                // line, which has no text to take tags (`fix_margins`)
                .pixels_above_lines(0)
                .build()),
            headings: [
                heading(1, 20.0, 500),
                heading(2, 17.0, 500),
                heading(3, 16.0, 600),
                heading(4, 15.0, 600),
                heading(5, 14.0, 600),
                heading(6, 13.0, 600),
            ],
            list_start: plain("list-start"),
            levels: (0..LEVELS).map(|i| plain(&format!("level-{i}"))).collect(),
            done: plain("done"),
            quote_levels: (0..LEVELS).map(|i| plain(&format!("quote-{i}"))).collect(),
            quote_text: plain("quote-text"),
            quote_first: plain("quote-first"),
            quote_last: plain("quote-last"),
            alert_title: plain("alert-title"),
            bold: add(gtk::TextTag::builder().name("bold").weight(500).build()),
            italic: add(gtk::TextTag::builder()
                .name("italic")
                .style(pango::Style::Italic)
                .build()),
            underline: add(gtk::TextTag::builder()
                .name("underline")
                .underline(pango::Underline::Single)
                .build()),
            strike: add(gtk::TextTag::builder()
                .name("strike")
                .strikethrough(true)
                .build()),
            raw: plain("raw"),
            escaped: plain("escaped"),
            italic_us: plain("italic-us"),
            bold_us: plain("bold-us"),
            mono_block: add(gtk::TextTag::builder()
                .name("mono-block")
                .family(MONO)
                .scale(MONO_SCALE)
                .line_height(1.1)
                .build()),
            code_line: plain("code-line"),
            table_line: plain("table-line"),
            table_first: plain("table-first"),
            table_last: plain("table-last"),
            // the fences are small: hidden, they are the box's padding
            fence_open: add(gtk::TextTag::builder()
                .name("fence-open")
                .scale(MONO_SCALE * 0.75)
                .build()),
            fence_close: add(gtk::TextTag::builder()
                .name("fence-close")
                .scale(MONO_SCALE * 0.75)
                .build()),
            indent_first: plain("indent-first"),
            indent_last: plain("indent-last"),
            table_head: add(gtk::TextTag::builder()
                .name("table-head")
                .weight(700)
                .build()),
            // the |---| line is a thin band between the head and the rows
            table_rule: add(gtk::TextTag::builder()
                .name("table-rule")
                .scale(MONO_SCALE * 0.5)
                .pixels_above_lines(0)
                .pixels_below_lines(0)
                .build()),
            // upright: in italics a backslash leans into a bar
            math_block: add(gtk::TextTag::builder()
                .name("math-block")
                .family("serif")
                .scale(16.0 / 15.0)
                .justification(gtk::Justification::Center)
                .build()),
            math_line: plain("math-line"),
            setext_rule: add(gtk::TextTag::builder()
                .name("setext-rule")
                .scale(MONO_SCALE * 0.75)
                .build()),
            mono: add(gtk::TextTag::builder()
                .name("mono")
                .family(MONO)
                .scale(MONO_SCALE)
                .build()),
            mono_h1: add(gtk::TextTag::builder()
                .name("mono-h1")
                .family(MONO)
                .scale(MONO_SCALE * 20.0 / 15.0)
                .build()),
            mono_h2: add(gtk::TextTag::builder()
                .name("mono-h2")
                .family(MONO)
                .scale(MONO_SCALE * 17.0 / 15.0)
                .build()),
            kbd: add(gtk::TextTag::builder()
                .name("kbd")
                .family(MONO)
                .scale(12.0 / 15.0)
                .build()),
            code_box: plain("code-box"),
            kbd_box: plain("kbd-box"),
            link: plain("link"),
            deco_bold: add(gtk::TextTag::builder()
                .name("deco-bold")
                .weight(500)
                .build()),
            deco_italic: add(gtk::TextTag::builder()
                .name("deco-italic")
                .style(pango::Style::Italic)
                .build()),
            deco_strike: add(gtk::TextTag::builder()
                .name("deco-strike")
                .strikethrough(true)
                .build()),
            deco_underline: add(gtk::TextTag::builder()
                .name("deco-underline")
                .underline(pango::Underline::Single)
                .build()),
            mark: plain("mark"),
            sup: add(gtk::TextTag::builder().name("sup").scale(0.72).build()),
            sub: add(gtk::TextTag::builder().name("sub").scale(0.72).build()),
            math: add(gtk::TextTag::builder()
                .name("math")
                .family("serif")
                .scale(16.0 / 15.0)
                .build()),
            footnote: add(gtk::TextTag::builder().name("footnote").weight(600).build()),
            footnote_line: add(gtk::TextTag::builder()
                .name("footnote-line")
                .scale(13.0 / 15.0)
                .build()),
            comment: add(gtk::TextTag::builder()
                .name("comment")
                .style(pango::Style::Italic)
                .build()),
            alert_labels: [
                Alert::Note,
                Alert::Tip,
                Alert::Important,
                Alert::Warning,
                Alert::Caution,
            ]
            .map(|alert| {
                add(gtk::TextTag::builder()
                    .name(format!("alert-{alert:?}").to_lowercase())
                    .weight(600)
                    .scale(13.0 / 15.0)
                    .foreground_rgba(&alert_color(alert))
                    .build())
            }),
            picture: plain("picture"),
            syntax: plain("syntax"),
            pad: add(gtk::TextTag::builder()
                .name("pad")
                .family(MONO)
                .scale(0.45)
                // alpha 0 reads as "no alpha" to Pango: nearly 0 it is
                .foreground_rgba(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.001))
                .build()),
            conceal: add(gtk::TextTag::builder()
                .name("conceal")
                .invisible(true)
                .build()),
            ghost: add(gtk::TextTag::builder()
                .name("ghost")
                .foreground_rgba(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.001))
                .build()),
            emoji: add(gtk::TextTag::builder()
                .name("emoji")
                .foreground_rgba(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.001))
                .build()),
            decorated: plain("decorated"),
            no_below: add(gtk::TextTag::builder()
                .name("no-below")
                .pixels_below_lines(0)
                .build()),
            table: table.clone(),
            numbers: RefCell::new(HashMap::new()),
            picture_room: RefCell::new(HashMap::new()),
            lifts: RefCell::new(HashMap::new()),
            zoom: Cell::new(1.0),
        };
        tags.set_zoom(1.0);
        tags
    }

    fn set_zoom(&self, zoom: f64) {
        self.zoom.set(zoom);
        let px = |v: f64| (v * zoom).round() as i32;
        let [h1, h2, h3, h4, h5, h6] = &self.headings;
        h1.set_pixels_below_lines(px(26.0));
        h2.set_pixels_above_lines(px(10.0));
        h2.set_pixels_below_lines(px(4.0));
        for tag in [h3, h4, h5, h6] {
            tag.set_pixels_above_lines(px(8.0));
            tag.set_pixels_below_lines(px(2.0));
        }
        self.list_start.set_pixels_above_lines(px(4.0));
        for (i, tag) in self.levels.iter().enumerate() {
            tag.set_left_margin(px(20.0 * (i + 1) as f64));
        }
        for ((level, digits), tag) in self.numbers.borrow().iter() {
            tag.set_left_margin(number_margin(*level, *digits, zoom).round() as i32);
        }
        for (i, tag) in self.quote_levels.iter().enumerate() {
            tag.set_left_margin(px(QUOTE_INDENT * (i + 1) as f64));
        }
        self.quote_first.set_pixels_above_lines(px(QUOTE_GAP + 2.0));
        self.quote_last.set_pixels_below_lines(px(QUOTE_GAP + 2.0));
        // the icon goes where the quote's text starts
        self.alert_title.set_left_margin(px(QUOTE_INDENT + 23.0));
        self.code_line.set_left_margin(px(14.0));
        self.code_line.set_right_margin(px(14.0));
        self.table_line.set_right_margin(px(4.0));
        // a row too wide for the window goes on, indented, inside its box
        // (a negative indent: GTK indents the lines after the first)
        self.table_line.set_indent(-px(TABLE_WRAP_INDENT));
        self.fence_open
            .set_pixels_above_lines(px(BLOCK_GAP + CODE_PAD));
        self.fence_close
            .set_pixels_below_lines(px(BLOCK_GAP + CODE_PAD));
        // as far into the box as a fenced block's first line
        self.indent_first
            .set_pixels_above_lines(px(BLOCK_GAP + 2.0 * CODE_PAD + 6.0));
        self.indent_last
            .set_pixels_below_lines(px(BLOCK_GAP + 2.0 * CODE_PAD + 6.0));
        self.math_line
            .set_pixels_above_lines(px(BLOCK_GAP + 2.0 * CODE_PAD + 2.0));
        self.math_line
            .set_pixels_below_lines(px(BLOCK_GAP + 2.0 * CODE_PAD + 2.0));
        self.table_line.set_pixels_above_lines(px(ROW_PAD));
        self.table_line.set_pixels_below_lines(px(ROW_PAD));
        self.table_first
            .set_pixels_above_lines(px(BLOCK_GAP + ROW_PAD + 2.0));
        self.table_last
            .set_pixels_below_lines(px(BLOCK_GAP + ROW_PAD + 2.0));
        // superscript and subscript sit above and below the text
        self.sup.set_rise(px(5.0) * pango::SCALE);
        self.footnote.set_rise(px(5.0) * pango::SCALE);
        self.sub.set_rise(-px(3.0) * pango::SCALE);
        self.footnote.set_scale(0.72);
        // an emoji is about 1.2em wide; a colon 0.25em
        self.emoji
            .set_letter_spacing(px(15.0 * 0.95) * pango::SCALE);
        for tag in &self.alert_labels {
            tag.set_letter_spacing((0.6 * zoom * pango::SCALE as f64) as i32);
        }
    }

    /// Colours that follow the theme.
    fn set_ink(&self, ink: &gdk::RGBA) {
        self.syntax.set_foreground_rgba(Some(&tint(ink, 0.4)));
        self.done.set_foreground_rgba(Some(&tint(ink, 0.5)));
        self.quote_text.set_foreground_rgba(Some(&tint(ink, 0.78)));
        self.footnote_line
            .set_foreground_rgba(Some(&tint(ink, 0.8)));
        self.comment.set_foreground_rgba(Some(&tint(ink, 0.5)));
    }

    fn heading(&self, level: u8) -> &gtk::TextTag {
        &self.headings[(level.clamp(1, 6) - 1) as usize]
    }

    /// The margin of a numbered item at `level` in a list whose widest
    /// number has `digits` digits.
    fn number_margin(&self, level: usize, digits: u8) -> gtk::TextTag {
        self.numbers
            .borrow_mut()
            .entry((level, digits))
            .or_insert_with(|| {
                let tag = gtk::TextTag::builder()
                    .name(format!("number-{level}-{digits}"))
                    .left_margin(number_margin(level, digits, self.zoom.get()).round() as i32)
                    .build();
                self.table.add(&tag);
                tag
            })
            .clone()
    }

    /// Room under a line for pictures `height` tall.
    fn picture_room(&self, height: i32) -> gtk::TextTag {
        self.picture_room
            .borrow_mut()
            .entry(height)
            .or_insert_with(|| {
                let tag = gtk::TextTag::builder()
                    .name(format!("picture-room-{height}"))
                    .pixels_below_lines(height)
                    .build();
                self.table.add(&tag);
                // `no_below` must still win over it
                self.no_below.set_priority(self.table.size() - 1);
                tag
            })
            .clone()
    }

    /// Room above a line, `pixels` (its own and what moved down to it).
    /// Made after the paragraph tags, so it wins over theirs.
    fn lift(&self, pixels: i32) -> gtk::TextTag {
        self.lifts
            .borrow_mut()
            .entry(pixels)
            .or_insert_with(|| {
                let tag = gtk::TextTag::builder()
                    .name(format!("lift-{pixels}"))
                    .pixels_above_lines(pixels)
                    .build();
                self.table.add(&tag);
                tag
            })
            .clone()
    }

    fn char_tags(&self) -> [(&gtk::TextTag, Style); 8] {
        [
            (&self.bold, Style::BOLD),
            (&self.italic, Style::ITALIC),
            (&self.underline, Style::UNDERLINE),
            (&self.strike, Style::STRIKE),
            (&self.raw, Style::RAW),
            (&self.escaped, Style::ESCAPED),
            (&self.italic_us, Style::ITALIC_US),
            (&self.bold_us, Style::BOLD_US),
        ]
    }

    /// Paragraph tags `apply_kind_tags` puts on (code and tables aside).
    fn line_tags(&self) -> Vec<gtk::TextTag> {
        let mut tags: Vec<gtk::TextTag> = self
            .headings
            .iter()
            .chain([
                &self.list_start,
                &self.done,
                &self.quote_text,
                &self.quote_first,
                &self.quote_last,
                &self.alert_title,
                &self.math_block,
                &self.math_line,
                &self.setext_rule,
                // also on code fences, but display math's have no mono-block
                &self.fence_open,
                &self.fence_close,
            ])
            .chain(self.levels.iter())
            .chain(self.quote_levels.iter())
            .cloned()
            .collect();
        tags.extend(self.numbers.borrow().values().cloned());
        tags
    }

    /// Whole-line tags of code blocks and tables (see `apply_kind_tags`).
    fn block_tags(&self) -> [&gtk::TextTag; 11] {
        [
            &self.table_rule,
            &self.mono_block,
            &self.code_line,
            &self.fence_open,
            &self.fence_close,
            &self.indent_first,
            &self.indent_last,
            &self.table_line,
            &self.table_first,
            &self.table_last,
            &self.table_head,
        ]
    }

    /// What `decorate_line` puts on.
    fn deco_tags(&self) -> Vec<gtk::TextTag> {
        let mut tags: Vec<gtk::TextTag> = [
            &self.decorated,
            &self.ghost,
            &self.mono,
            &self.mono_h1,
            &self.mono_h2,
            &self.kbd,
            &self.code_box,
            &self.kbd_box,
            &self.link,
            &self.deco_bold,
            &self.deco_italic,
            &self.deco_strike,
            &self.deco_underline,
            &self.mark,
            &self.sup,
            &self.sub,
            &self.math,
            &self.footnote,
            &self.footnote_line,
            &self.comment,
            &self.picture,
            &self.syntax,
            &self.pad,
            &self.conceal,
            &self.emoji,
        ]
        .into_iter()
        .chain(self.alert_labels.iter())
        .cloned()
        .collect();
        tags.extend(self.picture_room.borrow().values().cloned());
        tags
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
    /// The character typed, if this step typed one: a space after a word
    /// starts a new undo step, so undo goes back a word at a time.
    typed: Option<char>,
    at: Instant,
}

// ---------------------------------------------------------------------------
// The open document

struct Doc {
    /// `None` for a new note that has not been written yet.
    path: Option<PathBuf>,
    /// The folder of a note whose file name follows its title: notes in the
    /// notes folder, and notes started in Kludd while they are open.
    managed: Option<PathBuf>,
    /// Created in this session; trashed again if left empty.
    created: bool,
    /// Plain text, not Markdown (a `.txt`): every line is raw, there is no
    /// formatting, and the file is saved exactly as typed.
    plain: bool,
    final_eol: Eol,
    /// Line break this note uses (CRLF files stay CRLF when typing Enter).
    newline: Eol,
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
    /// Where the toolbar sits for the current selection (start, end, x):
    /// formatting the same selection (say H1, which makes the text bigger)
    /// must not slide it away from under the mouse.
    toolbar_x: Option<(i32, i32, i32)>,
    writing: bool,
    save_again: bool,
    /// Lines whose syntax is shown: the cursor's and the selection end's
    /// (`cursor_lines`), and the block they are in (`revealed`, sorted).
    cursor_lines: Vec<usize>,
    revealed: Vec<usize>,
    /// A table is waiting to be lined up after an edit in it.
    align_pending: bool,
}

#[derive(Default)]
struct Callbacks {
    back: Option<Box<dyn Fn()>>,
    status: Option<Box<dyn Fn(String)>>,
    error: Option<Box<dyn Fn(String)>>,
    open_file: Option<Box<dyn Fn(PathBuf)>>,
    word_count: Option<Box<dyn Fn(bool)>>,
}

struct Inner {
    root: gtk::Overlay,
    find: Rc<find::FindBar>,
    word_count: Rc<stats::WordCount>,
    /// A recount is waiting for the typing to pause.
    recount_timer: RefCell<Option<glib::SourceId>>,
    /// Where finding starts from: the cursor when the bar opened, then
    /// the match last gone to. A mark, so edits before it move it along.
    find_from: gtk::TextMark,
    toolbar_buttons: RefCell<Vec<(&'static str, gtk::Button)>>,
    view: View,
    buffer: gtk::TextBuffer,
    tags: Tags,
    toolbar: gtk::Popover,
    state: RefCell<State>,
    callbacks: RefCell<Callbacks>,
    /// The apply-tag/remove-tag handlers that mark lines' Markdown stale.
    tag_handlers: RefCell<Vec<glib::SignalHandlerId>>,
    /// Pictures linked from notes, by file: `None` while loading (or if
    /// it could not be read).
    pictures: RefCell<HashMap<PathBuf, Option<gdk::Texture>>>,
    /// The note's link reference definitions (`[label]: url`), by label.
    references: RefCell<HashMap<String, String>>,
}

#[derive(Clone)]
pub struct Editor(Rc<Inner>);

impl Editor {
    pub fn new() -> Editor {
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
        scroll::smooth_wheel(&scroller);

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
        // room for the back button and the find bar side by side
        column.set_size_request(340, -1);
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
        // find in the note: top right, level with the back button
        let find = find::FindBar::new();
        find.widget.set_margin_top(14);
        find.widget.set_margin_end(20);
        // never over the back button, however narrow the window
        find.widget.set_margin_start(96);
        root.add_overlay(&find.widget);
        // the word count: bottom left, just above the status bar, under
        // the text rather than over it
        let word_count = stats::WordCount::new();
        word_count.counter.set_margin_top(4);
        word_count.counter.set_margin_bottom(8);
        column.append(&word_count.counter);
        word_count.menu.set_parent(&root);

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
            find,
            find_from: gtk::TextMark::new(Some("find-from"), true),
            word_count,
            recount_timer: RefCell::new(None),
            toolbar_buttons: RefCell::new(Vec::new()),
            view,
            buffer,
            tags,
            toolbar,
            state: RefCell::new(State::default()),
            callbacks: RefCell::new(Callbacks::default()),
            tag_handlers: RefCell::new(Vec::new()),
            pictures: RefCell::new(HashMap::new()),
            references: RefCell::new(HashMap::new()),
        }));
        editor.build_toolbar();
        editor.connect_buffer();
        editor.connect_keys();
        editor.connect_pointer();
        editor.connect_clipboard();
        editor.connect_links();
        editor.connect_find();
        editor.connect_word_count();
        let weak = editor.downgrade();
        editor
            .0
            .view
            .imp()
            .pictures
            .replace(Some(Box::new(move |n| {
                upgrade(&weak).map_or_else(Vec::new, |editor| editor.pictures(n))
            })));
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

    /// Called with a note a link points to (`[plan](plan.md)`).
    pub fn connect_open_file(&self, f: impl Fn(PathBuf) + 'static) {
        self.0.callbacks.borrow_mut().open_file = Some(Box::new(f));
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

    pub fn set_theme(&self, theme: &theme::Theme) {
        let ink = gdk::RGBA::parse(theme.ink).unwrap_or(gdk::RGBA::BLACK);
        self.0.tags.set_ink(&ink);
        self.0.view.imp().accent.replace(Some(theme.accent_rgba()));
        self.0.view.imp().ink.replace(Some(ink));
        self.0.view.queue_draw();
    }

    pub fn set_zoom(&self, zoom: f64) {
        self.0.view.imp().zoom.set(zoom);
        self.0.tags.set_zoom(zoom);
        // the rooms moved between lines are in pixels: as big as now
        for n in 0..self.line_count() {
            self.fix_room_below(n);
        }
        for n in 0..self.line_count() {
            self.fix_room_above(n);
        }
        self.fix_margins(self.line_count().saturating_sub(1));
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
        // move the toolbar in place once the text is laid out again; never
        // hide it in between, or a quick second click lands in the text
        if self.0.toolbar.is_visible() {
            let weak = self.downgrade();
            glib::timeout_add_local_once(Duration::from_millis(30), move || {
                if let Some(editor) = upgrade(&weak)
                    && editor.0.toolbar.is_visible()
                {
                    editor.show_toolbar();
                }
            });
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
        let (final_eol, plain_file) = self
            .0
            .state
            .borrow()
            .doc
            .as_ref()
            .map_or((Eol::Lf, false), |d| (d.final_eol, d.plain));
        let count = self.line_count();
        self.check_line_table();
        // line endings straight from the text: one copy instead of thousands
        // of iterator walks
        let buffer = &self.0.buffer;
        let plain = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
        if plain_file {
            return format!("{plain}{}", final_eol.as_str());
        }
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
        // they cover whole lines, so the ones at its start are all there
        // are (see `after_delete` for lines joined together); looking costs
        // less than removing what is not there
        let line_tags = tags.line_tags();
        for tag in start.tags() {
            if line_tags.contains(&tag) {
                buffer.remove_tag(&tag, &start, &end);
            }
        }
        // every code block and table line has mono-block
        if has_tag_in(&start, &end, &tags.mono_block) {
            for tag in tags.block_tags() {
                buffer.remove_tag(tag, &start, &end);
            }
        }
        buffer.apply_tag(&tags.body, &start, &end);
        self.apply_kind_tags(line, &start, &end);
        self.fix_margins(line);
    }

    /// Heading / list / quote paragraph tags for one line (nothing removed
    /// first).
    fn apply_kind_tags(&self, line: usize, start: &gtk::TextIter, end: &gtk::TextIter) {
        let buffer = &self.0.buffer;
        let tags = &self.0.tags;
        let view = &self.0.view;
        let kind = view.kind(line);
        let block = view.block(line);
        let shape = view.shape(line);
        if let Some(level) = kind.heading_level() {
            buffer.apply_tag(tags.heading(level), start, end);
        }
        match kind {
            LineKind::Bullet { .. } | LineKind::Numbered { .. } => {
                let level = (shape.level as usize).min(LEVELS - 1);
                let margin = match kind {
                    LineKind::Numbered { task: None, .. } => {
                        tags.number_margin(level, shape.digits)
                    }
                    _ => tags.levels[level].clone(),
                };
                buffer.apply_tag(&margin, start, end);
                if !(line > 0 && view.kind(line - 1).is_list()) {
                    buffer.apply_tag(&tags.list_start, start, end);
                }
                if matches!(kind.task(), Some(Task::Done(_))) {
                    buffer.apply_tag(&tags.done, start, end);
                }
            }
            LineKind::Quote { depth, .. } => self.apply_quote_tags(depth, shape, start, end),
            _ => {}
        }
        match block {
            Block::Quote { depth } => self.apply_quote_tags(depth, shape, start, end),
            Block::SetextText { level } => buffer.apply_tag(tags.heading(level), start, end),
            Block::SetextRule { .. } => buffer.apply_tag(&tags.setext_rule, start, end),
            Block::MathOpen => buffer.apply_tag(&tags.fence_open, start, end),
            Block::MathClose => buffer.apply_tag(&tags.fence_close, start, end),
            Block::Math => buffer.apply_tag(&tags.math_block, start, end),
            Block::MathLine => {
                buffer.apply_tag(&tags.math_block, start, end);
                buffer.apply_tag(&tags.math_line, start, end);
            }
            _ => {}
        }
        if !block.is_code() && !block.is_table() {
            return;
        }
        buffer.apply_tag(&tags.mono_block, start, end);
        let tag = if block.is_code() {
            &tags.code_line
        } else {
            &tags.table_line
        };
        buffer.apply_tag(tag, start, end);
        if let Block::Indented { first, last } = block {
            if first {
                buffer.apply_tag(&tags.indent_first, start, end);
            }
            if last {
                buffer.apply_tag(&tags.indent_last, start, end);
            }
            return;
        }
        let edge = match block {
            Block::FenceOpen => &tags.fence_open,
            Block::FenceClose => &tags.fence_close,
            Block::TableHead => {
                buffer.apply_tag(&tags.table_head, start, end);
                &tags.table_first
            }
            Block::TableEnd => &tags.table_last,
            Block::TableRule => &tags.table_rule,
            _ => return,
        };
        buffer.apply_tag(edge, start, end);
    }

    fn apply_quote_tags(
        &self,
        depth: u8,
        shape: Shape,
        start: &gtk::TextIter,
        end: &gtk::TextIter,
    ) {
        let buffer = &self.0.buffer;
        let tags = &self.0.tags;
        let level = (depth as usize).clamp(1, LEVELS) - 1;
        buffer.apply_tag(&tags.quote_levels[level], start, end);
        if shape.quote_first {
            buffer.apply_tag(&tags.quote_first, start, end);
        }
        if shape.quote_last {
            buffer.apply_tag(&tags.quote_last, start, end);
        }
        match shape.alert {
            None => buffer.apply_tag(&tags.quote_text, start, end),
            // the title line, `[!NOTE]`, makes room for the alert's icon
            Some(_) if shape.quote_first => {
                let line = start.line() as usize;
                if matches!(self.0.view.kind(line), LineKind::Quote { .. }) {
                    buffer.apply_tag(&tags.alert_title, start, end);
                }
            }
            Some(_) => {}
        }
    }

    fn refresh_lines(&self, first: usize, last: usize) {
        for n in first..=(last + 1).min(self.line_count().saturating_sub(1)) {
            self.apply_line_tags(n);
            self.decorate_line(n, None);
        }
        self.0.view.queue_draw();
    }

    /// Works out the blocks and shapes again; lines whose part in one
    /// changed are redrawn.
    fn update_blocks(&self) {
        self.update_layout(true);
    }

    /// Works out the blocks (code, tables, math, rules, quotes kept as
    /// written) again, and, if `reshape` or a block changed, the shapes
    /// (list nesting and numbers, quotes, alerts). Typing inside a line
    /// mostly changes neither: see `reshapes`.
    fn update_layout(&self, reshape: bool) {
        let count = self.line_count();
        let view = &self.0.view;
        let buffer = &self.0.buffer;
        let plain = self.plain();
        let (raw, shaped) = {
            let kinds = view.imp().kinds.borrow();
            (
                !plain && kinds.contains(&LineKind::Raw),
                !plain
                    && kinds
                        .iter()
                        .any(|k| k.is_list() || matches!(k, LineKind::Quote { .. })),
            )
        };
        // reference definitions are raw lines: with none left, links to
        // them are plain text again
        if !raw && !self.0.references.borrow().is_empty() {
            self.0.references.borrow_mut().clear();
            for n in 0..count {
                if self.line_model(n).text().contains('[') {
                    self.decorate_line(n, None);
                }
            }
        }
        // without raw lines there are no blocks: unless the shapes may have
        // changed (or there are none to clear), there is nothing to do
        if !raw && (!reshape || !shaped) {
            let blocks = view.imp().blocks.borrow();
            let shapes = view.imp().shapes.borrow();
            let clean = blocks.len() == count
                && shapes.len() == count
                && blocks.iter().all(|b| *b == Block::None)
                && (!reshape || shapes.iter().all(|s| *s == Shape::default()));
            if clean {
                return;
            }
        }
        let kinds = view.imp().kinds.borrow().clone();
        // the whole text once, rather than line by line, when blocks need it
        let all = raw.then(|| buffer.text(&buffer.start_iter(), &buffer.end_iter(), true));
        let ranges = all.as_ref().map(|t| markdown::split_lines(t));
        let line_text = |n: usize| -> String {
            match (&all, &ranges) {
                (Some(text), Some(ranges)) => ranges
                    .get(n)
                    .map_or(String::new(), |(r, _)| text[r.clone()].to_string()),
                _ => buffer
                    .text(&self.line_start(n), &self.line_content_end(n), true)
                    .to_string(),
            }
        };
        let mut references = HashMap::new();
        let mut bracketed = Vec::new();
        let blocks = match (&all, &ranges) {
            (Some(text), Some(ranges)) => {
                let lines: Vec<(LineKind, &str)> = (0..count)
                    .map(|n| {
                        let line = ranges.get(n).map_or("", |(r, _)| &text[r.clone()]);
                        (kinds.get(n).copied().unwrap_or(LineKind::Text), line)
                    })
                    .collect();
                for (n, (kind, line)) in lines.iter().enumerate() {
                    if line.contains('[') {
                        bracketed.push(n);
                    }
                    if *kind == LineKind::Raw
                        && let Some((label, url)) = markdown::reference_definition(line)
                    {
                        references.entry(label).or_insert(url);
                    }
                }
                markdown::blocks(&lines)
            }
            _ => vec![Block::None; count],
        };
        // links to a definition that came or went look different now
        if *self.0.references.borrow() != references {
            self.0.references.replace(references);
            for n in bracketed {
                self.decorate_line(n, None);
            }
        }
        let mut changed = Vec::new();
        {
            let mut old = view.imp().blocks.borrow_mut();
            old.resize(count, Block::None);
            for (n, block) in blocks.iter().enumerate() {
                if old[n] != *block {
                    old[n] = *block;
                    changed.push(n);
                }
            }
        }
        if reshape || !changed.is_empty() {
            let shapes = if shaped || raw {
                markdown::shapes(&kinds, &blocks, line_text)
            } else {
                vec![Shape::default(); count]
            };
            let mut old = view.imp().shapes.borrow_mut();
            old.resize(count, Shape::default());
            for (n, shape) in shapes.into_iter().enumerate() {
                if old[n] != shape {
                    old[n] = shape;
                    changed.push(n);
                }
            }
        }
        changed.sort_unstable();
        changed.dedup();
        for &n in &changed {
            self.apply_line_tags(n);
            self.decorate_line(n, None);
        }
        if !changed.is_empty() {
            view.queue_draw();
        }
    }

    /// Whether an edit inside line `n` (which was `before`) can change the
    /// shapes: they hang on the lines' kinds, on whether the other lines in
    /// a list are blank or indented, and on a quote's first line.
    fn reshapes(&self, n: usize, before: &Line) -> bool {
        let kind = self.0.view.kind(n);
        if kind != before.kind {
            return true;
        }
        match kind {
            // a quote kept as written may be an alert's title
            LineKind::Raw if matches!(self.0.view.block(n), Block::Quote { .. }) => true,
            LineKind::Text | LineKind::Raw => {
                let lead = |text: &str| {
                    let trimmed = text.trim_start();
                    (trimmed.is_empty(), text.len() - trimmed.len())
                };
                lead(&before.text()) != lead(&self.line_model(n).text())
            }
            LineKind::Quote { .. } => true,
            _ => false,
        }
    }

    /// Re-applies what is drawn inside a line: faint fences and table pipes,
    /// code spans, keys, links, pictures and the rest. The syntax around
    /// them hides unless the cursor is at it; in a table it always shows,
    /// so the columns stay lined up. `line` is the line's model, if at hand.
    fn decorate_line(&self, n: usize, line: Option<&Line>) {
        self.decorate(n, line, false);
    }

    fn decorate(&self, n: usize, line: Option<&Line>, fresh: bool) {
        // decorations never change the Markdown: the line's cache stays good
        let buffer = &self.0.buffer;
        for id in self.0.tag_handlers.borrow().iter() {
            buffer.block_signal(id);
        }
        self.put_decorations(n, line, fresh);
        self.fix_margins(n);
        for id in self.0.tag_handlers.borrow().iter() {
            buffer.unblock_signal(id);
        }
    }

    /// GTK 4 aborts the whole app ("Byte index … is off the end of the
    /// line") when it looks up the point in the room below a line that
    /// has hidden text: a click there, a drag over it, Page Down landing
    /// on it. It takes the line's length with the hidden text as one
    /// without (`gtk_text_layout_get_iter_at_position`). So no line with
    /// hidden text gets room below it: that room becomes room above the
    /// next line (the last line's goes below the text), and the drawing
    /// puts it back where it belongs (`View::true_yrange`).
    fn fix_margins(&self, n: usize) {
        let count = self.line_count();
        if count == 0 {
            return;
        }
        let last = count - 1;
        let first = n.saturating_sub(1).min(last);
        for m in first..=(n + 1).min(last) {
            self.fix_room_below(m);
        }
        for m in first..=(n + 3).min(last) {
            self.fix_room_above(m);
        }
        let view = &self.0.view;
        // an empty last line has no text to tag: its lift is the view's
        // room above lines, which every other line has `body` to override
        let last_start = self.line_start(last);
        let empty_last = last > 0 && last_start.is_end();
        let above_last = if empty_last {
            view.carried_into(last)
        } else {
            0
        };
        if view.pixels_above_lines() != above_last {
            view.set_pixels_above_lines(above_last);
        }
        let below_all = view.carried_into(count);
        if view.bottom_margin() != below_all {
            view.set_bottom_margin(below_all);
        }
    }

    /// Takes the room below line `m` away if it has hidden text (or gives
    /// it back if it no longer has).
    fn fix_room_below(&self, m: usize) {
        let (start, end) = self.line_span(m);
        let tags = &self.0.tags;
        let has = start.has_tag(&tags.no_below);
        let want = room_below(&start) > 0 && has_tag_in(&start, &end, &tags.conceal);
        if want != has {
            let buffer = &self.0.buffer;
            if want {
                buffer.apply_tag(&tags.no_below, &start, &end);
            } else {
                buffer.remove_tag(&tags.no_below, &start, &end);
            }
        }
    }

    /// Puts the room taken from below line `m - 1` above line `m`, on top
    /// of the room it has above it already.
    fn fix_room_above(&self, m: usize) {
        let (start, end) = self.line_span(m);
        let moved = match self.0.view.hidden_whole(&start) {
            true => 0,
            false => self.0.view.carried_into(m),
        };
        let want = (moved > 0).then(|| self.0.tags.lift(room_above(&start) + moved));
        let buffer = &self.0.buffer;
        for tag in start.tags() {
            let lift = tag.name().is_some_and(|n| n.starts_with("lift-"));
            if lift && Some(&tag) != want.as_ref() {
                buffer.remove_tag(&tag, &start, &end);
            }
        }
        if let Some(tag) = want
            && !start.has_tag(&tag)
        {
            buffer.apply_tag(&tag, &start, &end);
        }
    }

    /// `fresh`: the line has no tags yet (it was just written).
    fn put_decorations(&self, n: usize, line: Option<&Line>, fresh: bool) {
        if n >= self.line_count() {
            return;
        }
        let buffer = &self.0.buffer;
        let tags = &self.0.tags;
        let block = self.0.view.block(n);
        let shape = self.0.view.shape(n);
        let fence = matches!(
            block,
            Block::FenceOpen | Block::FenceClose | Block::MathOpen | Block::MathClose
        );
        let faint = fence || block == Block::TableRule;
        let table = block.is_table();
        let owned;
        let line = match line {
            Some(line) => line,
            None => {
                owned = self.line_model(n);
                &owned
            }
        };
        let items = if self.plain() || block.is_opaque() {
            Vec::new()
        } else {
            self.line_inlines(line)
        };
        // a link reference definition: quiet, like a footnote
        let definition =
            line.kind == LineKind::Raw && markdown::reference_definition(&line.text()).is_some();
        let alert_title = shape.alert.is_some()
            && shape.quote_first
            && matches!(line.kind, LineKind::Quote { .. });
        let special = table
            || faint
            || alert_title
            || definition
            || matches!(
                block,
                Block::Indented { .. }
                    | Block::MathLine
                    | Block::Rule
                    | Block::SetextRule { .. }
                    | Block::Quote { .. }
            );
        // (a plain text file has no blocks: nothing is faint or a table there)
        let bare = items.is_empty() && !special;
        if fresh && bare {
            return;
        }
        let start = self.line_start(n);
        let end = self.line_content_end(n);
        // (a heading's hidden === line hides its line break too)
        let (_, span_end) = self.line_span(n);
        if !fresh && has_tag_in(&start, &end, &tags.decorated) {
            for tag in tags.deco_tags() {
                if has_tag_in(&start, &span_end, &tag) {
                    buffer.remove_tag(&tag, &start, &span_end);
                }
            }
        }
        if bare || start == end {
            return;
        }
        buffer.apply_tag(&tags.decorated, &start, &end);
        // like GitHub, a code block's fences and a table's pipes only show
        // while it is being edited (a table that is not lined up keeps them)
        let cursor_lines = self.cursor_lines();
        let editing = || {
            self.block_around(n).is_some_and(|(first, last)| {
                cursor_lines.iter().any(|c| (first..=last).contains(c))
            })
        };
        let here = cursor_lines.contains(&n);
        let quiet = if fence {
            (!editing()).then_some(&tags.conceal)
        } else if table && !editing() && self.table_lined_up(n) {
            Some(&tags.ghost)
        } else {
            None
        };
        if faint || definition {
            if definition {
                buffer.apply_tag(&tags.footnote_line, &start, &end);
            }
            buffer.apply_tag(quiet.unwrap_or(&tags.syntax), &start, &end);
            return;
        }
        let origin = start.offset();
        let apply = |tag: &gtk::TextTag, from: usize, to: usize| {
            if from < to {
                let a = buffer.iter_at_offset(origin + from as i32);
                let b = buffer.iter_at_offset(origin + to as i32);
                buffer.apply_tag(tag, &a, &b);
            }
        };
        let text = line.text();
        let len = text.chars().count();
        // a rule's dashes, a heading's ===, display math's $$ and indented
        // code's indent show only while the cursor is there
        let hide_or_fade = |shown: bool, from: usize, to: usize| {
            apply(if shown { &tags.syntax } else { &tags.conceal }, from, to);
        };
        match block {
            Block::Rule => {
                hide_or_fade(here, 0, len);
                return;
            }
            // the whole line, so it takes no room
            Block::SetextRule { .. } => {
                if editing() {
                    apply(&tags.syntax, 0, len);
                } else if span_end.line() as usize > n {
                    buffer.apply_tag(&tags.conceal, &start, &span_end);
                } else {
                    apply(&tags.conceal, 0, len);
                }
                return;
            }
            Block::Indented { .. } => {
                if !editing() {
                    apply(&tags.conceal, 0, markdown::code_indent(&text).min(len));
                }
                return;
            }
            Block::MathLine => {
                let lead = text.chars().take_while(|c| *c == ' ').count();
                let trail = text.chars().rev().take_while(|c| c.is_whitespace()).count();
                hide_or_fade(here, 0, lead + 2);
                hide_or_fade(here, len - trail - 2, len);
                return;
            }
            Block::Quote { .. } => {
                let prefix = text[..markdown::quote_text(&text)].chars().count();
                hide_or_fade(here, 0, prefix);
            }
            _ => {}
        }
        if table {
            for pipe in markdown::table_pipes(&text) {
                apply(quiet.unwrap_or(&tags.syntax), pipe, pipe + 1);
            }
        }
        // an alert's `[!NOTE]` reads NOTE, in its colour
        if alert_title && let Some(alert) = shape.alert {
            let open = text.find("[!").map_or(0, |b| text[..b].chars().count());
            let close = text.rfind(']').map_or(len, |b| text[..b].chars().count());
            let label = &tags.alert_labels[alert as usize];
            hide_or_fade(here, open, open + 2);
            apply(label, open + 2, close);
            hide_or_fade(here, close, (close + 1).min(len));
        }
        let cursors: Vec<usize> = [buffer.get_insert(), buffer.selection_bound()]
            .iter()
            .map(|mark| buffer.iter_at_mark(mark))
            .filter(|iter| iter.line() as usize == n)
            .map(|iter| iter.line_offset() as usize)
            .collect();
        let mono = match line.kind {
            LineKind::H1 => &tags.mono_h1,
            LineKind::H2 => &tags.mono_h2,
            _ => &tags.mono,
        };
        let mut pictures = false;
        for item in items {
            let (whole, text) = (item.whole.clone(), item.text.clone());
            let at = cursors.iter().any(|c| whole.start <= *c && *c <= whole.end);
            let shown = table || text.is_empty() || at;
            // the syntax around the text: faint while shown, else hidden
            let syntax = |shown: bool| {
                let tag = if shown { &tags.syntax } else { &tags.conceal };
                apply(tag, whole.start, text.start.max(whole.start));
                apply(tag, text.end.min(whole.end), whole.end);
            };
            match item.kind {
                InlineKind::Code | InlineKind::Kbd => {
                    let code = item.kind == InlineKind::Code;
                    if !table {
                        apply(if code { mono } else { &tags.kbd }, whole.start, whole.end);
                    }
                    if shown {
                        apply(&tags.syntax, whole.start, text.start);
                        apply(&tags.syntax, text.end, whole.end);
                    } else {
                        // one character either side stays, unseen, as the
                        // room for the box's padding
                        apply(&tags.conceal, whole.start, text.start - 1);
                        apply(&tags.pad, text.start - 1, text.start);
                        apply(&tags.pad, text.end, text.end + 1);
                        apply(&tags.conceal, text.end + 1, whole.end);
                    }
                    let tag = if code { &tags.code_box } else { &tags.kbd_box };
                    apply(tag, text.start, text.end);
                }
                InlineKind::Link => {
                    apply(&tags.link, text.start, text.end);
                    syntax(shown);
                }
                InlineKind::Url => apply(&tags.link, whole.start, whole.end),
                InlineKind::Image => {
                    // a picture shown under the line stands for its source;
                    // one that cannot be shown reads as its alt text
                    let showing = self.picture_of(&item.url).is_some();
                    pictures |= showing;
                    if showing && !at && !table {
                        apply(&tags.conceal, whole.start, whole.end);
                    } else {
                        apply(&tags.link, text.start, text.end);
                        syntax(at || table || text.is_empty());
                    }
                }
                InlineKind::Bold
                | InlineKind::Italic
                | InlineKind::Strike
                | InlineKind::Underline
                | InlineKind::Highlight
                | InlineKind::Sup
                | InlineKind::Sub
                | InlineKind::Math => {
                    let tag = match item.kind {
                        InlineKind::Bold => &tags.deco_bold,
                        InlineKind::Italic => &tags.deco_italic,
                        InlineKind::Strike => &tags.deco_strike,
                        InlineKind::Underline => &tags.deco_underline,
                        InlineKind::Highlight => &tags.mark,
                        InlineKind::Sup => &tags.sup,
                        InlineKind::Sub => &tags.sub,
                        _ => &tags.math,
                    };
                    apply(tag, text.start, text.end);
                    syntax(shown);
                }
                InlineKind::Footnote | InlineKind::FootnoteDef => {
                    if item.kind == InlineKind::FootnoteDef {
                        apply(&tags.footnote_line, 0, len);
                    }
                    apply(&tags.footnote, text.start, text.end);
                    syntax(shown);
                }
                InlineKind::Emoji => {
                    if !shown {
                        apply(&tags.emoji, whole.start, whole.start + 1);
                        apply(&tags.conceal, whole.start + 1, whole.end);
                    }
                }
                InlineKind::Comment => apply(&tags.comment, whole.start, whole.end),
                InlineKind::Tag | InlineKind::Break => apply(&tags.syntax, whole.start, whole.end),
            }
        }
        // room under the line for its pictures
        if pictures {
            let shown = self.pictures(n);
            if !shown.is_empty() {
                let gap = (PICTURE_GAP * self.0.view.zoom() as f64).round() as f32;
                let total: f32 = shown.iter().map(|(_, _, h)| h + gap).sum::<f32>() + gap;
                apply(&tags.picture_room(total.round() as i32), 0, len);
                apply(&tags.picture, 0, len);
            }
        }
    }

    /// The lines of the cursor and the other end of the selection.
    fn cursor_lines(&self) -> Vec<usize> {
        let buffer = &self.0.buffer;
        let mut lines: Vec<usize> = [buffer.get_insert(), buffer.selection_bound()]
            .iter()
            .map(|mark| buffer.iter_at_mark(mark).line() as usize)
            .collect();
        lines.dedup();
        lines
    }

    /// Shows the syntax where the cursor is (and the fences of a code block
    /// it is in) and hides it where it was.
    fn update_reveal(&self) {
        if self.0.state.borrow().programmatic {
            return;
        }
        let cursors = self.cursor_lines();
        let mut lines = cursors.clone();
        for &line in &cursors {
            let block = self.0.view.block(line);
            match self.block_around(line) {
                // only their fences change
                Some((first, last))
                    if block.is_math()
                        || (block.is_code() && !matches!(block, Block::Indented { .. })) =>
                {
                    lines.extend([first, last]);
                }
                Some((first, last)) => lines.extend(first..=last),
                None => {}
            }
        }
        lines.sort_unstable();
        lines.dedup();
        let (old, old_cursors) = {
            let mut state = self.0.state.borrow_mut();
            (
                std::mem::replace(&mut state.revealed, lines.clone()),
                std::mem::replace(&mut state.cursor_lines, cursors.clone()),
            )
        };
        // what changed: lines coming into or out of view, and the lines
        // the cursor was and is on (the rest of a block it stays in, say a
        // long table, looks the same)
        let mut redo: Vec<usize> = old
            .iter()
            .filter(|n| lines.binary_search(n).is_err())
            .chain(lines.iter().filter(|n| old.binary_search(n).is_err()))
            .chain(old_cursors.iter())
            .chain(cursors.iter())
            .copied()
            .collect();
        redo.sort_unstable();
        redo.dedup();
        for n in redo {
            self.decorate_line(n, None);
        }
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
            let id = buffer.connect_local(signal, true, move |args| {
                let editor = upgrade(&weak)?;
                let start = args[2].get::<gtk::TextIter>().ok()?;
                let end = args[3].get::<gtk::TextIter>().ok()?;
                editor
                    .0
                    .view
                    .invalidate(start.line() as usize, end.line() as usize);
                None
            });
            self.0.tag_handlers.borrow_mut().push(id);
        }

        let weak = self.downgrade();
        buffer.connect_mark_set(move |_, _, mark| {
            let Some(editor) = upgrade(&weak) else { return };
            match mark.name().as_deref() {
                Some("insert") => {
                    if !editor.0.state.borrow().programmatic {
                        editor.0.state.borrow_mut().typing_style = None;
                    }
                    editor.update_reveal();
                    editor.schedule_toolbar();
                    editor.0.view.queue_draw();
                }
                Some("selection_bound") => {
                    editor.update_reveal();
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
        let next = kind.next();
        {
            let mut cache = self.0.view.imp().cache.borrow_mut();
            let at = (line + 1).min(cache.len());
            cache.splice(at..at, std::iter::repeat_n(None, breaks));
            let mut blocks = self.0.view.imp().blocks.borrow_mut();
            let block = blocks.get(line).copied().unwrap_or_default();
            let at = (line + 1).min(blocks.len());
            blocks.splice(at..at, std::iter::repeat_n(block, breaks));
            let mut shapes = self.0.view.imp().shapes.borrow_mut();
            let shape = shapes.get(line).copied().unwrap_or_default();
            let at = (line + 1).min(shapes.len());
            shapes.splice(at..at, std::iter::repeat_n(shape, breaks));
        }
        let mut kinds = self.0.view.imp().kinds.borrow_mut();
        let at = (line + 1).min(kinds.len());
        if programmatic {
            // Kludd writes whole lines and sets their kinds itself: the line
            // its text pushes down (the last one here) stays as it was
            kinds.splice(at..at, std::iter::repeat_n(kind, breaks));
        } else if iter.line_offset() == 0 && !iter.ends_line() {
            // Enter at the start of a line pushes it down whole: the new
            // lines above are empty (an empty list item, or text under a
            // heading), the line keeps its kind and a numbered item its
            // place in the count.
            let above = if kind.is_list() {
                kind.with_task(kind.task().map(|_| Task::Open))
            } else {
                next
            };
            let moved = match kind {
                LineKind::Numbered {
                    indent,
                    number,
                    delim,
                    task,
                } => LineKind::Numbered {
                    indent,
                    number: number.saturating_add(breaks as u32).min(999_999_999),
                    delim,
                    task,
                },
                other => other,
            };
            kinds[line] = above;
            kinds.splice(
                at..at,
                std::iter::repeat_n(above, breaks - 1).chain([moved]),
            );
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
        let reshape = breaks > 0 || before.len() != 1 || self.reshapes(line, &before[0]);
        self.update_layout(reshape);

        let after = self.models(line, line + breaks);
        let typing = breaks == 0 && chars == 1;
        self.push_step(Step {
            first: line,
            before,
            after,
            cursor_before,
            cursor_after: end.offset(),
            typing,
            typed: if typing { text.chars().next() } else { None },
            at: Instant::now(),
        });
        self.changed();
        if breaks == 0 && self.0.view.block(line).is_table() {
            self.schedule_align();
        }
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
                let mut blocks = self.0.view.imp().blocks.borrow_mut();
                let to = (l2 + 1).min(blocks.len());
                blocks.drain((l1 + 1).min(to)..to);
                let mut shapes = self.0.view.imp().shapes.borrow_mut();
                let to = (l2 + 1).min(shapes.len());
                shapes.drain((l1 + 1).min(to)..to);
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
        if before.len() > 1 {
            // the joined line still has the paragraph tags of the lines
            // that went into it, partway along
            let (start, end) = self.line_span(line);
            for tag in self.0.tags.line_tags() {
                self.0.buffer.remove_tag(&tag, &start, &end);
            }
        }
        self.refresh_lines(line, line);
        let reshape = before.len() != 1 || self.reshapes(line, &before[0]);
        self.update_layout(reshape);
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
        let single = before.len() == 1;
        let after = self.models(line, line);
        self.push_step(Step {
            first: line,
            before,
            after,
            cursor_before,
            cursor_after: at.offset(),
            typing,
            typed: None,
            at: Instant::now(),
        });
        self.changed();
        if single && self.0.view.block(line).is_table() {
            self.schedule_align();
        }
    }

    /// The line table must always have one entry per buffer line. Should a
    /// change ever slip past the handlers, pad or trim instead of crashing.
    fn check_line_table(&self) {
        let count = self.line_count();
        let mut cache = self.0.view.imp().cache.borrow_mut();
        if cache.len() != count {
            cache.resize(count, None);
        }
        let mut blocks = self.0.view.imp().blocks.borrow_mut();
        if blocks.len() != count {
            blocks.resize(count, Block::None);
        }
        let mut shapes = self.0.view.imp().shapes.borrow_mut();
        if shapes.len() != count {
            shapes.resize(count, Shape::default());
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
            let plain_line = line.kind == LineKind::Text || line.kind == LineKind::Raw;
            if !plain_line || self.0.view.block(first + i) != Block::None {
                let a = buffer.iter_at_offset(line_start);
                let b = buffer.iter_at_offset(offset);
                self.apply_kind_tags(first + i, &a, &b);
            }
            self.decorate(first + i, Some(line), true);
        }
        // the line after may start or continue a list
        self.apply_line_tags(first + lines.len());
        self.update_blocks();
        // a code block or table line was decorated before its block was
        // known: again, now that it is
        for n in first..first + lines.len() {
            if self.0.view.block(n) != Block::None {
                self.decorate_line(n, None);
            }
        }
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
        self.update_blocks();
        let after = self.models(first, last);
        if after != before {
            self.push_step(Step {
                first,
                before,
                after,
                cursor_before,
                cursor_after: self.0.buffer.cursor_position(),
                typing: false,
                typed: None,
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
        self.scroll_to_cursor();
        self.push_step(Step {
            first,
            before,
            after: lines,
            cursor_before,
            cursor_after,
            typing: false,
            typed: None,
            at: Instant::now(),
        });
        self.changed();
        self.relayout();
    }

    fn push_step(&self, step: Step) {
        let mut state = self.0.state.borrow_mut();
        state.redo.clear();
        // (a step can span a table that was lined up after typing in it)
        if let Some(last) = state.undo.last_mut()
            && last.typing
            && step.typing
            && step.before.len() == 1
            && step.after.len() == 1
            && last.before.len() == last.after.len()
            && (last.first..last.first + last.after.len()).contains(&step.first)
            && last.cursor_after == step.cursor_before
            && step.at.duration_since(last.at) < Duration::from_secs(1)
            // a space after a word starts the next undo step
            && (!step.typed.is_some_and(char::is_whitespace)
                || last.typed.is_some_and(char::is_whitespace))
        {
            let at = step.first - last.first;
            if let Some(line) = step.after.into_iter().next() {
                last.after[at] = line;
            }
            last.cursor_after = step.cursor_after;
            last.typed = step.typed;
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

    /// Brings the cursor into view once GTK has laid the text out (an
    /// immediate scroll uses estimated line heights and lands anywhere).
    fn scroll_to_cursor(&self) {
        let mark = self.0.buffer.get_insert();
        self.0.view.scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
    }

    /// The open file is plain text: it has no formatting to change.
    fn plain(&self) -> bool {
        self.0.state.borrow().doc.as_ref().is_some_and(|d| d.plain)
    }

    fn newline(&self) -> Eol {
        self.0
            .state
            .borrow()
            .doc
            .as_ref()
            .map_or(Eol::Lf, |d| d.newline)
    }

    fn place_cursor(&self, offset: i32) {
        let buffer = &self.0.buffer;
        buffer.place_cursor(&buffer.iter_at_offset(offset));
        self.scroll_to_cursor();
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
        if self.plain() {
            return;
        }
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
            Style::STRIKE => &self.0.tags.strike,
            _ => &self.0.tags.underline,
        };
        // raw lines (code, tables) keep their text exactly: no formatting
        let raw = |line: i32| self.0.view.kind(line as usize) == LineKind::Raw;
        let all = {
            let mut iter = start;
            let mut all = true;
            while iter < end {
                if !iter.ends_line() && !raw(iter.line()) && !iter.has_tag(tag) {
                    all = false;
                    break;
                }
                iter.forward_char();
            }
            all
        };
        let (first, last) = (start.line() as usize, end.line() as usize);
        let pieces: Vec<(gtk::TextIter, gtk::TextIter)> = (first..=last)
            .filter(|&n| !raw(n as i32))
            .map(|n| {
                let (a, b) = self.line_span(n);
                (a.max(start), b.min(end))
            })
            .collect();
        if pieces.is_empty() {
            return;
        }
        self.change(first, last, || {
            for (start, end) in &pieces {
                if all {
                    buffer.remove_tag(tag, start, end);
                    let us = if flag == Style::BOLD {
                        &self.0.tags.bold_us
                    } else {
                        &self.0.tags.italic_us
                    };
                    buffer.remove_tag(us, start, end);
                } else {
                    buffer.apply_tag(tag, start, end);
                }
            }
        });
    }

    /// A heading or plain text for the selected lines; choosing the kind
    /// they already have turns them back into text.
    pub fn set_line_kind(&self, kind: LineKind) {
        if self.plain() {
            return;
        }
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

    /// Highlights the selection (writes `==` around it), or takes the
    /// highlight off if it is one. A highlight stays on one line.
    pub fn toggle_highlight(&self) {
        if self.plain() {
            return;
        }
        let buffer = &self.0.buffer;
        let Some((start, end)) = buffer.selection_bounds() else {
            return;
        };
        let n = start.line() as usize;
        if end.line() as usize != n || self.0.view.kind(n) == LineKind::Raw {
            return;
        }
        let line = self.line_model(n);
        let (a, b) = (start.line_offset() as usize, end.line_offset() as usize);
        let line_start = self.line_start(n).offset();
        let (spans, selected) = match self.selected_highlight() {
            // drop the `==` either side
            Some(mark) => {
                let (left, rest) = split_spans(&line.spans, mark.whole.start);
                let (_, rest) = split_spans(&rest, mark.text.start - mark.whole.start);
                let (middle, rest) = split_spans(&rest, mark.text.len());
                let (_, right) = split_spans(&rest, mark.whole.end - mark.text.end);
                let at = mark.whole.start;
                ([left, middle, right].concat(), (at, at + mark.text.len()))
            }
            // `==` either side, looking like the text next to it
            None => {
                let (left, rest) = split_spans(&line.spans, a);
                let (middle, right) = split_spans(&rest, b - a);
                let look = |span: Option<&Span>| {
                    let style = span.map_or(Style::NONE, |s| s.style);
                    style.and(Style(
                        Style::MARKUP.0 | Style::ITALIC_US.0 | Style::BOLD_US.0,
                    ))
                };
                let open = Span::new("==", look(middle.first()));
                let close = Span::new("==", look(middle.last()));
                let spans = [left, vec![open], middle, vec![close], right].concat();
                (spans, (a + 2, b + 2))
            }
        };
        let new = Line {
            spans: markdown::merge(spans),
            ..line
        };
        let cursor = line_start + selected.1 as i32;
        self.rewrite(n, n, vec![new], cursor);
        buffer.select_range(
            &buffer.iter_at_offset(line_start + selected.0 as i32),
            &buffer.iter_at_offset(cursor),
        );
    }

    /// The highlight the selection is (its text, or all of it with its
    /// `==`), if it is one.
    fn selected_highlight(&self) -> Option<markdown::Inline> {
        let (start, end) = self.0.buffer.selection_bounds()?;
        if start.line() != end.line() {
            return None;
        }
        let (a, b) = (start.line_offset() as usize, end.line_offset() as usize);
        markdown::inlines(&self.line_model(start.line() as usize).spans)
            .into_iter()
            .find(|i| {
                i.kind == InlineKind::Highlight
                    && i.whole.end - i.whole.start == i.text.len() + 4
                    && (i.text == (a..b) || i.whole == (a..b))
            })
    }

    pub fn toggle_bullets(&self) {
        self.toggle_lines(
            |k| matches!(k, LineKind::Bullet { task: None, .. }),
            |k, _| LineKind::bullet(k.list_indent().unwrap_or(0)),
        );
    }

    pub fn toggle_numbers(&self) {
        self.toggle_lines(
            |k| matches!(k, LineKind::Numbered { task: None, .. }),
            |k, i| LineKind::numbered(k.list_indent().unwrap_or(0), i as u32 + 1),
        );
    }

    pub fn toggle_tasks(&self) {
        self.toggle_lines(
            |k| k.task().is_some(),
            |k, _| {
                LineKind::bullet(k.list_indent().unwrap_or(0))
                    .with_task(Some(k.task().unwrap_or(Task::Open)))
            },
        );
    }

    pub fn toggle_quote(&self) {
        self.toggle_lines(
            |k| matches!(k, LineKind::Quote { .. }),
            |_, _| LineKind::quote(1),
        );
    }

    /// Makes the selected lines `make(kind, i)`, or text again if they all
    /// are what `is` looks for already.
    fn toggle_lines(
        &self,
        is: impl Fn(LineKind) -> bool,
        make: impl Fn(LineKind, usize) -> LineKind,
    ) {
        if self.plain() {
            return;
        }
        // raw lines (code, tables, quotes kept as written) stay as written:
        // made rich, their Markdown would come alive
        let (first, last) = self.selected_lines();
        let rich: Vec<usize> = (first..=last)
            .filter(|&n| self.0.view.kind(n) != LineKind::Raw)
            .collect();
        if rich.is_empty() {
            return;
        }
        let all = rich.iter().all(|&n| is(self.0.view.kind(n)));
        self.change(first, last, || {
            for (i, &n) in rich.iter().enumerate() {
                let kind = self.0.view.kind(n);
                self.set_kind(n, if all { LineKind::Text } else { make(kind, i) });
            }
        });
    }

    /// Tab / Shift+Tab on list items: in under the item before, or out to
    /// the parent's level. Returns false if no list item is selected.
    fn indent_bullets(&self, delta: i32) -> bool {
        let (first, last) = self.selected_lines();
        if !(first..=last).any(|n| self.0.view.kind(n).is_list()) {
            return false;
        }
        self.change(first, last, || {
            for n in first..=last {
                let kind = self.0.view.kind(n);
                if !kind.is_list() {
                    continue;
                }
                let kinds = self.0.view.imp().kinds.borrow().clone();
                let (inside, out) = markdown::list_moves(&kinds, n);
                let indent = if delta > 0 { inside } else { out };
                if let Some(indent) = indent {
                    self.set_kind(n, kind.with_indent(indent));
                }
            }
        });
        true
    }

    /// Ticks or unticks the task on `line`.
    fn toggle_task(&self, line: usize) {
        let kind = self.0.view.kind(line);
        let task = match kind.task() {
            Some(Task::Open) => Task::Done(b'x'),
            Some(Task::Done(_)) => Task::Open,
            None => return,
        };
        self.change(line, line, || {
            self.set_kind(line, kind.with_task(Some(task)))
        });
    }

    /// The task box under a point of the view, as its line.
    fn task_at(&self, x: f64, y: f64) -> Option<usize> {
        let view = &self.0.view;
        let (bx, by) =
            view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        let (iter, _) = view.line_at_y(by);
        let line = iter.line() as usize;
        self.0.view.kind(line).task()?;
        let level = (view.shape(line).level as usize).min(LEVELS - 1);
        let (left, size) = view.task_box(level);
        let rect = view.cursor_at(&iter);
        let cy = rect.y() as f32 + rect.height() as f32 / 2.0;
        let slack = 3.0 * view.zoom();
        let inside = (left - slack..=left + size + slack).contains(&(bx as f32))
            && (cy - size / 2.0 - slack..=cy + size / 2.0 + slack).contains(&(by as f32));
        inside.then_some(line)
    }

    // -- code blocks and tables -----------------------------------------------------

    /// The table line `line` is in, as its first and last line.
    fn table_around(&self, line: usize) -> Option<(usize, usize)> {
        self.0
            .view
            .block(line)
            .is_table()
            .then(|| self.block_around(line))
            .flatten()
    }

    /// Whether the table line `line` is in has every row's pipes in the same
    /// places, at both ends: its column lines can be drawn straight. Wide
    /// characters (emoji, CJK) are wider than a column of JetBrains Mono, so
    /// a table with them keeps its pipes.
    fn table_lined_up(&self, line: usize) -> bool {
        let Some((first, last)) = self.table_around(line) else {
            return false;
        };
        let text = self
            .0
            .buffer
            .text(&self.line_start(first), &self.line_content_end(last), true);
        if text.chars().any(|c| c >= '\u{2070}') {
            return false;
        }
        let rows: Vec<&str> = text.lines().collect();
        let head = markdown::table_pipes(rows[0]);
        head.first() == Some(&0)
            && rows.iter().all(|row| {
                let pipes = markdown::table_pipes(row);
                pipes == head && pipes.last() == Some(&(row.chars().count() - 1))
            })
    }

    /// The code block or table line `line` is in, as its first and last line.
    fn block_around(&self, line: usize) -> Option<(usize, usize)> {
        let blocks = self.0.view.imp().blocks.borrow();
        if *blocks.get(line)? == Block::None {
            return None;
        }
        let mut first = line;
        while first > 0 && continues(blocks[first - 1], blocks[first]) {
            first -= 1;
        }
        let mut last = line;
        while last + 1 < blocks.len() && continues(blocks[last], blocks[last + 1]) {
            last += 1;
        }
        Some((first, last))
    }

    fn line_texts(&self, first: usize, last: usize) -> Vec<String> {
        (first..=last).map(|n| self.line_model(n).text()).collect()
    }

    /// New table rows as raw lines in place of `before`: each keeps its line
    /// ending, a new row gets the note's.
    fn table_lines(&self, rows: &[String], before: &[Line]) -> Vec<Line> {
        let newline = self.newline();
        let last = before.last().map_or(Eol::None, |l| l.eol);
        rows.iter()
            .enumerate()
            .map(|(i, row)| {
                let eol = if i + 1 == rows.len() {
                    last
                } else if i + 1 < before.len() {
                    before[i].eol
                } else {
                    newline
                };
                markdown::raw_line(row, eol)
            })
            .collect()
    }

    /// Where row `row` of `lines`, written from `first`, starts.
    fn row_offset(&self, first: usize, lines: &[Line], row: usize) -> i32 {
        let before: usize = lines
            .iter()
            .take(row)
            .map(|l| l.text().chars().count() + l.eol.as_str().len().max(1))
            .sum();
        self.line_start(first).offset() + before as i32
    }

    /// Whether `rows`, a table written from line `first`, fit the window
    /// without wrapping. A table lined up wider than that would wrap in
    /// the middle of its padding, so it is left as typed. Its characters
    /// are measured on its first line as drawn (whole pixels each would be
    /// off by a column or two on a wide table).
    fn table_fits(&self, first: usize, rows: &[String]) -> bool {
        let view = &self.0.view;
        let Some(start) = self.0.buffer.iter_at_line(first as i32) else {
            return true;
        };
        let mut row_end = start;
        if !row_end.ends_line() {
            view.forward_display_line_end(&mut row_end);
        }
        let chars = row_end.offset() - start.offset();
        let left = view.cursor_at(&start).x();
        let room = view.visible_rect().width() - left;
        if chars <= 0 || room <= 0 {
            return true; // empty, or not laid out yet
        }
        let char_width = (view.cursor_at(&row_end).x() - left) as f64 / chars as f64;
        // the table line's right margin
        let room = room as f64 - 4.0 * view.zoom() as f64;
        let widest = rows.iter().map(|r| r.chars().count()).max().unwrap_or(0);
        widest as f64 * char_width <= room
    }

    /// `markdown::format_table`, unless the table lined up would not fit
    /// the window (see `table_fits`): then the rows stay as they are, and
    /// a new row (`|`) gets one empty cell per column.
    fn format_fitting(
        &self,
        first: usize,
        rows: &[String],
        at: Option<(usize, usize)>,
    ) -> (Vec<String>, Option<usize>) {
        let (formatted, offset) = markdown::format_table(rows, at);
        if self.table_fits(first, &formatted) {
            return (formatted, offset);
        }
        let cells = rows
            .first()
            .map_or(1, |r| markdown::table_cells(r).len().max(1));
        let rows = rows
            .iter()
            .map(|r| {
                if r == "|" {
                    format!("|{}", "  |".repeat(cells))
                } else {
                    r.clone()
                }
            })
            .collect();
        (rows, at.map(|(_, column)| column))
    }

    /// Lines the table up again soon after an edit in it.
    fn schedule_align(&self) {
        if std::mem::replace(&mut self.0.state.borrow_mut().align_pending, true) {
            return;
        }
        let weak = self.downgrade();
        // before the next frame, so the table never shows out of line
        glib::idle_add_local_full(glib::Priority::HIGH_IDLE, move || {
            if let Some(editor) = upgrade(&weak) {
                editor.0.state.borrow_mut().align_pending = false;
                editor.align_table();
            }
            glib::ControlFlow::Break
        });
    }

    /// Lines up the table at the cursor, as part of the edit that put it
    /// out of line (one undo takes back both).
    fn align_table(&self) {
        let Some((line, cursor)) = self.cursor_line() else {
            return;
        };
        let Some((first, last)) = self.table_around(line) else {
            return;
        };
        if !self.0.state.borrow().redo.is_empty() {
            // an undo came first: leave what it brought back alone
            return;
        }
        let rows = self.line_texts(first, last);
        let at = Some((line - first, cursor.line_offset() as usize));
        let (formatted, offset) = self.format_fitting(first, &rows, at);
        if formatted == rows {
            return;
        }
        let before = self.models(first, last);
        let lines = self.table_lines(&formatted, &before);
        self.replace_lines(first, before.len(), &lines);
        let cursor_after = self.line_start(line).offset() + offset.unwrap_or(0) as i32;
        self.0
            .buffer
            .place_cursor(&self.0.buffer.iter_at_offset(cursor_after));
        {
            let mut state = self.0.state.borrow_mut();
            match state.undo.last_mut() {
                Some(step)
                    if step.before.len() == step.after.len()
                        && step.first >= first
                        && step.first + step.after.len() <= last + 1 =>
                {
                    let mut full = before;
                    let at = step.first - first;
                    full.splice(at..at + step.before.len(), step.before.drain(..));
                    step.first = first;
                    step.before = full;
                    step.after = lines;
                    step.cursor_after = cursor_after;
                }
                _ => state.undo.push(Step {
                    first,
                    before,
                    after: lines,
                    cursor_before: cursor_after,
                    cursor_after,
                    typing: false,
                    typed: None,
                    at: Instant::now(),
                }),
            }
        }
        self.changed();
    }

    /// Tab in a table: lines it up and moves to the end of the next cell
    /// (a new row after the last one); Shift+Tab goes back.
    fn table_tab(&self, back: bool) -> bool {
        let Some((line, cursor)) = self.cursor_line() else {
            return false;
        };
        let Some((first, last)) = self.table_around(line) else {
            return false;
        };
        let rows = self.line_texts(first, last);
        let at = Some((line - first, cursor.line_offset() as usize));
        let (mut formatted, offset) = self.format_fitting(first, &rows, at);
        let step = markdown::table_step(&formatted, line - first, offset.unwrap_or(0), back);
        let (row, column) = match step {
            Some(step) => step,
            None if back => (line - first, offset.unwrap_or(0)),
            None => {
                formatted.push("|".into());
                formatted = self.format_fitting(first, &formatted, None).0;
                (formatted.len() - 1, 2)
            }
        };
        let before = self.models(first, last);
        let lines = self.table_lines(&formatted, &before);
        let cursor_after = self.row_offset(first, &lines, row) + column as i32;
        if formatted == rows {
            self.place_cursor(cursor_after);
        } else {
            self.rewrite(first, last, lines, cursor_after);
        }
        true
    }

    /// Enter in a table adds a row under the cursor's; on an empty last
    /// row it leaves the table, like Enter on an empty bullet.
    fn table_enter(&self) -> bool {
        let Some((line, _)) = self.cursor_line() else {
            return false;
        };
        let Some((first, last)) = self.table_around(line) else {
            return false;
        };
        let mut rows = self.line_texts(first, last);
        let row = line - first;
        let empty = |text: &str| text.chars().all(|c| c == '|' || c.is_whitespace());
        if line == last && row >= 2 && empty(&rows[row]) {
            let eol = self.line_eol(line);
            let text = Line {
                kind: LineKind::Text,
                spans: Vec::new(),
                eol,
            };
            self.rewrite(line, line, vec![text], self.line_start(line).offset());
            return true;
        }
        // under the head comes the rule first
        let new = (row + 1).max(2);
        rows.insert(new, "|".into());
        let (formatted, _) = self.format_fitting(first, &rows, None);
        let before = self.models(first, last);
        let lines = self.table_lines(&formatted, &before);
        let cursor_after = self.row_offset(first, &lines, new) + 2;
        self.rewrite(first, last, lines, cursor_after);
        true
    }

    /// Enter after typing a table's `|---|---|` line under its head line
    /// makes it a table, lined up, with an empty row to fill in.
    fn start_table(&self) -> bool {
        let Some((line, cursor)) = self.cursor_line() else {
            return false;
        };
        if line == 0 || !cursor.ends_line() || self.plain() {
            return false;
        }
        let loose = |n: usize| {
            matches!(self.0.view.kind(n), LineKind::Text | LineKind::Raw)
                && self.0.view.block(n) == Block::None
        };
        if !loose(line) || !loose(line - 1) {
            return false;
        }
        let rows = self.line_texts(line - 1, line);
        if !markdown::table_rule(&rows[1])
            || !markdown::table_row(&rows[0])
            || markdown::table_cells(&rows[0]).len() != markdown::table_cells(&rows[1]).len()
        {
            return false;
        }
        let rows = [rows[0].clone(), rows[1].clone(), "|".into()];
        let (formatted, _) = markdown::format_table(&rows, None);
        let before = self.models(line - 1, line);
        let lines = self.table_lines(&formatted, &before);
        let cursor_after = self.row_offset(line - 1, &lines, 2) + 2;
        self.rewrite(line - 1, line, lines, cursor_after);
        true
    }

    /// Enter after typing ``` (or ```rust) on a line of its own makes a code
    /// block, closed already, with the cursor inside.
    fn start_code_block(&self) -> bool {
        let Some((line, cursor)) = self.cursor_line() else {
            return false;
        };
        if self.plain() || self.0.view.kind(line) != LineKind::Text || !cursor.ends_line() {
            return false;
        }
        let text = self.line_model(line).text();
        let Some(close) = markdown::closing_fence(&text) else {
            return false;
        };
        let newline = self.newline();
        let lines = vec![
            markdown::raw_line(&text, newline),
            markdown::raw_line("", newline),
            markdown::raw_line(&close, self.line_eol(line)),
        ];
        let cursor_after = self.row_offset(line, &lines, 1);
        self.rewrite(line, line, lines, cursor_after);
        true
    }

    /// Enter after typing `$$` on a line of its own makes a block of
    /// display math, closed already, with the cursor inside.
    fn start_math(&self) -> bool {
        let Some((line, cursor)) = self.cursor_line() else {
            return false;
        };
        if self.plain() || self.0.view.kind(line) != LineKind::Text || !cursor.ends_line() {
            return false;
        }
        if self.line_model(line).text() != "$$" {
            return false;
        }
        let newline = self.newline();
        let lines = vec![
            markdown::raw_line("$$", newline),
            markdown::raw_line("", newline),
            markdown::raw_line("$$", self.line_eol(line)),
        ];
        let cursor_after = self.row_offset(line, &lines, 1);
        self.rewrite(line, line, lines, cursor_after);
        true
    }

    /// Enter after typing `---` (or `***`, `___`) on a line of its own
    /// draws a line across, and goes on below it. Right under text, `---`
    /// would make that text a heading: a blank line goes between.
    fn start_rule(&self) -> bool {
        let Some((line, cursor)) = self.cursor_line() else {
            return false;
        };
        if self.plain() || self.0.view.kind(line) != LineKind::Text || !cursor.ends_line() {
            return false;
        }
        let text = self.line_model(line).text();
        if !["---", "***", "___"].contains(&text.as_str()) {
            return false;
        }
        let newline = self.newline();
        let under_text = text == "---"
            && line > 0
            && matches!(self.0.view.kind(line - 1), LineKind::Text | LineKind::Raw)
            && self.0.view.block(line - 1) == Block::None
            && !self.line_model(line - 1).text().trim().is_empty();
        let mut lines = Vec::new();
        if under_text {
            lines.push(Line {
                kind: LineKind::Text,
                spans: Vec::new(),
                eol: newline,
            });
        }
        lines.push(markdown::raw_line(&text, newline));
        lines.push(Line {
            kind: LineKind::Text,
            spans: Vec::new(),
            eol: self.line_eol(line),
        });
        let cursor_after = self.row_offset(line, &lines, lines.len() - 1);
        self.rewrite(line, line, lines, cursor_after);
        true
    }

    /// Enter at the end of a code block's (or display math's) closing line
    /// goes back to text.
    fn leave_code_block(&self) -> bool {
        let Some((line, cursor)) = self.cursor_line() else {
            return false;
        };
        let block = self.0.view.block(line);
        if !matches!(block, Block::FenceClose | Block::MathClose) || !cursor.ends_line() {
            return false;
        }
        let fence = self.line_model(line);
        let text = Line {
            kind: LineKind::Text,
            spans: Vec::new(),
            eol: fence.eol,
        };
        let lines = vec![
            Line {
                eol: self.newline(),
                ..fence
            },
            text,
        ];
        let cursor_after = self.row_offset(line, &lines, 1);
        self.rewrite(line, line, lines, cursor_after);
        true
    }

    /// Enter in a code block keeps the line's indentation.
    fn code_enter(&self) -> bool {
        let Some((line, cursor)) = self.cursor_line() else {
            return false;
        };
        if self.0.view.block(line) != Block::Code {
            return false;
        }
        let indent: String = self
            .line_model(line)
            .text()
            .chars()
            .take(cursor.line_offset() as usize)
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        if indent.is_empty() {
            return false;
        }
        let text = format!("{}{indent}", self.newline().as_str());
        self.0.buffer.insert_interactive_at_cursor(&text, true);
        self.scroll_to_cursor();
        true
    }

    // -- word count -------------------------------------------------------------------

    fn connect_word_count(&self) {
        let wc = &self.0.word_count;
        let weak = self.downgrade();
        wc.switch.connect_active_notify(move |switch| {
            let Some(editor) = upgrade(&weak) else { return };
            let on = switch.is_active();
            let counter = &editor.0.word_count.counter;
            if counter.is_visible() == on {
                return; // set from the config, not switched
            }
            counter.set_visible(on);
            if on {
                editor.recount();
            }
            if let Some(f) = &editor.0.callbacks.borrow().word_count {
                f(on);
            }
        });
        let weak = self.downgrade();
        wc.counter.connect_clicked(move |_| {
            if let Some(editor) = upgrade(&weak) {
                editor.toggle_word_count_menu();
            }
        });
        // counts follow the text once the typing pauses, while on show
        let weak = self.downgrade();
        self.0.buffer.connect_changed(move |_| {
            let Some(editor) = upgrade(&weak) else { return };
            let wc = &editor.0.word_count;
            if !wc.counter.is_visible() && !wc.menu.is_visible() {
                return;
            }
            if let Some(id) = editor.0.recount_timer.borrow_mut().take() {
                id.remove();
            }
            let weak = editor.downgrade();
            let id = glib::timeout_add_local_once(Duration::from_millis(250), move || {
                if let Some(editor) = upgrade(&weak) {
                    editor.0.recount_timer.borrow_mut().take();
                    editor.recount();
                }
            });
            editor.0.recount_timer.replace(Some(id));
        });
        // the window changed size: the menu stays above the counter
        for adjustment in [self.0.view.vadjustment(), self.0.view.hadjustment()]
            .into_iter()
            .flatten()
        {
            let weak = self.downgrade();
            adjustment.connect_notify_local(Some("page-size"), move |_, _| {
                if let Some(editor) = upgrade(&weak)
                    && editor.0.word_count.menu.is_visible()
                {
                    editor.place_word_count_menu();
                    editor.0.word_count.menu.present();
                }
            });
        }
        // a click in the text closes the menu, as elsewhere in Kludd
        let click = gtk::GestureClick::new();
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = self.downgrade();
        click.connect_pressed(move |_, _, _, _| {
            if let Some(editor) = upgrade(&weak) {
                editor.0.word_count.menu.popdown();
            }
        });
        self.0.view.add_controller(click);
    }

    /// Counts the note's words and characters again.
    fn recount(&self) {
        let text = self.text();
        let markdown = !self.plain();
        self.0.word_count.show(stats::count(&text, markdown));
    }

    /// Ctrl+Shift+C: opens the word count menu, or closes it.
    pub fn toggle_word_count_menu(&self) {
        let menu = &self.0.word_count.menu;
        if menu.is_visible() {
            menu.popdown();
            return;
        }
        self.hide_toolbar();
        self.recount();
        self.place_word_count_menu();
        menu.popup();
    }

    /// The word count menu goes in the bottom left corner: just above the
    /// "x words" counter when it shows, so none of the counter peeks out
    /// from under the menu.
    fn place_word_count_menu(&self) {
        let wc = &self.0.word_count;
        let corner = (24.0, f64::from(self.0.root.height() - 12));
        let (x, y) = if wc.counter.is_visible() {
            wc.counter
                .compute_bounds(&self.0.root)
                .map(|b| (f64::from(b.x()), f64::from(b.y()) - 6.0))
                .unwrap_or(corner)
        } else {
            corner
        };
        wc.menu
            .set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        wc.menu.set_halign(gtk::Align::Start);
    }

    /// Closes the word count menu; false if it was not open.
    pub fn close_word_count_menu(&self) -> bool {
        let menu = &self.0.word_count.menu;
        let open = menu.is_visible();
        menu.popdown();
        open
    }

    /// Whether the "x words" counter shows while typing (from the config).
    pub fn set_word_count_shown(&self, on: bool) {
        let wc = &self.0.word_count;
        wc.counter.set_visible(on);
        wc.switch.set_active(on);
        if on {
            self.recount();
        }
    }

    /// The counter was switched on or off in the menu.
    pub fn connect_word_count_shown(&self, f: impl Fn(bool) + 'static) {
        self.0.callbacks.borrow_mut().word_count = Some(Box::new(f));
    }

    // -- find in the note -------------------------------------------------------------

    fn connect_find(&self) {
        let bar = &self.0.find;
        let buffer = &self.0.buffer;
        buffer.add_mark(&self.0.find_from, &buffer.start_iter());
        let weak = self.downgrade();
        bar.text.connect_changed(move |_| {
            if let Some(editor) = upgrade(&weak) {
                editor.refind(true);
            }
        });
        let weak = self.downgrade();
        bar.connect_step(move |step| {
            if let Some(editor) = upgrade(&weak) {
                editor.find_step(step);
            }
        });
        let weak = self.downgrade();
        bar.close.connect_clicked(move |_| {
            if let Some(editor) = upgrade(&weak) {
                editor.close_find();
            }
        });
        // the text changed under the bar: the matches move with it
        let weak = self.downgrade();
        let pending = Rc::new(Cell::new(false));
        self.0.buffer.connect_changed(move |_| {
            let Some(editor) = upgrade(&weak) else { return };
            if !editor.0.find.widget.is_visible() || pending.replace(true) {
                return;
            }
            let weak = editor.downgrade();
            let pending = pending.clone();
            glib::idle_add_local_once(move || {
                pending.set(false);
                if let Some(editor) = upgrade(&weak) {
                    editor.refind(false);
                }
            });
        });
    }

    /// Ctrl+F: shows the find bar (with the selection in it, if it is a
    /// few words on one line) and puts the keys there.
    pub fn open_find(&self) {
        let bar = &self.0.find;
        let buffer = &self.0.buffer;
        let selected = buffer
            .selection_bounds()
            .map(|(a, b)| buffer.text(&a, &b, true).to_string())
            .filter(|t| !t.contains('\n') && t.chars().count() <= 100);
        let from = buffer
            .selection_bounds()
            .map_or_else(|| buffer.iter_at_mark(&buffer.get_insert()), |(a, _)| a);
        self.0.buffer.move_mark(&self.0.find_from, &from);
        if !bar.widget.is_visible() {
            bar.widget.set_visible(true);
        }
        self.hide_toolbar();
        match selected {
            Some(text) if bar.text.text() != text => bar.text.set_text(&text),
            // the last search again: its matches show, but the cursor
            // stays where it is until Enter goes to the next one from it
            _ => {
                self.refind(false);
                self.0.view.imp().find_current.set(None);
                let total = self.0.view.imp().finds.borrow().len();
                bar.show_count(None, total);
                self.0.view.queue_draw();
            }
        }
        bar.text.grab_focus();
        bar.text.select_region(0, -1);
    }

    /// Closes the find bar and goes back to the text, at the match last
    /// gone to. Returns false if it was not open.
    pub fn close_find(&self) -> bool {
        if !self.hide_find() {
            return false;
        }
        self.0.view.grab_focus();
        true
    }

    /// Hides the find bar and its matches; false if it was not open.
    fn hide_find(&self) -> bool {
        let bar = &self.0.find;
        if !bar.widget.is_visible() {
            return false;
        }
        bar.widget.set_visible(false);
        let view = self.0.view.imp();
        view.finds.borrow_mut().clear();
        view.find_current.set(None);
        self.0.view.queue_draw();
        true
    }

    /// Finds the bar's text in the note again. With `jump` (the text in the
    /// bar changed), goes to the first match from where finding started.
    fn refind(&self, jump: bool) {
        let bar = &self.0.find;
        let buffer = &self.0.buffer;
        let query = bar.text.text();
        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
        let finds = self.find_in(&text, &query);
        let from = buffer.iter_at_mark(&self.0.find_from).offset();
        let current = (!finds.is_empty()).then(|| {
            let after = finds.partition_point(|&(a, _)| a < from);
            if after < finds.len() { after } else { 0 }
        });
        bar.show_count(current, finds.len());
        let target = current.map(|c| finds[c].0);
        let view = self.0.view.imp();
        view.finds.replace(finds);
        view.find_current.set(current);
        if jump && let Some(offset) = target {
            // what edits before it move along, as after a step
            buffer.move_mark(&self.0.find_from, &buffer.iter_at_offset(offset));
            self.go_to_find(offset);
        }
        self.0.view.queue_draw();
    }

    /// Where `query` is in the note: in its text as shown (`[the docs]
    /// (url)` reads "the docs", so "see the docs" is found across the
    /// hidden `[`), and in all of it, hidden syntax too (a link's
    /// address). In order, never overlapping.
    fn find_in(&self, text: &str, query: &str) -> Vec<(i32, i32)> {
        let mut finds: Vec<(i32, i32)> = find::find_all(text, query)
            .into_iter()
            .map(|(a, b)| (a as i32, b as i32))
            .collect();
        // the text as read: without its syntax, hidden or (where the
        // cursor is, going to a match puts it there) shown faintly, nor
        // the room kept around a code span
        let tags = &self.0.tags;
        let buffer = &self.0.buffer;
        let mut hidden = Vec::new();
        for tag in [&tags.conceal, &tags.syntax, &tags.pad] {
            let mut iter = buffer.start_iter();
            if !iter.has_tag(tag) && !iter.forward_to_tag_toggle(Some(tag)) {
                continue;
            }
            while !iter.is_end() {
                let start = iter.offset();
                iter.forward_to_tag_toggle(Some(tag));
                hidden.push((start, iter.offset()));
                if !iter.forward_to_tag_toggle(Some(tag)) {
                    break;
                }
            }
        }
        hidden.sort_unstable();
        if !hidden.is_empty() {
            let mut shown = String::with_capacity(text.len());
            let mut offsets: Vec<i32> = Vec::with_capacity(text.len());
            let mut gaps = hidden.iter().peekable();
            for (i, c) in text.chars().enumerate() {
                let i = i as i32;
                while gaps.peek().is_some_and(|&&(_, end)| end <= i) {
                    gaps.next();
                }
                if gaps.peek().is_some_and(|&&(start, _)| start <= i) {
                    continue;
                }
                shown.push(c);
                offsets.push(i);
            }
            for (a, b) in find::find_all(&shown, query) {
                finds.push((offsets[a], offsets[b - 1] + 1));
            }
            finds.sort_unstable();
            let mut kept: Vec<(i32, i32)> = Vec::with_capacity(finds.len());
            for found in finds {
                if kept.last().is_none_or(|&(_, end)| found.0 >= end) {
                    kept.push(found);
                }
            }
            finds = kept;
        }
        finds
    }

    /// Next (`step` 1) or previous (-1) match, round at the ends.
    fn find_step(&self, step: i32) {
        let view = self.0.view.imp();
        let total = view.finds.borrow().len();
        if total == 0 {
            return;
        }
        // none gone to yet: the first after the cursor (or before it)
        let from = {
            let buffer = &self.0.buffer;
            buffer.iter_at_mark(&self.0.find_from).offset()
        };
        let current = match view.find_current.get() {
            Some(c) => (c as i32 + step).rem_euclid(total as i32) as usize,
            None => {
                let after = view.finds.borrow().partition_point(|&(a, _)| a < from);
                if step > 0 {
                    after % total
                } else {
                    (after + total - 1) % total
                }
            }
        };
        view.find_current.set(Some(current));
        let offset = view.finds.borrow()[current].0;
        let buffer = &self.0.buffer;
        buffer.move_mark(&self.0.find_from, &buffer.iter_at_offset(offset));
        self.0.find.show_count(Some(current), total);
        self.go_to_find(offset);
        self.0.view.queue_draw();
    }

    /// Puts the cursor on a match (its hidden syntax shows) and scrolls it
    /// into view, clear of the edges.
    fn go_to_find(&self, offset: i32) {
        let buffer = &self.0.buffer;
        buffer.place_cursor(&buffer.iter_at_offset(offset));
        let mark = buffer.get_insert();
        self.0.view.scroll_to_mark(&mark, 0.2, false, 0.0, 0.0);
    }

    // -- links ---------------------------------------------------------------------

    /// A click opens a link (on release, so a press dragged away opens
    /// nothing; a double-click opens it once); the pointer shows where. A
    /// link being edited (the cursor is in it, its syntax shown) takes
    /// clicks like text, unless Ctrl is held. A click on a task's box
    /// ticks it.
    fn connect_links(&self) {
        let click = gtk::GestureClick::builder()
            .button(gdk::BUTTON_PRIMARY)
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        // the link pressed on, and whether the last click opened one
        let pressed: Rc<RefCell<Option<String>>> = Rc::default();
        let opened = Rc::new(Cell::new(false));
        // where a press on a link was, and once it is dragged (no click,
        // then), where the selection it drags out starts; the drag below
        let press_at: Rc<Cell<Option<i32>>> = Rc::default();
        let drag_from: Rc<Cell<Option<i32>>> = Rc::default();
        let weak = self.downgrade();
        let (pending, just_opened) = (pressed.clone(), opened.clone());
        let (pressed_at, dragged) = (press_at.clone(), drag_from.clone());
        click.connect_pressed(move |gesture, presses, x, y| {
            let Some(editor) = upgrade(&weak) else { return };
            pending.replace(None);
            dragged.set(None);
            // the second click of a double-click on a link: it is open
            // already, and what it opened must not get a click
            if presses > 1 && just_opened.get() {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                return;
            }
            just_opened.set(false);
            let state = gesture.current_event_state();
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            if let Some(url) = editor
                .link_at(x, y, ctrl)
                .filter(|_| !shift && presses == 1)
            {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                pending.replace(Some(url));
                pressed_at.set(editor.offset_at(x, y));
            } else if let Some(line) = editor.task_at(x, y).filter(|_| !editor.plain()) {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                editor.toggle_task(line);
            } else if !shift && presses == 1 {
                editor.fix_click_past_end(x, y);
            }
        });
        // opens the link pressed on, unless the press became a drag
        let open = {
            let weak = self.downgrade();
            let pending = pressed.clone();
            Rc::new(move || {
                if let Some(editor) = upgrade(&weak)
                    && let Some(url) = pending.take()
                {
                    opened.set(true);
                    editor.open_link(&url);
                }
            })
        };
        let open_now = open.clone();
        click.connect_released(move |_, _, _, _| open_now());
        // a press held past the double-click time stops the click gesture
        // (no release comes then): the drag gesture, which does not time
        // out, opens it on release instead
        // a press on a link that is dragged selects, as anywhere else in
        // the text (in the click's group, it sees the press it claimed)
        let drag = gtk::GestureDrag::builder()
            .button(gdk::BUTTON_PRIMARY)
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        drag.group_with(&click);
        let weak = self.downgrade();
        let dragging = drag_from.clone();
        let pending = pressed.clone();
        drag.connect_drag_update(move |drag, dx, dy| {
            let Some(editor) = upgrade(&weak) else { return };
            if dragging.get().is_none() {
                let far = dx * dx + dy * dy > 6.0 * 6.0;
                if !far || pending.borrow().is_none() {
                    return;
                }
                pending.replace(None);
                dragging.set(press_at.take());
            }
            if let (Some(from), Some((x, y))) = (dragging.get(), drag.start_point()) {
                editor.drag_select(from, x + dx, y + dy);
            }
        });
        drag.connect_drag_end(move |_, _, _| {
            drag_from.set(None);
            open();
        });
        // taken by another gesture: open nothing
        drag.connect_cancel(move |_, _| {
            pressed.replace(None);
        });
        self.0.view.add_controller(click);
        self.0.view.add_controller(drag);

        // the hand over what a click opens; it changes when the pointer
        // moves, and when the cursor does (into a link, say)
        let pointer: Rc<Cell<Option<(f64, f64, bool)>>> = Rc::default();
        let on_link = Rc::new(Cell::new(false));
        let update = {
            let weak = self.downgrade();
            let pointer = pointer.clone();
            move || {
                let Some(editor) = upgrade(&weak) else { return };
                let over = pointer.get().is_some_and(|(x, y, ctrl)| {
                    editor.link_at(x, y, ctrl).is_some() || editor.task_at(x, y).is_some()
                });
                if on_link.replace(over) != over {
                    let cursor = if over { "pointer" } else { "text" };
                    editor.0.view.set_cursor_from_name(Some(cursor));
                }
            }
        };
        // run once GTK has laid the text out again (asked during a scroll,
        // before that, its own lookups fail)
        let update: Rc<dyn Fn()> = {
            let update = Rc::new(update);
            let queued = Rc::new(Cell::new(false));
            Rc::new(move || {
                if queued.replace(true) {
                    return;
                }
                let (update, queued) = (update.clone(), queued.clone());
                glib::idle_add_local_full(glib::Priority::LOW, move || {
                    queued.set(false);
                    update();
                    glib::ControlFlow::Break
                });
            })
        };
        let motion = gtk::EventControllerMotion::new();
        let (at, changed) = (pointer.clone(), update.clone());
        motion.connect_motion(move |motion, x, y| {
            let state = motion.current_event_state();
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            at.set(Some((x, y, ctrl)));
            changed();
        });
        let changed = update.clone();
        motion.connect_leave(move |_| {
            pointer.set(None);
            changed();
        });
        self.0.view.add_controller(motion);
        // the text moved under a still pointer
        if let Some(adjustment) = self.0.view.vadjustment() {
            let update = update.clone();
            adjustment.connect_value_changed(move |_| update());
        }
        // once the cursor has settled (typing moves it on every key)
        let weak = self.downgrade();
        let queued = Rc::new(Cell::new(false));
        self.0.buffer.connect_mark_set(move |_, _, mark| {
            let Some(editor) = upgrade(&weak) else { return };
            if *mark != editor.0.buffer.get_insert() || queued.replace(true) {
                return;
            }
            let (update, queued) = (update.clone(), queued.clone());
            glib::idle_add_local_once(move || {
                queued.set(false);
                update();
            });
        });
    }

    /// A click right of a line's end puts the cursor there. GTK puts it
    /// inside hidden syntax instead when the line ends in a link, and the
    /// next letter typed would break the link.
    fn fix_click_past_end(&self, x: f64, y: f64) {
        let view = &self.0.view;
        let (bx, by) =
            view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        let Some(end) = view.past_line_end(bx, by) else {
            return;
        };
        let offset = end.offset();
        // once GTK has placed it, unless the press became a selection
        let weak = self.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(editor) = upgrade(&weak)
                && !editor.0.buffer.has_selection()
            {
                let buffer = &editor.0.buffer;
                buffer.place_cursor(&buffer.iter_at_offset(offset));
            }
        });
    }

    /// The buffer offset of the character at a point of the view, if any.
    fn offset_at(&self, x: f64, y: f64) -> Option<i32> {
        let view = &self.0.view;
        let (bx, by) =
            view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        let tags = &self.0.tags;
        let iter = view
            .char_at(bx, by, &[&tags.link, &tags.footnote, &tags.table_line])
            .or_else(|| view.char_at(bx, by, &[]))?;
        Some(iter.offset())
    }

    /// Selects from `from` to a point of the view, for a drag that started
    /// on a link (GTK's own selecting never saw its press).
    fn drag_select(&self, from: i32, x: f64, y: f64) {
        let view = &self.0.view;
        let (bx, by) =
            view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        let to = view.char_at(bx, by, &[]).unwrap_or_else(|| {
            // beside the text: the start or the end of the line there
            let (mut line, _) = view.line_at_y(by);
            if bx > view.cursor_at(&line).x() && !line.ends_line() {
                line.forward_to_line_end();
            }
            line
        });
        let buffer = &self.0.buffer;
        buffer.select_range(&to, &buffer.iter_at_offset(from));
        // dragged past the edge: the text follows
        view.scroll_mark_onscreen(&buffer.get_insert());
    }

    /// The link (or picture, or footnote) a click at a point of the view
    /// opens: none on a link being edited (a cursor in or right next to
    /// it, so its syntax shows), unless `ctrl`.
    fn link_at(&self, x: f64, y: f64, ctrl: bool) -> Option<String> {
        let view = &self.0.view;
        let (bx, by) =
            view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        let tags = &self.0.tags;
        let iter = view.char_at(bx, by, &[&tags.link, &tags.footnote, &tags.table_line])?;
        self.link_to_open(&iter, ctrl)
    }

    /// The link (or picture, or footnote) a click on `iter` opens, as in
    /// `link_at`.
    fn link_to_open(&self, iter: &gtk::TextIter, ctrl: bool) -> Option<String> {
        let tags = &self.0.tags;
        let n = iter.line() as usize;
        // a table shows its Markdown, links too: they have no link tag
        let table = self.0.view.block(n).is_table();
        // the shown Markdown of a link being edited counts too, for Ctrl
        let syntax = ctrl && iter.has_tag(&tags.syntax);
        if !table && !syntax && !iter.has_tag(&tags.link) && !iter.has_tag(&tags.footnote) {
            return None;
        }
        let offset = iter.line_offset() as usize;
        let item = self
            .line_inlines(&self.line_model(n))
            .into_iter()
            .find(|i| {
                matches!(
                    i.kind,
                    InlineKind::Link | InlineKind::Url | InlineKind::Image | InlineKind::Footnote
                ) && i.whole.contains(&offset)
            })?;
        if !ctrl {
            // a table shows its Markdown all the time: it is always edited
            let buffer = &self.0.buffer;
            let editing = table
                || [buffer.get_insert(), buffer.selection_bound()]
                    .iter()
                    .map(|mark| buffer.iter_at_mark(mark))
                    .filter(|c| c.line() as usize == n)
                    .any(|c| {
                        let c = c.line_offset() as usize;
                        // a bare URL shows no Markdown: only a cursor inside
                        // it edits it, not one right after it
                        if item.kind == InlineKind::Url {
                            item.whole.start < c && c < item.whole.end
                        } else {
                            (item.whole.start..=item.whole.end).contains(&c)
                        }
                    });
            if editing {
                return None;
            }
        }
        let line = self.line_model(n).text();
        Some(match item.kind {
            InlineKind::Footnote => {
                let label: String = line
                    .chars()
                    .skip(item.text.start)
                    .take(item.text.len())
                    .collect();
                format!("[^{label}]")
            }
            _ => item.url,
        })
    }

    /// Opens a web (or mail) link in its app; a link to a note next to this
    /// one opens it in Kludd, other files in their app. A footnote goes to
    /// its text.
    fn open_link(&self, url: &str) {
        if let Some(label) = url.strip_prefix("[^").and_then(|l| l.strip_suffix(']')) {
            self.go_to_footnote(label);
            return;
        }
        if let Some(anchor) = url.strip_prefix('#') {
            self.go_to_heading(anchor);
            return;
        }
        let window = self.0.view.root().and_downcast::<gtk::Window>();
        let launch = |uri: &str| {
            gtk::UriLauncher::new(uri).launch(window.as_ref(), gio::Cancellable::NONE, |_| {});
        };
        if has_scheme(url) {
            launch(url);
            return;
        }
        let Some(path) = self.linked_path(url) else {
            return;
        };
        if !path.exists() {
            self.emit_error(format!("link not found: {}", config::tilde_path(&path)));
            return;
        }
        let note = path.is_file()
            && (storage::is_markdown(&path)
                || path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("txt")));
        if note {
            // after this click is done with: opening swaps the whole buffer
            let weak = self.downgrade();
            let anchor = url.split_once('#').map(|(_, a)| a.to_string());
            glib::idle_add_local_once(move || {
                let Some(editor) = upgrade(&weak) else { return };
                if let Some(f) = &editor.0.callbacks.borrow().open_file {
                    f(path);
                }
                // `other.md#section`: then to the section
                if let Some(anchor) = anchor.filter(|a| !a.is_empty()) {
                    editor.go_to_heading(&anchor);
                }
            });
        } else {
            launch(&gio::File::for_path(&path).uri());
        }
    }

    /// The file a link without a scheme points to, next to this note:
    /// `#section` is a place in it, `%20` a space.
    fn linked_path(&self, url: &str) -> Option<PathBuf> {
        let path = url.split('#').next().unwrap_or_default();
        if path.is_empty() {
            return None;
        }
        let path = config::expand_tilde(&percent_decode(path));
        let dir = {
            let state = self.0.state.borrow();
            let doc = state.doc.as_ref();
            doc.and_then(|d| {
                d.path
                    .as_deref()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
            })
            .or_else(|| doc.and_then(|d| d.managed.clone()))
            .unwrap_or_else(config::home_dir)
        };
        Some(dir.join(path))
    }

    /// What is drawn in a line, links to the note's reference definitions
    /// included.
    fn line_inlines(&self, line: &Line) -> Vec<markdown::Inline> {
        let mut items = markdown::inlines(&line.spans);
        markdown::reference_links(&line.spans, &self.0.references.borrow(), &mut items);
        items
    }

    /// Moves the cursor to the text of footnote `label` (`[^label]: …`).
    fn go_to_footnote(&self, label: &str) {
        let wanted = format!("[^{label}]:");
        let found = (0..self.line_count())
            .find(|&n| self.line_model(n).text().trim_start().starts_with(&wanted));
        if let Some(n) = found {
            let at = self.line_start(n).offset() + wanted.chars().count() as i32;
            // after this click is done with, or GTK puts the cursor at it
            let weak = self.downgrade();
            glib::idle_add_local_once(move || {
                if let Some(editor) = upgrade(&weak) {
                    editor.place_cursor(at);
                    editor.0.view.grab_focus();
                }
            });
        }
    }

    /// A `#section` link: goes to the heading GitHub gives that anchor
    /// (`## Install it` is `#install-it`; a second one is `-1`).
    fn go_to_heading(&self, anchor: &str) {
        let wanted = percent_decode(anchor).to_lowercase();
        // `#` and `#top` go to the top, as on GitHub
        let top = (wanted.is_empty() || wanted == "top").then_some(0);
        let mut seen: HashMap<String, usize> = HashMap::new();
        let found = top.or_else(|| {
            (0..self.line_count()).find(|&n| {
                let line = self.line_model(n);
                // `# x`, and text underlined with `===` or `---`
                let setext = matches!(self.0.view.block(n), Block::SetextText { .. });
                if line.kind.heading_level().is_none() && !setext {
                    return false;
                }
                // as read: a link in it is its text, `<sub>` and such go;
                // read as a heading, or `1. Start` would be a list item
                let text = if setext {
                    markdown::searchable(&line.text())
                } else {
                    markdown::searchable(&format!("# {}", line.text()))
                };
                let slug = heading_anchor(&text);
                let count = seen.entry(slug.clone()).or_insert(0);
                let id = if *count == 0 {
                    slug
                } else {
                    format!("{slug}-{count}")
                };
                *count += 1;
                id == wanted
            })
        });
        let Some(n) = found else {
            self.emit_error(format!("no heading for #{anchor}"));
            return;
        };
        let at = self.line_start(n).offset();
        // after this click is done with, or GTK puts the cursor at it; the
        // heading goes to the top, its section under it
        let weak = self.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(editor) = upgrade(&weak) {
                editor.place_cursor(at);
                let insert = editor.0.buffer.get_insert();
                editor.0.view.scroll_to_mark(&insert, 0.0, true, 0.0, 0.0);
                editor.0.view.grab_focus();
            }
        });
    }

    // -- pictures ------------------------------------------------------------------

    /// The picture a local image link shows, once loaded; asking for one
    /// not loaded yet starts loading it. Web images are not fetched.
    fn picture_of(&self, url: &str) -> Option<gdk::Texture> {
        if url.is_empty() || has_scheme(url) {
            return None;
        }
        let path = self.linked_path(url)?;
        let state = self.0.pictures.borrow().get(&path).cloned();
        match state {
            Some(picture) => picture,
            None => {
                self.load_picture(path);
                None
            }
        }
    }

    /// Reads a picture off the main thread; its lines are drawn again once
    /// it is in.
    fn load_picture(&self, path: PathBuf) {
        self.0.pictures.borrow_mut().insert(path.clone(), None);
        let weak = self.downgrade();
        glib::spawn_future_local(async move {
            let source = path.clone();
            let texture = gio::spawn_blocking(move || gdk::Texture::from_filename(&source).ok())
                .await
                .ok()
                .flatten();
            let Some(editor) = upgrade(&weak) else { return };
            if texture.is_none() {
                return;
            }
            editor.0.pictures.borrow_mut().insert(path, texture);
            for n in 0..editor.line_count() {
                let text = editor.line_model(n).text();
                if text.contains("![") || text.to_ascii_lowercase().contains("<img") {
                    editor.decorate_line(n, None);
                }
            }
            editor.0.view.queue_draw();
        });
    }

    /// The pictures shown under line `n`, sized to fit the view.
    fn pictures(&self, n: usize) -> Pictures {
        let view = &self.0.view;
        let zoom = view.zoom();
        let line = self.line_model(n);
        let margin = view.cursor_locations(Some(&self.line_start(n))).0.x() as f32;
        let room = if view.width() > 100 {
            view.width() as f32 - margin - 4.0 * zoom
        } else {
            480.0 * zoom
        };
        markdown::inlines(&line.spans)
            .into_iter()
            .filter(|i| i.kind == InlineKind::Image)
            .filter_map(|i| self.picture_of(&i.url))
            .map(|texture| {
                let (w, h) = (
                    texture.width() as f32 * zoom,
                    texture.height() as f32 * zoom,
                );
                let scale = (room / w).min(280.0 * zoom / h).min(1.0);
                let size = ((w * scale).round(), (h * scale).round());
                (texture, size.0, size.1)
            })
            .collect()
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
        // hardware keycodes for the digit row and `.`, so layouts don't matter
        const KEY_1: u32 = 10;
        const KEY_6: u32 = 15;
        const KEY_7: u32 = 16;
        const KEY_8: u32 = 17;
        const KEY_9: u32 = 18;
        const KEY_0: u32 = 19;
        const KEY_PERIOD: u32 = 60;

        if ctrl {
            match key.to_lower() {
                gdk::Key::b => self.toggle_style(Style::BOLD),
                gdk::Key::i => self.toggle_style(Style::ITALIC),
                gdk::Key::u => self.toggle_style(Style::UNDERLINE),
                gdk::Key::z => self.undo(),
                gdk::Key::y => self.redo(),
                _ => match code {
                    KEY_1..=KEY_6 => {
                        self.set_line_kind(LineKind::heading((code - KEY_1 + 1) as u8))
                    }
                    KEY_0 => self.set_line_kind(LineKind::Text),
                    _ => return false,
                },
            }
            return true;
        }
        if ctrl_shift {
            match (key.to_lower(), code) {
                (gdk::Key::z, _) => self.redo(),
                (gdk::Key::x, _) => self.toggle_style(Style::STRIKE),
                (gdk::Key::h, _) => self.toggle_highlight(),
                (_, KEY_7) => self.toggle_numbers(),
                (_, KEY_8) => self.toggle_bullets(),
                (_, KEY_9) => self.toggle_tasks(),
                (_, KEY_PERIOD) => self.toggle_quote(),
                _ => return false,
            }
            return true;
        }
        if !mods.is_empty() && mods != M::SHIFT_MASK {
            return false;
        }
        match key {
            gdk::Key::Tab if mods.is_empty() => self.table_tab(false) || self.indent_bullets(1),
            gdk::Key::ISO_Left_Tab | gdk::Key::Tab => {
                // outdents bullets; elsewhere it does nothing (no stray tab)
                if !self.table_tab(true) {
                    self.indent_bullets(-1);
                }
                true
            }
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

    /// What typing the start of a line and a space makes: `- ` / `* ` a
    /// bullet, `1. ` a numbered item, `[ ] ` a task, `> ` a quote, `# ` to
    /// `###### ` a heading; `[ ] ` on a list item gives it a box.
    fn autoformat(&self) -> bool {
        let Some((line, cursor)) = self.cursor_line() else {
            return false;
        };
        let kind = self.0.view.kind(line);
        let start = self.line_start(line);
        let typed = self.0.buffer.text(&start, &cursor, true);
        let task = match typed.as_str() {
            "[]" | "[ ]" => Some(Task::Open),
            "[x]" | "[X]" => Some(Task::Done(b'x')),
            _ => None,
        };
        let kind = match kind {
            LineKind::Text => match typed.as_str() {
                "-" | "*" => LineKind::bullet(0),
                ">" => LineKind::quote(1),
                _ if task.is_some() => LineKind::bullet(0).with_task(task),
                t if (1..=6).contains(&t.len()) && t.chars().all(|c| c == '#') => {
                    LineKind::heading(t.len() as u8)
                }
                t => {
                    let digits = t.len().saturating_sub(1);
                    let number = t.get(..digits).and_then(|d| d.parse::<u32>().ok());
                    // a leading zero would be lost (`007.` stays text)
                    let zero = t.starts_with('0') && digits > 1;
                    match (number, t.as_bytes().last()) {
                        (Some(number), Some(&delim @ (b'.' | b')'))) if digits <= 9 && !zero => {
                            LineKind::Numbered {
                                indent: 0,
                                number,
                                delim,
                                task: None,
                            }
                        }
                        _ => return false,
                    }
                }
            },
            k if k.is_list() && k.task().is_none() && task.is_some() => k.with_task(task),
            _ => return false,
        };
        // the space is typed as usual first, so undoing the formatting
        // leaves the literal "- " (and typing on keeps it literal)
        self.0.buffer.insert_interactive_at_cursor(" ", true);
        let prefix = typed.chars().count() as i32 + 1;
        self.change(line, line, || {
            let mut a = self.line_start(line);
            let mut b = self.0.buffer.iter_at_offset(a.offset() + prefix);
            self.0.buffer.delete(&mut a, &mut b);
            self.set_kind(line, kind);
        });
        true
    }

    /// Enter on an empty list item or quote line ends it; in a CRLF note
    /// Enter types CRLF so the file keeps one kind of line ending. Tables,
    /// code blocks, math and rules have their own Enter (see above).
    fn enter(&self) -> bool {
        if self.table_enter()
            || self.start_table()
            || self.start_code_block()
            || self.start_math()
            || self.start_rule()
            || self.leave_code_block()
            || self.code_enter()
        {
            return true;
        }
        if let Some((line, cursor)) = self.cursor_line() {
            let kind = self.0.view.kind(line);
            let ends = kind.is_list() || matches!(kind, LineKind::Quote { .. });
            if ends && cursor.line_offset() == 0 && cursor.ends_line() {
                self.change(line, line, || self.set_kind(line, LineKind::Text));
                return true;
            }
        }
        if self.newline() == Eol::CrLf {
            let buffer = &self.0.buffer;
            buffer.delete_selection(true, true);
            buffer.insert_interactive_at_cursor("\r\n", true);
            self.scroll_to_cursor();
            return true;
        }
        false
    }

    /// Backspace at the start of a list item outdents it, then turns it
    /// into text; at the start of a quote it takes a level off; at the
    /// start of a heading it turns it into text.
    fn backspace(&self) -> bool {
        let Some((line, cursor)) = self.cursor_line() else {
            return false;
        };
        if cursor.line_offset() != 0 {
            return false;
        }
        let kind = self.0.view.kind(line);
        let next = match kind {
            k if k.is_list() && k.list_indent() != Some(0) => {
                let kinds = self.0.view.imp().kinds.borrow().clone();
                let (_, out) = markdown::list_moves(&kinds, line);
                k.with_indent(out.unwrap_or(0))
            }
            LineKind::Quote { depth, .. } if depth > 1 => LineKind::quote(depth - 1),
            k if k.is_list() || k.heading_level().is_some() => LineKind::Text,
            LineKind::Quote { .. } => LineKind::Text,
            _ => return false,
        };
        self.change(line, line, || self.set_kind(line, next));
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

    /// Pasted text is read as Markdown (as plain text in a plain file).
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
        // into a raw line (code, a table) text goes as it is
        let plain = self.plain() || current.kind == LineKind::Raw;
        let mut pasted = parse(text, plain);
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
                kind: if plain { LineKind::Raw } else { LineKind::Text },
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
        let newline = self.newline();
        for line in pasted.iter_mut().take(n - 1) {
            line.eol = newline;
        }

        let line_start = self.line_start(line).offset();
        let before_last: i32 = pasted
            .iter()
            .take(n - 1)
            .map(|l| (l.text().chars().count() + l.eol.as_str().len()) as i32)
            .sum();
        let cursor_after = line_start + before_last + cursor_chars as i32;
        self.rewrite(line, line, pasted, cursor_after);
        if n == 1 && self.0.view.block(line).is_table() {
            self.schedule_align();
        }
    }

    // -- format toolbar ------------------------------------------------------

    fn build_toolbar(&self) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        // (name, CSS class, icon instead of the name)
        let buttons: [(&str, &str, Option<&str>); 11] = [
            ("B", "b", None),
            ("I", "i", None),
            ("U", "u", None),
            ("S", "s", None),
            ("mark", "icon", Some(theme::ICON_HIGHLIGHT)),
            ("•", "dot", None),
            ("1.", "num", None),
            ("task", "icon", Some(theme::ICON_TASK)),
            ("quote", "icon", Some(theme::ICON_QUOTE)),
            ("H1", "h1", None),
            ("H2", "h2", None),
        ];
        for (label, class, icon) in buttons {
            let button = gtk::Button::builder()
                .focus_on_click(false)
                .can_focus(false)
                .css_classes([class])
                .build();
            match icon {
                Some(svg) => button.set_child(Some(&Icon::new(svg, 14))),
                None => button.set_label(label),
            }
            button.set_cursor_from_name(Some("pointer"));
            let weak = self.downgrade();
            button.connect_clicked(move |_| {
                let Some(editor) = upgrade(&weak) else { return };
                match label {
                    "B" => editor.toggle_style(Style::BOLD),
                    "I" => editor.toggle_style(Style::ITALIC),
                    "U" => editor.toggle_style(Style::UNDERLINE),
                    "S" => editor.toggle_style(Style::STRIKE),
                    "mark" => editor.toggle_highlight(),
                    "•" => editor.toggle_bullets(),
                    "1." => editor.toggle_numbers(),
                    "task" => editor.toggle_tasks(),
                    "quote" => editor.toggle_quote(),
                    "H1" => editor.set_line_kind(LineKind::H1),
                    _ => editor.set_line_kind(LineKind::H2),
                }
            });
            row.append(&button);
            self.0.toolbar_buttons.borrow_mut().push((label, button));
        }
        self.0.toolbar.set_child(Some(&row));
    }

    /// Marks the toolbar buttons whose formatting the whole selection has,
    /// so it is clear what a click will switch off.
    fn update_toolbar_state(&self) {
        let Some((start, end)) = self.0.buffer.selection_bounds() else {
            return;
        };
        let tags = &self.0.tags;
        let all_chars = |tag: &gtk::TextTag| {
            let mut iter = start;
            while iter < end {
                if !iter.ends_line() && !iter.has_tag(tag) {
                    return false;
                }
                iter.forward_char();
            }
            true
        };
        let (first, last) = self.selected_lines();
        let all_lines =
            |check: &dyn Fn(LineKind) -> bool| (first..=last).all(|n| check(self.0.view.kind(n)));
        for (label, button) in self.0.toolbar_buttons.borrow().iter() {
            let on = match *label {
                "B" => all_chars(&tags.bold),
                "I" => all_chars(&tags.italic),
                "U" => all_chars(&tags.underline),
                "S" => all_chars(&tags.strike),
                "mark" => self.selected_highlight().is_some(),
                "•" => all_lines(&|k| matches!(k, LineKind::Bullet { task: None, .. })),
                "1." => all_lines(&|k| matches!(k, LineKind::Numbered { task: None, .. })),
                "task" => all_lines(&|k| k.task().is_some()),
                "quote" => all_lines(&|k| matches!(k, LineKind::Quote { .. })),
                "H1" => all_lines(&|k| k == LineKind::H1),
                _ => all_lines(&|k| k == LineKind::H2),
            };
            if on {
                button.add_css_class("on");
            } else {
                button.remove_css_class("on");
            }
        }
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
                        editor.no_drag_and_drop(event);
                    }
                    gdk::EventType::ButtonRelease => {
                        editor.0.state.borrow_mut().pointer_down = false;
                        editor.schedule_toolbar();
                    }
                    // a release can get lost (say, to a drag); typing means
                    // the mouse is not selecting any more
                    gdk::EventType::KeyPress => {
                        editor.0.state.borrow_mut().pointer_down = false;
                    }
                    _ => {}
                }
            }
            glib::Propagation::Proceed
        });
        self.0.view.add_controller(legacy);

        // hidden while scrolling, back once it stops
        if let Some(adjustment) = self.0.view.vadjustment() {
            let weak = self.downgrade();
            adjustment.connect_value_changed(move |_| {
                if let Some(editor) = upgrade(&weak)
                    && editor.0.buffer.has_selection()
                {
                    editor.hide_toolbar();
                    editor.schedule_toolbar();
                }
            });
        }
        // typing over or deleting the selection removes it without moving
        // a mark, so watch the selection itself
        let weak = self.downgrade();
        self.0.buffer.connect_has_selection_notify(move |_| {
            if let Some(editor) = upgrade(&weak) {
                editor.schedule_toolbar();
            }
        });
    }

    /// Pressing inside the selection would start dragging the text (which
    /// drops its formatting and its Markdown, and leaves the toolbar
    /// stuck): collapse the selection at the click instead, so a drag
    /// selects anew.
    fn no_drag_and_drop(&self, event: &gdk::Event) {
        let Some(button) = event.downcast_ref::<gdk::ButtonEvent>() else {
            return;
        };
        if button.button() != gdk::BUTTON_PRIMARY
            || event
                .modifier_state()
                .contains(gdk::ModifierType::SHIFT_MASK)
        {
            return;
        }
        let Some((start, _)) = self.0.buffer.selection_bounds() else {
            return;
        };
        let view = &self.0.view;
        let (Some((x, y)), Some(native)) = (event.position(), view.native()) else {
            return;
        };
        let (sx, sy) = native.surface_transform();
        let point = graphene::Point::new((x - sx) as f32, (y - sy) as f32);
        let Some(p) = native.compute_point(view, &point) else {
            return;
        };
        let (bx, by) =
            view.window_to_buffer_coords(gtk::TextWindowType::Widget, p.x() as i32, p.y() as i32);
        // GTK finds the press inside the selection in more places than
        // `char_at` does (the room between lines): any press collapses it,
        // and GTK then puts the cursor where it lands
        let iter = view.char_at(bx, by, &[]).unwrap_or(start);
        self.0.buffer.place_cursor(&iter);
    }

    /// Shows the toolbar again if there is a selection (say, after the
    /// window was hidden and shown).
    pub fn refresh_toolbar(&self) {
        self.schedule_toolbar();
    }

    /// Hides the toolbar at once; shows it again once the selection settles.
    fn schedule_toolbar(&self) {
        if let Some(id) = self.0.state.borrow_mut().toolbar_timer.take() {
            id.remove();
        }
        let dragging = self.0.state.borrow().pointer_down;
        let (first, last) = self.selected_lines();
        let raw = (first..=last).all(|n| self.0.view.kind(n) == LineKind::Raw);
        if !self.0.buffer.has_selection() || dragging || self.plain() || raw {
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
        let glyphs = view.cursor_at(&first);
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
        let selection = (start.offset(), end.offset());
        let x = match self.0.state.borrow().toolbar_x {
            Some((a, b, kept)) if (a, b) == selection => kept,
            _ => x,
        };
        self.0.state.borrow_mut().toolbar_x = Some((selection.0, selection.1, x));
        let room_above = y >= toolbar_height + 2;
        let (position, rect) = if room_above {
            (
                gtk::PositionType::Top,
                gdk::Rectangle::new(x, y, width.max(1), 1),
            )
        } else {
            // no room above (the line is at the top, maybe half scrolled
            // out): go under the end of the selection, never on top of it
            let end_glyphs = view.cursor_at(&end);
            let (ey, eh) = view.glyph_box(&end_glyphs);
            let (_, ey) = view.buffer_to_window_coords(gtk::TextWindowType::Widget, 0, ey as i32);
            let below = ey + eh as i32;
            if below + toolbar_height <= view.height() {
                (
                    gtk::PositionType::Bottom,
                    gdk::Rectangle::new(x, below.max(y + height as i32), width.max(1), 1),
                )
            } else {
                (
                    gtk::PositionType::Top,
                    gdk::Rectangle::new(x, view.height(), width.max(1), 1),
                )
            }
        };
        self.update_toolbar_state();
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
        self.0.view.imp().blocks.replace(vec![Block::None]);
        self.0.view.imp().shapes.replace(vec![Shape::default()]);
        self.0.view.imp().cache.replace(vec![None]);
        self.0.state.borrow_mut().programmatic = false;
        self.replace_lines(0, 1, lines);
        let cursor = cursor.clamp(0, buffer.char_count());
        buffer.place_cursor(&buffer.iter_at_offset(cursor));
        self.scroll_to_cursor();
    }

    /// Opens a file. `managed` notes (inside the notes folder) are renamed
    /// after their title; others are edited where they are. Plain text has
    /// no title, so it keeps its name.
    pub fn open(&self, path: &Path, managed: bool) -> std::io::Result<()> {
        self.close();
        let text = std::fs::read_to_string(path)?;
        let plain = !storage::is_markdown(path);
        let lines = parse(&text, plain);
        let final_eol = lines.last().map_or(Eol::None, |l| l.eol);
        let crlf = lines.iter().filter(|l| l.eol == Eol::CrLf).count();
        let lf = lines.iter().filter(|l| l.eol == Eol::Lf).count();
        self.0.state.borrow_mut().doc = Some(Doc {
            path: Some(path.to_path_buf()),
            managed: path
                .parent()
                .filter(|_| managed && !plain)
                .map(Path::to_path_buf),
            created: false,
            plain,
            final_eol,
            newline: if crlf > lf { Eol::CrLf } else { Eol::Lf },
            saved: text.clone(),
            dirty: false,
        });
        self.load(&lines, text.chars().count() as i32);
        // cursor at the end of the text, not after a trailing newline
        let buffer = &self.0.buffer;
        buffer.place_cursor(&buffer.end_iter());
        self.scroll_to_cursor();
        self.emit_status();
        self.recount();
        Ok(())
    }

    /// Starts a new note in `dir` with an empty title line.
    pub fn new_note(&self, dir: &Path) {
        self.close();
        self.0.state.borrow_mut().doc = Some(Doc {
            path: None,
            managed: Some(dir.to_path_buf()),
            created: true,
            plain: false,
            final_eol: Eol::Lf,
            newline: Eol::Lf,
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
        self.hide_find();
        self.0.word_count.menu.popdown();
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
            Some(Doc {
                managed: Some(dir), ..
            }) => {
                let dir = dir.clone();
                drop(state);
                let slug = storage::slugify(&self.title().unwrap_or_default());
                config::tilde_path(&dir.join(format!("{slug}.md")))
            }
            _ => String::new(),
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
        if let Some(dir) = doc.managed.clone() {
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
            if !doc
                .path
                .as_deref()
                .is_some_and(|p| storage::same_file(p, path))
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
            let plain = doc.plain;
            drop(state);
            let lines = parse(&text, plain);
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

fn parse(text: &str, plain: bool) -> Vec<Line> {
    if plain {
        markdown::parse_plain(text)
    } else {
        markdown::parse(text)
    }
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

/// The anchor GitHub gives a heading: lower case, spaces to `-`, other
/// punctuation left out (`Pattern 1: Weekly!` -> `pattern-1-weekly`).
fn heading_anchor(text: &str) -> String {
    text.trim()
        .chars()
        .flat_map(char::to_lowercase)
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

/// A link with a scheme (`https:`, `mailto:`) rather than a path.
fn has_scheme(url: &str) -> bool {
    url.split_once(':').is_some_and(|(s, _)| {
        s.len() > 1
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
    })
}

/// `%20` and the like in a link to a file.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).and_then(|&b| hex(b)),
                bytes.get(i + 2).and_then(|&b| hex(b)),
            )
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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
    fn heading_anchors_like_github() {
        assert_eq!(heading_anchor("Install"), "install");
        assert_eq!(
            heading_anchor("Pattern 1: Weekly is the new monthly!"),
            "pattern-1-weekly-is-the-new-monthly"
        );
        assert_eq!(heading_anchor("Hyprland setup"), "hyprland-setup");
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
        let editor = Editor::new();

        // a new note starts on an empty title line
        editor.new_note(&dir);
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
        // (the last line goes on the numbered item: typed at its end, the
        // `!` is bold like the word before it)
        editor.place_cursor(editor.0.buffer.char_count());
        type_text(&editor, "!");
        editor.save_sync();
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            readme.replace("**bold**", "**bold!**")
        );
        assert!(
            outside.exists() && !dir.join("readme.md").exists(),
            "outside files are never moved"
        );

        check_cache(&editor);
        // a new note left empty leaves nothing behind
        editor.new_note(&dir);
        type_text(&editor, "x");
        editor.save_sync();
        assert!(dir.join("x.md").exists());
        editor.undo();
        editor.close();
        assert!(!dir.join("x.md").exists());

        // a note started in an added folder is written there and named
        // after its title while it is open; reopened, it keeps its name
        let folder = temp_dir("folder");
        editor.new_note(&folder);
        assert_eq!(
            editor.status_path(),
            config::tilde_path(&folder.join("untitled.md"))
        );
        type_text(&editor, "Plan");
        editor.save_sync();
        type_text(&editor, "s\nship it");
        editor.save_sync();
        let plans = folder.join("plans.md");
        assert_eq!(
            std::fs::read_to_string(&plans).unwrap(),
            "# Plans\nship it\n"
        );
        assert!(!folder.join("plan.md").exists() && !dir.join("plans.md").exists());
        editor.open(&plans, false).unwrap();
        editor.place_cursor(find(&editor, "Plans") + 5);
        type_text(&editor, " v2");
        editor.save_sync();
        assert_eq!(
            std::fs::read_to_string(&plans).unwrap(),
            "# Plans v2\nship it\n"
        );
        assert_eq!(
            std::fs::read_dir(&folder).unwrap().count(),
            1,
            "not renamed"
        );
        editor.close();

        // undo goes back a word at a time, not a whole sentence
        editor.new_note(&dir);
        type_text(&editor, "Words\nfirst second third");
        editor.undo();
        assert_eq!(editor.text(), "# Words\nfirst second\n");
        editor.undo();
        assert_eq!(editor.text(), "# Words\nfirst\n");
        editor.redo();
        editor.redo();
        assert_eq!(editor.text(), "# Words\nfirst second third\n");

        // Shift+Tab outside a list types nothing
        assert!(editor.key(gdk::Key::ISO_Left_Tab, 23, gdk::ModifierType::SHIFT_MASK));
        assert_eq!(editor.text(), "# Words\nfirst second third\n");

        // undoing a bullet autoformat leaves the literal "- "
        type_text(&editor, "\n- ");
        assert!(editor.text().ends_with("\n- \n"), "{:?}", editor.text());
        assert!(matches!(editor.0.view.kind(2), LineKind::Bullet { .. }));
        editor.undo();
        assert_eq!(editor.0.view.kind(2), LineKind::Text);
        type_text(&editor, "x");
        assert!(editor.text().ends_with("\\- x\n"), "{:?}", editor.text());
        editor.close();

        // a CRLF note stays CRLF when typing Enter and pasting
        let crlf = temp_dir("crlf").join("dos.md");
        std::fs::write(&crlf, "# Dos\r\nline\r\n").unwrap();
        editor.open(&crlf, false).unwrap();
        type_text(&editor, "\nnew");
        editor.paste("a\nb");
        editor.save_sync();
        let saved = std::fs::read_to_string(&crlf).unwrap();
        assert_eq!(saved, "# Dos\r\nline\r\nnewa\r\nb\r\n");

        // a .txt is plain text: nothing in it is Markdown, and it keeps its
        // name even in the notes folder
        let txt = dir.join("todo.txt");
        let plain = "# not a title\r\n- not a bullet\r\n**not bold** 5 \\* 3\r\n";
        std::fs::write(&txt, plain).unwrap();
        editor.open(&txt, true).unwrap();
        assert!((0..editor.line_count()).all(|n| editor.0.view.kind(n) == LineKind::Raw));
        assert_eq!(editor.text(), plain);
        select(&editor, 0, 5);
        editor.toggle_style(Style::BOLD);
        editor.set_line_kind(LineKind::H1);
        editor.toggle_bullets();
        assert_eq!(editor.text(), plain, "no formatting in plain text");
        editor.place_cursor(editor.0.buffer.char_count());
        type_text(&editor, "\n- a *b* \\c\n# d");
        editor.paste("**e**\n## f\n");
        editor.save_sync();
        let typed = format!("{plain}- a *b* \\c\r\n# d**e**\r\n## f\r\n\r\n");
        assert_eq!(std::fs::read_to_string(&txt).unwrap(), typed);
        assert!(!dir.join("not-a-title.md").exists(), "never renamed");
        assert!((0..editor.line_count()).all(|n| editor.0.view.kind(n) == LineKind::Raw));
        editor.undo();
        assert_eq!(editor.text(), format!("{plain}- a *b* \\c\r\n# d\r\n"));
        editor.redo();
        editor.save_sync();
        std::fs::write(&txt, "*changed* outside\n").unwrap();
        editor.file_changed(&txt);
        assert_eq!(editor.text(), "*changed* outside\n");
        assert_eq!(editor.0.view.kind(0), LineKind::Raw);
        editor.close();

        // code blocks: ``` and Enter make one, closed, with the cursor inside;
        // what is typed there is code: kept as written, indentation carried on
        editor.new_note(&dir);
        type_text(
            &editor,
            "Code\n```rust\nfn main() {\n    let x = *y;\n- not a bullet",
        );
        let code = "# Code\n```rust\nfn main() {\n    let x = *y;\n    - not a bullet\n```\n";
        assert_eq!(editor.text(), code);
        let blocks: Vec<Block> = (0..editor.line_count())
            .map(|n| editor.0.view.block(n))
            .collect();
        assert_eq!(
            blocks,
            [
                Block::None,
                Block::FenceOpen,
                Block::Code,
                Block::Code,
                Block::Code,
                Block::FenceClose
            ]
        );
        assert!(editor.line_start(2).has_tag(&editor.0.tags.mono_block));
        select(&editor, find(&editor, "fn"), find(&editor, "main"));
        editor.toggle_style(Style::BOLD);
        assert_eq!(editor.text(), code, "no formatting in code");
        // pasted into code, Markdown stays as it is
        editor.place_cursor(find(&editor, "    - not"));
        editor.paste("**a** ");
        assert!(editor.text().contains("\n**a**     - not a bullet\n"));
        editor.undo();
        // Enter after the closing fence is back to text
        editor.place_cursor(editor.0.buffer.char_count());
        type_text(&editor, "\nafter");
        assert_eq!(editor.text(), format!("{code}after\n"));
        assert_eq!(editor.0.view.kind(6), LineKind::Text);
        assert_eq!(editor.0.view.block(6), Block::None);

        // tables: a head line, a |---| line and Enter make one; typing in a
        // cell keeps the columns lined up; Tab moves on, Enter adds a row
        let flush = || while glib::MainContext::default().iteration(false) {};
        let type_slowly = |text: &str| {
            for c in text.chars() {
                type_text(&editor, &c.to_string());
                flush();
            }
        };
        type_text(&editor, "\n| Name | Age |\n|---|---|\n");
        let table = "| Name | Age |\n| ---- | --- |\n|      |     |\n";
        assert_eq!(editor.text(), format!("{code}after\n{table}"));
        type_slowly("Ada Lovelace");
        let tab = |back: bool| {
            let (key, mods) = if back {
                (gdk::Key::ISO_Left_Tab, gdk::ModifierType::SHIFT_MASK)
            } else {
                (gdk::Key::Tab, gdk::ModifierType::empty())
            };
            assert!(editor.key(key, 23, mods));
        };
        tab(false);
        type_slowly("36");
        tab(false);
        type_slowly("Grace");
        let table = "| Name         | Age |\n| ------------ | --- |\n| Ada Lovelace | 36  |\n| Grace        |     |\n";
        assert_eq!(editor.text(), format!("{code}after\n{table}"));
        tab(true);
        tab(true);
        assert_eq!(
            editor.0.buffer.cursor_position(),
            find(&editor, "Ada Lovelace") + 12,
            "Shift+Tab goes back a cell"
        );
        // Enter adds a row; Enter on that empty last row leaves the table
        editor.place_cursor(find(&editor, "Grace") + 5);
        type_text(&editor, "\n\nend");
        assert_eq!(editor.text(), format!("{code}after\n{table}end\n"));
        assert_eq!(
            editor.0.view.block(editor.line_count() - 2),
            Block::TableEnd
        );
        // one undo takes back a word typed in a cell and the lining up
        editor.undo();
        editor.undo();
        editor.undo();
        assert!(
            editor.text().ends_with("| Grace        |     |\n"),
            "{}",
            editor.text()
        );
        editor.undo();
        assert!(
            editor
                .text()
                .ends_with("| Ada Lovelace | 36  |\n|              |     |\n"),
            "{}",
            editor.text()
        );
        check_cache(&editor);
        editor.close();

        // links read as their text: the syntax hides unless the cursor is at
        // it; bare URLs and code spans are found too
        editor.new_note(&dir);
        type_text(&editor, "Links\n");
        editor.paste("See [the docs](https://x.y), `code` and https://kludd.dev");
        let tags = &editor.0.tags;
        let at = |needle: &str| editor.0.buffer.iter_at_offset(find(&editor, needle));
        assert!(
            at("[the").has_tag(&tags.conceal),
            "hidden while the cursor is away"
        );
        assert!(at("the docs").has_tag(&tags.link));
        assert!(at("kludd.dev").has_tag(&tags.link));
        assert!(at("code").has_tag(&tags.mono));
        editor.place_cursor(find(&editor, "docs"));
        assert!(!at("[the").has_tag(&tags.conceal), "shown at the cursor");
        assert!(at("[the").has_tag(&tags.syntax));
        assert_eq!(
            editor.text(),
            "# Links\nSee [the docs](https://x.y), `code` and https://kludd.dev\n"
        );
        // a click opens a link, but one being edited takes clicks like
        // text (Ctrl+click still opens it)
        let open = |needle: &str, ctrl: bool| editor.link_to_open(&at(needle), ctrl);
        assert_eq!(open("the docs", false), None, "being edited");
        assert_eq!(open("the docs", true).as_deref(), Some("https://x.y"));
        assert_eq!(
            open("kludd.dev", false).as_deref(),
            Some("https://kludd.dev")
        );
        assert_eq!(open("code", false), None, "not a link");
        editor.place_cursor(find(&editor, "See"));
        assert_eq!(open("the docs", false).as_deref(), Some("https://x.y"));
        editor.place_cursor(find(&editor, ", `code`"));
        assert_eq!(open("the docs", false), None, "cursor right after it");
        // a `#section` link goes to its heading
        assert_eq!(editor.line_model(0).text(), "Links");
        editor.open_link("#links");
        glib::MainContext::default().iteration(false);
        while glib::MainContext::default().iteration(false) {}
        let buffer = &editor.0.buffer;
        assert_eq!(
            buffer.iter_at_mark(&buffer.get_insert()).offset(),
            0,
            "at the heading"
        );
        // a bare URL shows no Markdown to edit: right after it, it opens
        editor.place_cursor(find(&editor, "kludd.dev") + 9);
        assert_eq!(
            open("kludd.dev", false).as_deref(),
            Some("https://kludd.dev")
        );
        editor.place_cursor(find(&editor, "kludd.dev") + 2);
        assert_eq!(open("kludd.dev", false), None, "cursor inside it");

        // Ctrl+F: finds in the note, from the cursor on, round at the end
        let finds = || editor.0.view.imp().finds.borrow().clone();
        let cursor = || {
            let buffer = &editor.0.buffer;
            buffer.iter_at_mark(&buffer.get_insert()).offset()
        };
        editor.place_cursor(find(&editor, "code"));
        editor.open_find();
        editor.0.find.text.set_text("HTTPS");
        let (docs, dev) = (find(&editor, "https://x"), find(&editor, "https://k"));
        assert_eq!(finds(), [(docs, docs + 5), (dev, dev + 5)], "any case");
        assert_eq!(cursor(), dev, "the first match after the cursor");
        editor.find_step(1);
        assert_eq!(cursor(), docs, "round to the first");
        editor.find_step(-1);
        assert_eq!(cursor(), dev);
        type_text(&editor, "x");
        glib::MainContext::default().iteration(false);
        while glib::MainContext::default().iteration(false) {}
        assert_eq!(finds().len(), 2, "follows the text as it changes");
        editor.undo();
        // what is shown is found too: across a link's hidden `[`
        editor.place_cursor(0);
        editor.0.find.text.set_text("see the docs");
        let see = find(&editor, "See [");
        assert_eq!(finds(), [(see, see + 13)], "across hidden syntax");
        assert!(editor.close_find());
        assert!(finds().is_empty() && !editor.close_find());
        editor.close();

        // GTK aborts on a point in the room below a line with hidden text:
        // a heading's room below moves to the top of the next line
        editor.new_note(&dir);
        editor.paste("Rooms\n\n## See [the docs](https://x.y)\n\nNext line\n");
        editor.place_cursor(0);
        let line_tags = |n: usize| -> Vec<String> {
            editor
                .line_start(n)
                .tags()
                .iter()
                .filter_map(|t| t.name().map(|n| n.to_string()))
                .collect()
        };
        let heading = (0..editor.line_count())
            .find(|&n| editor.line_model(n).text().starts_with("See"))
            .unwrap();
        assert!(line_tags(heading).iter().any(|t| t == "no-below"));
        assert!(
            line_tags(heading + 1)
                .iter()
                .any(|t| t.starts_with("lift-"))
        );
        // shown (the cursor in it), it has its room below again
        editor.place_cursor(find(&editor, "docs"));
        assert!(!line_tags(heading).iter().any(|t| t == "no-below"));
        assert!(
            !line_tags(heading + 1)
                .iter()
                .any(|t| t.starts_with("lift-"))
        );
        editor.close();

        // which way each character reads, for measuring mixed lines
        editor.new_note(&dir);
        editor.paste("Dirs\n\nGo שלום עולם now 12\n\nשלום abc ١٢٣ סוף\n");
        let directions = |needle: &str| {
            let n = (0..editor.line_count())
                .find(|&n| editor.line_model(n).text().contains(needle))
                .unwrap();
            let (start, end) = (editor.line_start(n), editor.line_content_end(n));
            let dirs = editor.0.view.line_directions(&start, &end).unwrap();
            let text: String = start.visible_text(&end).into();
            let ways: String = dirs
                .chars
                .iter()
                .map(|(_, rtl)| if *rtl { 'R' } else { 'L' })
                .collect();
            (dirs.base, text, ways)
        };
        // the spaces between Hebrew words are Hebrew's; digits read left
        // to right, and the space before Arabic ones counts as the line's
        let (base, text, ways) = directions("Go");
        assert_eq!((base, text.as_str()), (false, "Go שלום עולם now 12"));
        assert_eq!(ways, "LLLRRRRRRRRRLLLLLLL");
        let (base, text, ways) = directions("abc");
        assert_eq!((base, text.as_str()), (true, "שלום abc ١٢٣ סוף"));
        assert_eq!(ways, "RRRRRLLLRLLLRRRR");
        assert!(!is_rtl('١') && is_rtl('ש'));
        editor.close();

        // a setext rule is hidden whole: the room goes past it
        let setext = dir.join("setext.md");
        std::fs::write(
            &setext,
            "Setext with [a link](https://s.com)\n====\n\nAfter it\n",
        )
        .unwrap();
        editor.open(&setext, false).unwrap();
        editor.place_cursor(editor.text().chars().count() as i32);
        let title = (0..editor.line_count())
            .find(|&n| editor.line_model(n).text().starts_with("Setext"))
            .unwrap();
        assert!(line_tags(title).iter().any(|t| t == "no-below"));
        assert!(editor.0.view.hidden_whole(&editor.line_start(title + 1)));
        assert!(line_tags(title + 2).iter().any(|t| t.starts_with("lift-")));
        editor.close();

        // an empty last line, which takes no tags, is lifted by the view
        editor.new_note(&dir);
        editor.paste("Rooms\n\n# Final [link](https://x.com)\n");
        editor.place_cursor(editor.text().chars().count() as i32);
        let lifted = editor.0.view.pixels_above_lines();
        assert!(lifted > 0, "{lifted}");
        assert_eq!(editor.0.view.bottom_margin(), 0);
        type_text(&editor, "x");
        assert_eq!(editor.0.view.pixels_above_lines(), 0);
        editor.close();

        // numbered lists, tasks, quotes and deeper headings: started by
        // typing their Markdown, carried on by Enter, ended on an empty line
        editor.new_note(&dir);
        type_text(
            &editor,
            "Kinds\n1. one\ntwo\n\n[ ] milk\neggs\n\n> said\nmore\n\n### Three\nplain",
        );
        assert_eq!(
            editor.text(),
            "# Kinds\n1. one\n2. two\n- [ ] milk\n- [ ] eggs\n> said\n> more\n### Three\nplain\n"
        );
        // a click on the box ticks it (one undo step); ticked, it stays
        // ticked for Enter's new item to start open
        editor.toggle_task(3);
        assert!(editor.text().contains("- [x] milk\n- [ ] eggs\n"));
        assert!(editor.line_start(3).has_tag(&editor.0.tags.done));
        editor.undo();
        assert!(editor.text().contains("- [ ] milk\n"));
        // Tab nests a numbered item under the one before (three spaces in,
        // where its text starts); Shift+Tab takes it back out
        editor.place_cursor(find(&editor, "two"));
        assert!(editor.indent_bullets(1));
        assert!(
            editor.text().contains("1. one\n   2. two\n"),
            "{}",
            editor.text()
        );
        assert_eq!(editor.0.view.shape(2).level, 1);
        assert_eq!(
            editor.0.view.shape(2).number,
            2,
            "a new list counts from its first number"
        );
        assert!(editor.indent_bullets(-1));
        assert!(editor.text().contains("1. one\n2. two\n"));
        // Backspace takes a quote level off, then the quote
        editor.place_cursor(find(&editor, "more"));
        assert!(editor.backspace());
        assert!(editor.text().contains("> said\nmore\n"));
        editor.undo();
        // the toolbar's kinds, and strikethrough
        let at = find(&editor, "plain");
        select(&editor, at, at + 5);
        editor.toggle_style(Style::STRIKE);
        editor.toggle_numbers();
        assert!(
            editor.text().ends_with("1. ~~plain~~\n"),
            "{}",
            editor.text()
        );
        editor.toggle_quote();
        assert!(
            editor.text().ends_with("> ~~plain~~\n"),
            "{}",
            editor.text()
        );
        editor.toggle_tasks();
        assert!(
            editor.text().ends_with("- [ ] ~~plain~~\n"),
            "{}",
            editor.text()
        );
        check_cache(&editor);
        // --- and Enter draw a line (a blank line keeps it from making the
        // text above a heading); $$ and Enter open display math
        editor.place_cursor(editor.0.buffer.char_count());
        type_text(&editor, "\n\nabove\n---\n$$\nx^2");
        assert!(
            editor.text().ends_with("above\n\n---\n$$\nx^2\n$$\n"),
            "{:?}",
            editor.text()
        );
        let last = editor.line_count() - 1;
        assert_eq!(editor.0.view.block(last - 3), Block::Rule);
        assert_eq!(editor.0.view.block(last - 1), Block::Math);
        // joining a heading onto the line above leaves none of its look
        let at = find(&editor, "Three");
        editor.place_cursor(at);
        assert!(editor.backspace(), "the heading becomes text");
        let mut a = editor.0.buffer.iter_at_offset(at - 1);
        let mut b = editor.0.buffer.iter_at_offset(at);
        editor.0.buffer.delete_interactive(&mut a, &mut b, true);
        let joined = editor.0.buffer.iter_at_offset(find(&editor, "Three") + 2);
        assert!(!joined.has_tag(editor.0.tags.heading(3)));
        assert!(editor.text().contains("> moreThree\n"), "{}", editor.text());
        // it all reads back the same
        editor.save_sync();
        let saved = std::fs::read_to_string(dir.join("kinds.md")).unwrap();
        assert_eq!(markdown::serialize(&markdown::parse(&saved)), saved);
        assert_eq!(saved, editor.text());
        editor.close();

        // QA: undo, redo and paste rewrite whole lines; the line under them
        // keeps its kind (number, tick) as it was
        let open_text = |name: &str, text: &str| {
            let path = temp_dir(name).join(format!("{name}.md"));
            std::fs::write(&path, text).unwrap();
            editor.open(&path, false).unwrap();
        };
        let list = "Shopping\n1. milk\n2. eggs\n\nx\n1. \n- [x] \n3) [x] \n";
        open_text("renumber", list);
        editor.place_cursor(find(&editor, "Shopping") + 8);
        type_text(&editor, "!");
        editor.undo();
        assert_eq!(editor.text(), list);
        editor.redo();
        assert_eq!(editor.text(), list.replacen("Shopping", "Shopping!", 1));
        editor.undo();
        editor.place_cursor(find(&editor, "Shopping") + 8);
        editor.paste(" list\nfor Friday");
        assert!(
            editor.text().contains("for Friday\n1. milk\n"),
            "{}",
            editor.text()
        );
        editor.undo();
        editor.place_cursor(find(&editor, "x\n"));
        type_text(&editor, "y");
        editor.undo();
        assert_eq!(
            editor.text(),
            list,
            "empty items keep their number and tick"
        );
        // the highlight button writes `==` around the selection, and takes
        // it off again; formatting stays inside it
        editor.place_cursor(editor.0.buffer.char_count());
        type_text(&editor, "\n\nMake it pop");
        let at = find(&editor, "pop");
        select(&editor, at, at + 3);
        editor.toggle_highlight();
        assert!(
            editor.text().ends_with("\nMake it ==pop==\n"),
            "{}",
            editor.text()
        );
        assert!(editor.selected_highlight().is_some(), "still selected");
        assert!(
            editor
                .0
                .buffer
                .iter_at_offset(at + 2)
                .has_tag(&editor.0.tags.mark)
        );
        editor.toggle_highlight();
        assert!(
            editor.text().ends_with("\nMake it pop\n"),
            "{}",
            editor.text()
        );
        select(&editor, at, at + 3);
        editor.toggle_style(Style::BOLD);
        editor.toggle_highlight();
        assert!(
            editor.text().ends_with("\nMake it **==pop==**\n"),
            "{}",
            editor.text()
        );
        editor.undo();
        editor.undo();
        assert!(
            editor.text().ends_with("\nMake it pop\n"),
            "{}",
            editor.text()
        );
        // a leading zero stays text (escaped, or Markdown would count it)
        editor.place_cursor(editor.0.buffer.char_count());
        type_text(&editor, "\n007. bond");
        let last = editor.line_count() - 1;
        assert_eq!(editor.0.view.kind(last), LineKind::Text);
        assert!(
            editor.text().ends_with("\n007\\. bond\n"),
            "{}",
            editor.text()
        );
        editor.close();
        // a quote kept as written becomes an alert (or stops being one) as
        // its title is typed
        open_text("alert", "> [!WARN]\n> - item\n");
        editor.place_cursor(find(&editor, "WARN") + 4);
        type_text(&editor, "ING");
        assert_eq!(editor.0.view.shape(0).alert, Some(Alert::Warning));
        type_text(&editor, "S");
        assert_eq!(editor.0.view.shape(1).alert, None);
        editor.close();
        // display math broken by typing loses its fences' small type
        open_text("fences", "$$\nx^2\n$$\n");
        assert!(editor.line_start(0).has_tag(&editor.0.tags.fence_open));
        editor.place_cursor(2);
        type_text(&editor, "a");
        assert_eq!(editor.0.view.block(0), Block::None);
        assert!(!editor.line_start(0).has_tag(&editor.0.tags.fence_open));
        editor.close();
        // a space typed after a bare `>` survives saving and reopening
        open_text("bare", "> a\n>\n> b\n");
        editor.place_cursor(find(&editor, "a\n") + 2);
        type_text(&editor, "  two spaces");
        let saved = editor.text();
        open_text("bare2", &saved);
        assert_eq!(editor.line_model(1).text(), "  two spaces", "{saved:?}");
        editor.close();

        // what is drawn never changes the note: not on opening, nor as the
        // cursor walks past hidden syntax (a heading's ===, footnotes)
        {
            let src = "# Bottom\n\nSetext title\n============\nUnder it.\n\nInline $e^{i\\pi}+1=0$ and ==marked==, :tada:\n\n![a picture](pic.png)\n\n> [!TIP]\n> Alerts.\n\n---\n\n    code\n\nHard break at the end\\\nand a footnote[^1].\n\n[^1]: The footnote's own text.\n";
            let path = temp_dir("hidden").join("hidden.md");
            std::fs::write(&path, src).unwrap();
            editor.open(&path, false).unwrap();
            assert_eq!(editor.text(), src);
            for n in 0..editor.line_count() {
                editor.place_cursor(editor.line_start(n).offset());
                assert_eq!(editor.text(), src, "cursor on line {n}");
            }
            editor.close();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), src);
        }

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
        // moving through a long block only redraws what changes
        let code: String = std::iter::once("text\n\n".to_string())
            .chain((0..1500).map(|i| format!("    let x{i} = {i};\n")))
            .collect();
        let code_path = dir.join("code.md");
        std::fs::write(&code_path, &code).unwrap();
        editor.open(&code_path, true).unwrap();
        let t = Instant::now();
        for n in 0..50 {
            editor.place_cursor(editor.line_start(700 + n).offset() + 6);
        }
        let per_move = t.elapsed() / 50;
        eprintln!("1500 lines of indented code: per cursor move {per_move:?}");
        assert!(
            per_move < Duration::from_millis(20),
            "moving too slow: {per_move:?}"
        );
        assert_eq!(editor.text(), code);
        editor.close();
        editor.open(&big_path, true).unwrap();
        eprintln!("5000 lines: open {open:?}, per keystroke {per_key:?}, serialize {serialize:?}");
        assert!(
            per_key < Duration::from_millis(4),
            "typing too slow: {per_key:?}"
        );
        editor.close();
    }
}
