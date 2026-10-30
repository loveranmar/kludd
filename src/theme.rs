//! Colour themes, the stylesheet and the tiny stroke-icon widget.
//!
//! Colours come from three values per theme: `@paper` (background), `@ink`
//! (text and accents) and `@accent` (the selection). The base stylesheet only
//! refers to those names; switching theme regenerates the small provider
//! that defines them.

use std::cell::{Cell, RefCell};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, gsk};

pub struct Theme {
    pub name: &'static str,
    pub paper: &'static str,
    pub ink: &'static str,
    /// Selection highlight: a saturated take on the paper colour, clearly
    /// visible behind ink text.
    pub accent: &'static str,
}

pub const THEMES: [Theme; 6] = [
    Theme {
        name: "pink",
        paper: "#FFD6E5",
        ink: "#4A1F30",
        accent: "#F79BC0",
    },
    Theme {
        name: "yellow",
        paper: "#FFF3B0",
        ink: "#3A3320",
        accent: "#F2D45C",
    },
    Theme {
        name: "mint",
        paper: "#D4F5E4",
        ink: "#173A2A",
        accent: "#8EDDB5",
    },
    Theme {
        name: "lavender",
        paper: "#E4DCFF",
        ink: "#2A2350",
        accent: "#B4A3FF",
    },
    Theme {
        name: "sky",
        paper: "#D6ECFF",
        ink: "#14304D",
        accent: "#94C7FF",
    },
    Theme {
        name: "dark",
        paper: "#1E1E24",
        ink: "#ECEAF2",
        accent: "#5B5BD6",
    },
];

impl Theme {
    pub fn accent_rgba(&self) -> gdk::RGBA {
        gdk::RGBA::parse(self.accent).unwrap_or(gdk::RGBA::BLACK)
    }
}

pub fn by_name(name: &str) -> &'static Theme {
    THEMES.iter().find(|t| t.name == name).unwrap_or(&THEMES[0])
}

/// Above GTK_STYLE_PROVIDER_PRIORITY_USER (800), so neither the system GTK
/// theme nor ~/.config/gtk-4.0/gtk.css can restyle Kludd.
const PRIORITY: u32 = 900;

const BASE_CSS: &str = r#"
/* Kludd draws everything itself: start every widget from nothing so the
   system GTK theme cannot leak in (this provider outranks it). */
* { all: unset; }

window.kludd {
  background: none;
  box-shadow: none;
  border: none;
  color: @ink;
  font-family: "Fredoka";
  font-size: 16px;
  font-weight: 400;
}
.frame {
  background-color: @paper;
  border: 2px solid alpha(@ink, 0.22);
  border-radius: 22px;
}

/* status bar */
.statusbar {
  padding: 6px 20px;
  background-color: alpha(@ink, 0.09);
  font-family: "JetBrains Mono";
  font-size: 11px;
}
/* browser line boxes, whole pixels: 11px mono is 14px tall, and 17px in
   the editor where the reference inherits line-height 1.55 */
.statusbar label { line-height: 14px; }
.statusbar.tall label { line-height: 17px; }

/* home view */
.home { padding: 18px 20px 0 20px; }
.home .head { margin-bottom: 12px; }
.home .title { font-size: 20px; font-weight: 500; line-height: 24px; }
.circle {
  min-width: 30px;
  min-height: 30px;
  border-radius: 15px;
}
.circle.soft { background-color: alpha(@ink, 0.07); color: @ink; }
.circle.primary { background-color: @ink; color: @paper; }
.search {
  background-color: alpha(@ink, 0.07);
  border-radius: 12px;
  padding: 6px 12px;
  font-size: 13px;
  opacity: 0.75;
  margin-bottom: 12px;
}
/* the reference's icon sits 1px below centre (vertical-align: -2px) */
.search-icon { margin-top: 2px; }
.search text {
  color: @ink;
  caret-color: @ink;
}
.search text > placeholder { color: @ink; opacity: 1; }
.search text selection { background-color: @accent; color: @ink; }
.card {
  background-color: alpha(@ink, 0.07);
  border-radius: 14px;
  padding: 10px 12px;
}
/* 19.6 + 2 + 15.4 in the reference; GTK rounds line boxes, so 20 + 1 + 16 */
.card .t { font-size: 14px; font-weight: 500; line-height: 19.6px; margin-bottom: 1px; }
.card .d { font-size: 11px; opacity: 0.7; line-height: 16px; }
.empty { font-size: 13px; opacity: 0.6; margin-top: 24px; }
/* room under the last card so it never touches the status bar */
.grid { margin-bottom: 12px; }
/* added folders: rows of their own above the notes, one gap apart */
.folders { margin-bottom: 8px; }

