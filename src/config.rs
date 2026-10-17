//! `~/.config/kludd/config.toml`: where notes live, the theme and the editor zoom.

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
}

impl Default for Config {
    fn default() -> Self {
        Config {
            notes_dir: "~/Kludd".into(),
            theme: "pink".into(),
            zoom: 1.0,
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
        assert_eq!(config.zoom, 1.0);
    }

    #[test]
    fn round_trips() {
        let config = Config {
            notes_dir: "/tmp/notes".into(),
            theme: "dark".into(),
            zoom: 1.25,
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
}
