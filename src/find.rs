//! Find in the note (Ctrl+F): the bar, and where a word is in the text.
//! The editor draws the matches and moves between them.

use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib};

use crate::theme::{self, Icon};

/// Matches past this many are not listed (nor counted).
const MAX_MATCHES: usize = 9_999;

/// Where `query` is in `text`, ignoring case, as `[start, end)` char
/// offsets in order, never overlapping. Nothing for an empty query.
pub fn find_all(text: &str, query: &str) -> Vec<(usize, usize)> {
    let needle: Vec<char> = query.chars().map(fold).collect();
    let mut out = Vec::new();
    if needle.is_empty() {
        return out;
    }
    let hay: Vec<char> = text.chars().map(fold).collect();
    let mut i = 0;
    while i + needle.len() <= hay.len() && out.len() < MAX_MATCHES {
        if hay[i..i + needle.len()] == needle[..] {
            out.push((i, i + needle.len()));
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

/// One char to one char, so offsets in the folded text are offsets in the
/// note (`ß`, which lower-cases to itself, stays; `İ` becomes `i`).
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// The bar: a field, "3 of 12", previous, next and close.
pub struct FindBar {
    pub widget: gtk::Box,
    pub text: gtk::Text,
    count: gtk::Label,
    prev: gtk::Button,
    next: gtk::Button,
    pub close: gtk::Button,
}

impl FindBar {
    pub fn new() -> Rc<FindBar> {
        // room for 16 characters, but it gives way in a narrow window
        let text = gtk::Text::builder()
            .placeholder_text("Find in note")
            .hexpand(true)
            .valign(gtk::Align::Center)
            .width_chars(3)
            .max_width_chars(16)
            .build();
        text.set_input_hints(gtk::InputHints::NO_EMOJI);
        theme::no_builtin_popups(&text);
        // its text is padded to one width (the font is monospaced), so the
        // pill keeps its size; in a narrow window the field gives way
        let count = gtk::Label::builder()
            .css_classes(["count"])
            .xalign(1.0)
            .visible(false)
            .build();
        let button = |icon: &str, label: &str| {
            let button = gtk::Button::builder()
                .child(&Icon::new(icon, 14))
                .focus_on_click(false)
                .can_focus(false)
                .tooltip_text(label)
                .build();
            button.update_property(&[gtk::accessible::Property::Label(label)]);
            button.set_cursor_from_name(Some("pointer"));
            button
        };
        let prev = button(theme::ICON_UP, "Previous match (Shift+Enter)");
        let next = button(theme::ICON_DOWN, "Next match (Enter)");
        let close = button(theme::ICON_CLOSE, "Close (Esc)");

        let icon = Icon::new(theme::ICON_SEARCH, 14);
        icon.set_margin_end(6);
        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["find"])
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .visible(false)
            .build();
        widget.append(&icon);
        widget.append(&text);
        widget.append(&count);
        widget.append(&prev);
        widget.append(&next);
        widget.append(&close);
        // clicking anywhere on the pill goes to the field
        let click = gtk::GestureClick::new();
        let target = text.downgrade();
        click.connect_pressed(move |_, _, _, _| {
            if let Some(text) = target.upgrade() {
                text.grab_focus_without_selecting();
            }
        });
        widget.add_controller(click);

        let bar = Rc::new(FindBar {
            widget,
            text,
            count,
            prev,
            next,
            close,
        });
        bar.show_count(None, 0);
        bar
    }

    /// Enter goes to the next match and Shift+Enter (or Up) to the one
    /// before; `step` gets +1 or -1. The arrow buttons do the same.
    pub fn connect_step(&self, step: impl Fn(i32) + 'static) {
        let step = Rc::new(step);
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let on_key = step.clone();
        keys.connect_key_pressed(move |_, key, _, state| {
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            match key {
                gdk::Key::Return | gdk::Key::KP_Enter => on_key(if shift { -1 } else { 1 }),
                gdk::Key::Down => on_key(1),
                gdk::Key::Up => on_key(-1),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        self.text.add_controller(keys);
        let on_prev = step.clone();
        self.prev.connect_clicked(move |_| on_prev(-1));
        self.next.connect_clicked(move |_| step(1));
    }

    /// "3 of 12" for the `current` match of `total`, or "No matches".
    pub fn show_count(&self, current: Option<usize>, total: usize) {
        let typed = !self.text.text().is_empty();
        let count = &self.count;
        count.set_visible(typed);
        if total == 0 {
            count.set_text(&padded("No matches"));
            count.add_css_class("none");
        } else if current.is_none() {
            // none gone to yet (the last search, opened again)
            count.set_text(&padded(&format!("{total} found")));
            count.remove_css_class("none");
        } else {
            let at = current.map_or(0, |c| c + 1);
            let more = if total >= MAX_MATCHES { "+" } else { "" };
            // "1409/1418" when "1409 of 1418" would not fit its width
            let long = format!("{at} of {total}{more}");
            let text = if long.chars().count() <= 10 {
                long
            } else {
                format!("{at}/{total}{more}")
            };
            count.set_text(&padded(&text));
            count.remove_css_class("none");
        }
        self.prev.set_sensitive(total > 0);
        self.next.set_sensitive(total > 0);
    }
}

/// The count's text, padded on the left to the width of "No matches"
/// (figure spaces, as wide as a digit).
fn padded(text: &str) -> String {
    let pad = 10usize.saturating_sub(text.chars().count());
    format!("{}{text}", "\u{2007}".repeat(pad))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_words_ignoring_case() {
        assert_eq!(
            find_all("Paywall plan, pay later", "pay"),
            [(0, 3), (14, 17)]
        );
        assert_eq!(
            find_all("aaaa", "aa"),
            [(0, 2), (2, 4)],
            "never overlapping"
        );
        assert_eq!(find_all("Öl och öl", "ÖL"), [(0, 2), (7, 9)]);
        assert_eq!(find_all("x **bold** y", "bold"), [(4, 8)]);
        assert!(find_all("anything", "").is_empty());
        assert!(find_all("short", "longer than it").is_empty());
        // offsets are chars, not bytes
        assert_eq!(find_all("😀 smile", "smile"), [(2, 7)]);
    }
}
