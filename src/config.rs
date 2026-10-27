//! `~/.config/kludd/config.toml`: where notes live, the added folders, the
//! theme and the editor zoom.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const MIN_ZOOM: f64 = 0.6;
pub const MAX_ZOOM: f64 = 2.5;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Folder that holds the notes. `~` expands to the home directory.
    pub notes_dir: String,
    /// One of the theme names in `theme::THEMES`.
    pub theme: String,
    /// Editor zoom factor, changed with Ctrl +/-.
    pub zoom: f64,
    /// Other folders whose notes Kludd lists, each in its own view. Last:
    /// TOML writes them as `[[folders]]` tables, after the plain values.
    pub folders: Vec<Folder>,
}

/// An added folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "FolderEntry")]
pub struct Folder {
    /// `~` expands to the home directory.
    pub path: String,
    /// One of the theme names in `theme::THEMES`: its card takes that
    /// theme's accent. `None` is a plain card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

impl Folder {
    pub fn new(path: String) -> Folder {
        Folder { path, color: None }
    }

    pub fn dir(&self) -> PathBuf {
        expand_tilde(&self.path)
    }
}

/// A folder as written in the file: a table, or just its path.
#[derive(Deserialize)]
#[serde(untagged)]
enum FolderEntry {
    Path(String),
    Table {
        path: String,
        #[serde(default)]
        color: Option<String>,
    },
}

impl From<FolderEntry> for Folder {
    fn from(entry: FolderEntry) -> Folder {
        match entry {
            FolderEntry::Path(path) => Folder::new(path),
            FolderEntry::Table { path, color } => Folder { path, color },
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            notes_dir: "~/Kludd".into(),
            theme: "pink".into(),
            zoom: 1.0,
            folders: Vec::new(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        config_dir().join("kludd").join("config.toml")
    }

    /// Loads the config, falling back to defaults for a missing or broken file.
    pub fn load() -> Config {
        let mut config: Config = std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default();
        if !config.zoom.is_finite() {
            config.zoom = 1.0;
        }
        config.zoom = config.zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        config
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)
    }

    pub fn notes_dir(&self) -> PathBuf {
        expand_tilde(&self.notes_dir)
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home_dir().join(".config"))
}

pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        home_dir()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(path)
    }
}

/// A path as typed: `~` is the home directory, and so is the start of a
/// relative path (Kludd has no working directory worth meaning).
/// `~/notes/` -> `/home/me/notes`.
pub fn typed_path(text: &str) -> PathBuf {
    let path = home_dir().join(expand_tilde(text.trim()));
    path.components().collect()
}

/// `/home/me/Kludd/a.md` -> `~/Kludd/a.md`, for the status bar.
pub fn tilde_path(path: &Path) -> String {
    let home = home_dir();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_partial_config() {
        let config: Config = toml::from_str("theme = \"mint\"").unwrap();
        assert_eq!(config.theme, "mint");
        assert_eq!(config.notes_dir, "~/Kludd");
        assert!(config.folders.is_empty());
        assert_eq!(config.zoom, 1.0);
    }

    #[test]
    fn round_trips() {
        let config = Config {
            notes_dir: "/tmp/notes".into(),
            theme: "dark".into(),
            zoom: 1.25,
            folders: vec![
                Folder::new("~/code/app/docs".into()),
                Folder {
                    path: "/srv/notes".into(),
                    color: Some("mint".into()),
                },
            ],
        };
        let text = toml::to_string(&config).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), config);
    }

    #[test]
    fn tilde() {
        let home = home_dir();
        assert_eq!(expand_tilde("~/Kludd"), home.join("Kludd"));
        assert_eq!(tilde_path(&home.join("Kludd")), "~/Kludd");
        assert_eq!(tilde_path(Path::new("/etc/x.md")), "/etc/x.md");
    }

    #[test]
    fn folders_as_paths_or_tables() {
        let config: Config = toml::from_str(
            "theme = \"sky\"\nfolders = [\"~/a\", { path = \"/b\", color = \"mint\" }]",
        )
        .unwrap();
        assert_eq!(config.theme, "sky");
        assert_eq!(
            config.folders,
            [
                Folder::new("~/a".into()),
                Folder {
                    path: "/b".into(),
                    color: Some("mint".into())
                }
            ]
        );
        assert_eq!(config.folders[0].dir(), home_dir().join("a"));
        let text = toml::to_string(&config).unwrap();
        assert!(text.contains("[[folders]]\npath = \"~/a\"\n"), "{text}");
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), config);
    }

    #[test]
    fn typed_paths() {
        let home = home_dir();
        assert_eq!(typed_path(" ~/code/docs/ "), home.join("code/docs"));
        assert_eq!(typed_path("code/./docs"), home.join("code/docs"));
        assert_eq!(typed_path("~"), home);
        assert_eq!(typed_path("/srv/notes/"), PathBuf::from("/srv/notes"));
    }
}
