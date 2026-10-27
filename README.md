<h1 align="center">Kludd</h1>

<p align="center">
  <strong>A dead-fast, cute Markdown notepad that lives one keypress away in Hyprland.</strong>
</p>

<p align="center">
  <img src="design/screenshots.png" alt="Kludd: the notes grid and the editor" width="800" />
</p>

---

## What is Kludd?

Kludd starts once when you log in and waits in the background, so pressing <kbd>Home</kbd> shows it right away: no startup, no loading. You write in rich text (bold, lists, headings) and every note is saved as a plain `.md` file in `~/Kludd`. Markdown that Kludd doesn't understand, like code blocks, tables and links, is kept exactly as you wrote it.

## Features

- **Instant** — Kludd keeps running in the background; the hotkey just shows its window
- **Plain files** — one note is one `.md` file named after its title; no database, no index, no frontmatter
- **Rich text, real Markdown** — bold, italic, underline, bullets and headings while you type, saved as normal Markdown
- **Never mangles a note** — anything unsupported is shown as plain text and written back byte for byte (round-trip tested)
- **Autosave** — 400 ms after you stop typing and whenever Kludd hides, written atomically
- **Live reload** — edit a note in nvim and Kludd picks up the change
- **Any `.md` or `.txt` file** — `kludd notes.md` edits a file in place, and Kludd shows up in "Open with"
- **Folders** — add any folder, like a project's `docs/notes`, and its notes get a view of their own; new notes can start right there
- **Plain text stays plain** — a `.txt` file shows exactly what it holds: no formatting, saved exactly as typed
- **Six themes** — pink, yellow, mint, lavender, sky and dark, switched live from the palette button
- **Big notes stay smooth** — in the test suite a 5 000-line note loads in about 50 ms and a keystroke costs about 0.1 ms

## Install

### From source

```bash
# GTK 4, the status-bar font and a Rust toolchain (rustup works too)
sudo pacman -S --needed gtk4 ttf-jetbrains-mono rust

git clone https://github.com/loveranmar/kludd.git
cd kludd
./install.sh
```

`install.sh` builds a release binary and installs it into `~/.local` together with the desktop entry, the icon and the Fredoka font. `./install.sh --uninstall` removes it again and leaves your notes alone.

### Arch (AUR)

Kludd is not on the AUR yet. The PKGBUILD lives in [`packaging/`](packaging/PKGBUILD); once it is published, installing is `yay -S kludd`.

## Hyprland setup

Add this to your Hyprland config (block window rules, Hyprland 0.53 and newer):

```ini
# Start Kludd once, in the background
exec-once = kludd --hidden

# Home shows Kludd, or hides it when it is in front
bind = , Home, exec, kludd --toggle

# Kludd draws its own rounded border, so Hyprland's is switched off
windowrule {
    name = kludd
    match:class = ^(io\.github\.loveranmar\.Kludd)$
    float = true
    center = true
    size = 560 460
    border_size = 0
    rounding = 0
    no_shadow = true
    no_blur = true
    no_anim = true
    opacity = 1.0 override 1.0 override
}
```

Kludd keeps running in the background with its window hidden, so the hotkey only has to show it. Closing the window (say, your `killactive` bind) hides it again; `kludd --quit` quits for real. If you used `install.sh`, `~/.local/bin` may not be on Hyprland's `PATH`; then write the full path, `/home/you/.local/bin/kludd`.

To open `.md` and `.txt` files with Kludd by default, run `xdg-mime default kludd.desktop text/markdown text/plain`.

## Keyboard shortcuts

