//! Word count (Ctrl+Shift+C): a small menu with the note's words and
//! characters, and the "x words" counter it can switch on while typing.

use std::rc::Rc;

use gtk::prelude::*;

use crate::markdown;

/// Words and characters (spaces included, line breaks not) in what a note
/// shows: Markdown is counted as read, without its syntax; plain text as
/// it is. A word is a run of non-space text with a letter or digit in it,
/// so a lone `—` or `|` is none.
pub fn count(text: &str, is_markdown: bool) -> (usize, usize) {
    let shown;
    let text = if is_markdown {
        shown = without_highlight_marks(&markdown::searchable(text));
        shown.as_str()
    } else {
        text
    };
    let words = text
        .split_whitespace()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .count();
    let chars = text.chars().filter(|&c| c != '\n' && c != '\r').count();
    (words, chars)
}

/// `==highlights==` read without their marks (the Markdown parser keeps
/// them): on each line, every pair of `==` goes.
fn without_highlight_marks(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            let pairs = line.matches("==").count() / 2;
            line.replacen("==", "", pairs * 2)
        })
        .collect()
}

/// `1234567` -> `1 234 567`, grouped with thin spaces.
pub fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('\u{2009}');
        }
        out.push(c);
    }
    out
}

/// "1 word", "12 words".
pub fn words_label(words: usize) -> String {
    let unit = if words == 1 { "word" } else { "words" };
    format!("{} {unit}", grouped(words))
}

pub struct WordCount {
    /// The "x words" pill in the editor's bottom left corner.
    pub counter: gtk::Button,
    pub menu: gtk::Popover,
    words: gtk::Label,
    chars: gtk::Label,
    pub switch: gtk::Switch,
}

impl WordCount {
    pub fn new() -> Rc<WordCount> {
        let counter = gtk::Button::builder()
            .css_classes(["word-count"])
            .halign(gtk::Align::Start)
            .valign(gtk::Align::End)
            .focus_on_click(false)
            .can_focus(false)
            .visible(false)
            .tooltip_text("Word count (Ctrl+Shift+C)")
            .build();
        counter.set_cursor_from_name(Some("pointer"));

        let stat = |caption: &str| {
            let value = gtk::Label::builder()
                .css_classes(["value"])
                .xalign(0.0)
                .build();
            let label = gtk::Label::builder()
                .label(caption)
                .css_classes(["caption"])
                .xalign(0.0)
                .build();
            let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
            column.set_hexpand(true);
            column.append(&value);
            column.append(&label);
            (column, value)
        };
        let (words_box, words) = stat("words");
        let (chars_box, chars) = stat("characters, with spaces");
        let numbers = gtk::Box::new(gtk::Orientation::Horizontal, 18);
        numbers.append(&words_box);
        numbers.append(&chars_box);

        let switch = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .focus_on_click(false)
            .build();
        switch.set_cursor_from_name(Some("pointer"));
        let caption = gtk::Label::builder()
            .label("Display word count while typing")
            .xalign(0.0)
            .hexpand(true)
            .build();
        let toggle = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        toggle.add_css_class("toggle");
        toggle.append(&caption);
        toggle.append(&switch);
        // the whole row flips the switch
        let click = gtk::GestureClick::new();
        let target = switch.downgrade();
        click.connect_released(move |gesture, _, x, y| {
            let Some(switch) = target.upgrade() else {
                return;
            };
            let on_switch = gesture
                .widget()
                .and_then(|row| row.pick(x, y, gtk::PickFlags::DEFAULT))
                .is_some_and(|w| w == switch || w.is_ancestor(&switch));
            if !on_switch {
                switch.set_active(!switch.is_active());
            }
        });
        toggle.add_controller(click);
        toggle.set_cursor_from_name(Some("pointer"));

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&numbers);
        content.append(&toggle);

        // never grabs (see `home::popover`): Esc and clicks elsewhere close it
        let menu = gtk::Popover::builder()
            .has_arrow(false)
            .autohide(false)
            .position(gtk::PositionType::Top)
            .css_classes(["kludd", "stats"])
            .child(&content)
            .build();

        Rc::new(WordCount {
            counter,
            menu,
            words,
            chars,
            switch,
        })
    }

    /// Shows the counts of `text` in the menu and the counter.
    pub fn show(&self, (words, chars): (usize, usize)) {
        self.words.set_text(&grouped(words));
        self.chars.set_text(&grouped(chars));
        self.counter.set_label(&words_label(words));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_what_is_read() {
        assert_eq!(
            count("# Hello world\n\nOne **two** three.\n", true),
            (5, 25)
        );
        // syntax and lone marks are no words; plain text counts as it is
        assert_eq!(count("- [x] done — [link](https://a.b)\n", true).0, 2);
        assert_eq!(count("**not** markdown\n", false), (2, 16));
        assert_eq!(count("", true), (0, 0));
        // prices are words, not math to leave out
        let prices = "- commit to **$7/week** but not to **$38/year**, even so\n";
        assert_eq!(count(prices, true), (9, 46));
        assert_eq!(count("==Start== here, a == b\n", true), (4, 18));
        // nested list items are words of their own
        let nested = "# Nest\n\n- one\n  - two\n    - three\n- four\n\n1. alpha\n   - beta\n";
        assert_eq!(count(nested, true), (7, 28));
    }

    #[test]
    fn numbers_read_easily() {
        assert_eq!(grouped(7), "7");
        assert_eq!(grouped(1234), "1\u{2009}234");
        assert_eq!(grouped(1234567), "1\u{2009}234\u{2009}567");
        assert_eq!(words_label(1), "1 word");
        assert_eq!(words_label(0), "0 words");
    }
}
