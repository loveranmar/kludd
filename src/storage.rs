//! Notes on disk: one `.md` file per note, nothing else.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gtk::{gio, glib, prelude::*};

use crate::markdown;

#[derive(Clone, Debug)]
pub struct Note {
    pub path: PathBuf,
    pub title: String,
    pub modified: SystemTime,
    len: u64,
    /// Lower-cased title and content, for search.
    haystack: String,
}

impl Note {
    pub fn matches(&self, query: &str) -> bool {
        query.is_empty() || self.haystack.contains(query)
    }
}

pub fn is_note(path: &Path) -> bool {
    let hidden = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_none_or(|n| n.starts_with('.'));
    !hidden
        && path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("md"))
}

/// Lists the notes in `dir`, newest first. Files whose size and mtime did
/// not change since `previous` are not read again.
pub fn scan(dir: &Path, previous: &[Note]) -> Vec<Note> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut notes: Vec<Note> = entries
        .flatten()
        .filter(|e| is_note(&e.path()))
        .filter_map(|entry| {
            let path = entry.path();
            let meta = std::fs::metadata(&path).ok()?;
            if !meta.is_file() {
                return None;
            }
            let modified = meta.modified().unwrap_or(UNIX_EPOCH);
            if let Some(old) = previous.iter().find(|n| n.path == path)
                && old.modified == modified
                && old.len == meta.len()
            {
                return Some(old.clone());
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            Some(note_from(path, &text, modified, meta.len()))
        })
        .collect();
    notes.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.title.cmp(&b.title))
    });
    notes
}

fn note_from(path: PathBuf, text: &str, modified: SystemTime, len: u64) -> Note {
    let title = markdown::title(text)
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| stem(&path));
    let haystack = format!("{title}\n{text}").to_lowercase();
    Note {
        path,
        title,
        modified,
        len,
        haystack,
    }
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `Things to fix` -> `things-to-fix`.
pub fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut gap = false;
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            if gap && !slug.is_empty() {
                slug.push('-');
            }
            gap = false;
            slug.push(c);
        } else if c != '\'' && c != '’' {
            gap = true;
        }
        if slug.len() >= 80 {
            break;
        }
    }
    if slug.is_empty() {
        "untitled".into()
    } else {
        slug
    }
}

/// `dir/slug.md`, or `dir/slug-2.md`, `-3`, ... if taken by another file.
pub fn unique_path(dir: &Path, slug: &str, current: Option<&Path>) -> PathBuf {
    let mut n = 1;
    loop {
        let name = if n == 1 {
            format!("{slug}.md")
        } else {
            format!("{slug}-{n}.md")
        };
        let candidate = dir.join(name);
        if current == Some(candidate.as_path()) || !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// "today 16:12", "yesterday", "12 Oct", "12 Oct 2025".
pub fn relative_date(modified: SystemTime) -> String {
    let secs = modified
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (Ok(then), Ok(now)) = (
        glib::DateTime::from_unix_local(secs),
        glib::DateTime::now_local(),
    ) else {
        return String::new();
    };
    let same_day = |a: &glib::DateTime, b: &glib::DateTime| a.ymd() == b.ymd();
    if same_day(&then, &now) {
        return format!("today {:02}:{:02}", then.hour(), then.minute());
    }
    if now.add_days(-1).is_ok_and(|y| same_day(&then, &y)) {
        return "yesterday".into();
    }
    let month = MONTHS[(then.month() as usize).clamp(1, 12) - 1];
    if then.year() == now.year() {
        format!("{} {month}", then.day_of_month())
    } else {
        format!("{} {month} {}", then.day_of_month(), then.year())
    }
}

/// Moves a note to the desktop trash (like `gio trash`); never deletes.
pub fn trash(path: &Path) -> Result<(), glib::Error> {
    gio::File::for_path(path).trash(gio::Cancellable::NONE)
}

/// Where writes really go: symlinks are followed, so saving a symlinked
/// note updates its target instead of replacing the link.
pub fn write_target(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Synchronous atomic write (temp file + rename), keeping the file mode.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let target = write_target(path);
    let dir = target.parent().unwrap_or(Path::new("."));
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.kludd-tmp"));
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        if let Ok(meta) = std::fs::metadata(&target) {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        std::fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slugify("Things to fix"), "things-to-fix");
        assert_eq!(slugify("  Vocab week 42!  "), "vocab-week-42");
        assert_eq!(slugify("It's Ågren's list"), "its-ågrens-list");
        assert_eq!(slugify("***"), "untitled");
        assert_eq!(slugify(""), "untitled");
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kludd-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn unique_paths_avoid_collisions() {
        let dir = temp_dir("unique");
        assert_eq!(unique_path(&dir, "a", None), dir.join("a.md"));
        std::fs::write(dir.join("a.md"), "").unwrap();
        assert_eq!(unique_path(&dir, "a", None), dir.join("a-2.md"));
        assert_eq!(
            unique_path(&dir, "a", Some(&dir.join("a.md"))),
            dir.join("a.md")
        );
        std::fs::write(dir.join("a-2.md"), "").unwrap();
        assert_eq!(unique_path(&dir, "a", None), dir.join("a-3.md"));
        assert_eq!(
            unique_path(&dir, "a", Some(&dir.join("a-2.md"))),
            dir.join("a-2.md")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scan_reads_titles_and_skips_others() {
        let dir = temp_dir("scan");
        std::fs::write(dir.join("one.md"), "# First note\nhello").unwrap();
        std::fs::write(dir.join("two.md"), "no heading here").unwrap();
        std::fs::write(dir.join(".hidden.md"), "# hidden").unwrap();
        std::fs::write(dir.join("other.txt"), "# nope").unwrap();
        let notes = scan(&dir, &[]);
        let mut titles: Vec<_> = notes.iter().map(|n| n.title.as_str()).collect();
        titles.sort();
        assert_eq!(titles, ["First note", "two"]);
        assert!(notes.iter().any(|n| n.matches("hello")));
        assert!(!notes.iter().any(|n| n.matches("nope")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn atomic_write_keeps_symlinks() {
        let dir = temp_dir("write");
        let real = dir.join("real.md");
        std::fs::write(&real, "old").unwrap();
        let link = dir.join("link.md");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_atomic(&link, "new").unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