/* editor view */
.editor-view { padding: 20px 24px 0 24px; }
.notch {
  min-width: 70px;
  min-height: 11px;
  border-radius: 0 0 8px 8px;
  background-color: alpha(@ink, 0.16);
}
.back { font-size: 13px; opacity: 0.7; margin-bottom: 4px; }
.back label { line-height: 20px; } /* 20.15 in the reference; GTK rounds up */
.back-icon { margin-top: 2px; }
textview.editor, textview.editor > text {
  background: none;
  color: @ink;
  caret-color: @ink;
  font-family: "Fredoka";
}
textview.editor > text selection { background-color: transparent; color: @ink; }

/* scrollbars */
window.kludd scrollbar { background: none; border: none; }
window.kludd scrollbar slider {
  min-width: 4px;
  min-height: 24px;
  border: none;
  border-radius: 4px;
  background-color: alpha(@ink, 0.25);
}
window.kludd undershoot, window.kludd overshoot { background: none; }

/* popovers: palette, format toolbar, card menu */
popover.kludd {
  background: none;
  box-shadow: none;
  border: none;
  padding: 0;
  margin: 0;
  color: @paper;
  font-family: "Fredoka";
  font-weight: 400;
}
popover.kludd > contents {
  background-color: @ink;
  color: @paper;
  border: none;
  box-shadow: none;
  margin: 0;
}
popover.palette > contents { border-radius: 14px; padding: 8px; }
.swatch {
  min-width: 24px;
  min-height: 24px;
  border-radius: 13px;
  border: 1px solid rgba(0, 0, 0, 0.15);
}
.swatch.on { box-shadow: 0 0 0 2px @ink, 0 0 0 4px @paper; }
.swatch.pink { background-color: #FFD6E5; }
.swatch.yellow { background-color: #FFF3B0; }
.swatch.mint { background-color: #D4F5E4; }
.swatch.lavender { background-color: #E4DCFF; }
.swatch.sky { background-color: #D6ECFF; }
.swatch.dark { background-color: #1E1E24; }
popover.toolbar > contents {
  border-radius: 10px;
  padding: 4px 6px;
  font-size: 13px;
}
popover.toolbar button { padding: 0 6px; border-radius: 6px; }
popover.toolbar button label { line-height: 20.15px; }
popover.toolbar button:hover { background-color: alpha(@paper, 0.15); }
popover.toolbar button.on { background-color: alpha(@paper, 0.25); }
popover.toolbar button.on:hover { background-color: alpha(@paper, 0.32); }
popover.toolbar .b { font-weight: 500; }
/* card menu: a folder's colours, above "Remove from Kludd"; round, and
   kept in their colour when hovered, unlike the menu's other buttons */
popover.toolbar .tints { margin: 6px 4px 8px 4px; }
popover.toolbar button.swatch { padding: 0; border-radius: 13px; }
popover.toolbar button.swatch.tint.none { background-color: @paper; }
popover.toolbar .i { font-style: italic; }
popover.toolbar .u label { text-decoration-line: underline; }
popover.toolbar .s label { text-decoration-line: line-through; }
popover.toolbar button.icon { padding: 0 5px; }
"#;

/// Owns the CSS providers for one display.
pub struct Styles {
    dynamic: gtk::CssProvider,
}

impl Styles {
    pub fn install(display: &gdk::Display, theme: &Theme, zoom: f64) -> Styles {
        let base = gtk::CssProvider::new();
        base.load_from_string(&format!("{BASE_CSS}{}", folder_css()));
        gtk::style_context_add_provider_for_display(display, &base, PRIORITY);
        let dynamic = gtk::CssProvider::new();
        gtk::style_context_add_provider_for_display(display, &dynamic, PRIORITY);
        let styles = Styles { dynamic };
        styles.update(theme, zoom);
        styles
    }

    /// Regenerates the colour (and editor size) definitions.
    pub fn update(&self, theme: &Theme, zoom: f64) {
        self.dynamic.load_from_string(&format!(
            "@define-color paper {};\n@define-color ink {};\n@define-color accent {};\n\
             textview.editor, textview.editor > text {{ font-size: {:.2}px; }}\n",
            theme.paper,
            theme.ink,
            theme.accent,
            15.0 * zoom
        ));
    }
}

/// A coloured folder card is a theme's accent with that theme's ink on top,
/// so it reads the same on any paper; the dots in the card menu show it.
fn folder_css() -> String {
    THEMES
        .iter()
        .map(|t| {
            format!(
                ".card.folder.{0} {{ background-color: {1}; color: {2}; }}\n\
                 popover.toolbar button.swatch.tint.{0} {{ background-color: {1}; }}\n",
                t.name, t.accent, t.ink
            )
        })
        .collect()
}

/// Turns off GTK's own right-click menu and emoji picker on a text widget.
/// Both are popovers that grab all input, and with the reset stylesheet
/// they draw without a background, so they read as a frozen window.
pub fn no_builtin_popups(widget: &impl IsA<gtk::Widget>) {
    let widget = widget.as_ref();
    let block = gtk::GestureClick::builder()
        .button(gdk::BUTTON_SECONDARY)
        .propagation_phase(gtk::PropagationPhase::Capture)
        .build();
    block.connect_pressed(|gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });
    widget.add_controller(block);
    widget.action_set_enabled("menu.popup", false);
    widget.action_set_enabled("misc.insert-emoji", false);
}

// ---------------------------------------------------------------------------
// Icon: Tabler outline icons, stroked with the widget's CSS `color`.

pub const ICON_PLUS: &str = include_str!("../assets/icons/plus.svg");
pub const ICON_SEARCH: &str = include_str!("../assets/icons/search.svg");
pub const ICON_ARROW_LEFT: &str = include_str!("../assets/icons/arrow-left.svg");
pub const ICON_PALETTE: &str = include_str!("../assets/icons/palette.svg");
pub const ICON_FOLDER: &str = include_str!("../assets/icons/folder.svg");
pub const ICON_FOLDER_PLUS: &str = include_str!("../assets/icons/folder-plus.svg");
pub const ICON_TASK: &str = include_str!("../assets/icons/square-check.svg");
pub const ICON_QUOTE: &str = include_str!("../assets/icons/quote.svg");
pub const ICON_HIGHLIGHT: &str = include_str!("../assets/icons/highlight.svg");
// GitHub's alerts: note, tip, important, warning, caution
pub const ICON_NOTE: &str = include_str!("../assets/icons/info-circle.svg");
pub const ICON_TIP: &str = include_str!("../assets/icons/bulb.svg");
pub const ICON_IMPORTANT: &str = include_str!("../assets/icons/message-report.svg");
pub const ICON_WARNING: &str = include_str!("../assets/icons/alert-triangle.svg");
pub const ICON_CAUTION: &str = include_str!("../assets/icons/alert-octagon.svg");

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Icon {
        pub paths: RefCell<Vec<gsk::Path>>,
        pub size: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Icon {
        const NAME: &'static str = "KluddIcon";
        type Type = super::Icon;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Icon {}

    impl WidgetImpl for Icon {
        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let size = self.size.get();
            (size, size, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let color = widget.color();
            let size = self.size.get() as f32;
            let stroke = gsk::Stroke::new(2.0);
            stroke.set_line_cap(gsk::LineCap::Round);
            stroke.set_line_join(gsk::LineJoin::Round);
            snapshot.save();
            snapshot.scale(size / 24.0, size / 24.0);
            for path in self.paths.borrow().iter() {
                snapshot.append_stroke(path, &stroke, &color);
            }
            snapshot.restore();
        }
    }
}

glib::wrapper! {
    pub struct Icon(ObjectSubclass<imp::Icon>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Icon {
    /// `svg` is one of the bundled Tabler files; `size` is in pixels.
    pub fn new(svg: &str, size: i32) -> Icon {
        let icon: Icon = glib::Object::new();
        icon.imp().size.set(size);
        icon.imp().paths.replace(icon_paths(svg));
        icon.set_valign(gtk::Align::Center);
        icon.set_halign(gtk::Align::Center);
        icon
    }
}

/// An icon's strokes, on a 24x24 grid.
pub fn icon_paths(svg: &str) -> Vec<gsk::Path> {
    svg_paths(svg)
        .filter_map(|d| gsk::Path::parse(d).ok())
        .collect()
}

/// The `d` attribute of every stroked `<path>` (Tabler adds an invisible
/// 24x24 bounding box path first, which is skipped).
fn svg_paths(svg: &str) -> impl Iterator<Item = &str> {
    svg.split("<path").skip(1).filter_map(|element| {
        if element.contains("stroke=\"none\"") {
            return None;
        }
        let start = element.find(" d=\"")? + 4;
        let len = element[start..].find('"')?;
        Some(&element[start..start + len])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_have_paths() {
        assert_eq!(
            svg_paths(ICON_PLUS).collect::<Vec<_>>(),
            ["M12 5l0 14", "M5 12l14 0"]
        );
        assert_eq!(svg_paths(ICON_SEARCH).count(), 2);
        assert_eq!(svg_paths(ICON_ARROW_LEFT).count(), 3);
        assert_eq!(svg_paths(ICON_PALETTE).count(), 4);
        assert_eq!(svg_paths(ICON_FOLDER).count(), 1);
        assert_eq!(svg_paths(ICON_FOLDER_PLUS).count(), 3);
        for icon in [
            ICON_TASK,
            ICON_QUOTE,
            ICON_HIGHLIGHT,
            ICON_NOTE,
            ICON_TIP,
            ICON_IMPORTANT,
            ICON_WARNING,
            ICON_CAUTION,
        ] {
            assert!(!icon_paths(icon).is_empty());
        }
    }

    #[test]
    fn folder_colours_follow_the_themes() {
        let css = folder_css();
        assert!(css.contains(".card.folder.mint { background-color: #8EDDB5; color: #173A2A; }"));
        assert_eq!(css.matches(".swatch.tint.").count(), THEMES.len());
    }

    #[test]
    fn unknown_theme_falls_back_to_pink() {
        assert_eq!(by_name("nope").name, "pink");
        assert_eq!(by_name("dark").paper, "#1E1E24");
    }
}
