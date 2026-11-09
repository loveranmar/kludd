//! Kludd: a dead-fast, cute Markdown notepad for Hyprland.

mod app;
mod config;
mod editor;
mod find;
mod home;
mod markdown;
mod scroll;
mod stats;
mod storage;
mod theme;

fn main() -> gtk::glib::ExitCode {
    app::run()
}