| Keys | Action |
|------|--------|
| <kbd>Home</kbd> | Show Kludd, or hide it (the Hyprland bind above) |
| <kbd>Ctrl</kbd>+<kbd>N</kbd> | New note (in the folder on show) |
| <kbd>Ctrl</kbd>+<kbd>F</kbd> | Search notes (or just start typing on the notes view) |
| <kbd>Esc</kbd> | Back to the notes, out of a folder, or hide Kludd |
| <kbd>Ctrl</kbd>+<kbd>B</kbd> / <kbd>I</kbd> / <kbd>U</kbd> | Bold / italic / underline |
| <kbd>Ctrl</kbd>+<kbd>1</kbd> / <kbd>2</kbd> | Heading 1 / heading 2 |
| <kbd>Ctrl</kbd>+<kbd>0</kbd> | Normal text |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>8</kbd> | Bullet list |
| <kbd>Tab</kbd> / <kbd>Shift</kbd>+<kbd>Tab</kbd> | Indent / outdent a bullet |
| <kbd>Ctrl</kbd>+<kbd>+</kbd> / <kbd>-</kbd> | Zoom the editor in / out |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>0</kbd> | Reset zoom |
| <kbd>Ctrl</kbd>+<kbd>Z</kbd> / <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>Z</kbd> | Undo / redo (formatting included) |

Typing `- ` or `* ` at the start of a line makes a bullet, `# ` and `## ` make headings, and Enter on an empty bullet ends the list. Select text to get the floating **B** *I* <u>U</u> • H1 H2 toolbar.

From a terminal, `kludd` shows the running Kludd, `kludd file.md` opens a file in it, `kludd ~/code/app/notes` adds that folder and shows its notes, `kludd --toggle` shows or hides it, and `kludd --quit` saves and quits.

## Configuration

`~/.config/kludd/config.toml` is created when you first change the theme or zoom, or add a folder:

```toml
notes_dir = "~/Kludd"       # where notes live
theme = "pink"              # pink, yellow, mint, lavender, sky or dark
zoom = 1.0                  # editor zoom, changed with Ctrl +/-

[[folders]]                 # an added folder, see Folders below
path = "~/code/app/notes"
color = "mint"              # its card's colour, a theme name (optional)
```

## How notes are stored

Every note is a single Markdown file in `notes_dir`, and nothing else is written there. The title is the note's first `# Heading`, and the file is named after it: *Things to fix* becomes `things-to-fix.md`. Change the title and the file is renamed with it (`-2`, `-3` and so on if the name is taken).

| In the editor | In the file |
|---------------|-------------|
| **Bold** | `**text**` |
| *Italic* | `*text*` |
| <u>Underline</u> | `<u>text</u>` |
| Bullet | `- text`, two spaces per level |
| Heading 1 / 2 | `# ` / `## ` |

`.txt` files in `notes_dir` show up in the notes too, named after the file. They are plain text: `# ` and `**` are just characters, the formatting shortcuts and toolbar are off, and the file is never renamed.

Files opened from elsewhere are edited where they are and are never renamed. Anything that is not a `.md` (or `.markdown`) file is opened as plain text. Deleting a note (right-click its card) moves it to the trash.

## Folders

Notes that live somewhere else, like a project's `docs/notes`, can have a place in Kludd too. Press the folder button next to **+**, type the folder's path (<kbd>Tab</kbd> completes folder names) and press <kbd>Enter</kbd>, or run `kludd ~/code/app/notes`. The folder gets a card above your notes; click it to see the `.md` and `.txt` files in it (not those in its subfolders), and press **+** there to start a note in that folder. Everything else stays in `notes_dir`.

Files in an added folder are edited where they are and keep their names, so links to them keep working. Only a note you start in Kludd is named after its title, while you write it. Right-click a folder card to give it a colour: each dot is one of the themes' accents, and the first one makes the card plain again. *Remove from Kludd*, in the same menu, takes the folder off the list; the folder and its files are left alone.

## License

[MIT](LICENSE): use, change and share Kludd freely; just keep the copyright notice. Kludd bundles the [Fredoka](https://github.com/hafontia/Fredoka-One) font ([OFL 1.1](assets/fonts/OFL.txt)) and six [Tabler Icons](https://tabler.io/icons) ([MIT](assets/icons/LICENSE-tabler)). Contributions are welcome, see [CONTRIBUTING.md](CONTRIBUTING.md).

---

<p align="center">
  Built by <a href="https://github.com/loveranmar">@loveranmar</a>
</p>
