//! Kludd: a dead-fast, cute Markdown notepad for Hyprland.

mod app;
mod config;
mod editor;
mod home;
mod markdown;
mod storage;
mod theme;

fn main() -> gtk::glib::ExitCode {
    app::run()
}
