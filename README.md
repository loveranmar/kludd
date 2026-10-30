<h1 align="center">Kludd</h1>

<p align="center">
  <strong>A dead-fast, cute Markdown notepad that lives one keypress away in Hyprland.</strong>
</p>

<p align="center">
  <img src="design/screenshots.png" alt="Kludd: the notes grid and the editor" width="800" />
</p>

---

## What is Kludd?

Kludd starts once when you log in and waits in the background, so pressing <kbd>Home</kbd> shows it right away: no startup, no loading. You write in rich text (bold, lists, tasks, quotes, headings, code blocks, tables, links, pictures) and every note is saved as a plain `.md` file in `~/Kludd`. Kludd draws all of Markdown, GitHub's extras included, and anything it draws over is kept exactly as you wrote it.

## Features

- **Instant** — Kludd keeps running in the background; the hotkey just shows its window
- **Plain files** — one note is one `.md` file named after its title; no database, no index, no frontmatter
- **Rich text, real Markdown** — bold, italic, underline, strikethrough, bullets, numbered lists, tasks you tick with a click, quotes and six levels of heading while you type, saved as normal Markdown
- **Code blocks, tables and links** — GitHub-style code blocks, `code` and <kbd>key</kbd> pills; tables that line themselves up as you type; links that read as their text and open with <kbd>Ctrl</kbd>+click
- **All the rest of Markdown** — pictures, rules, math, footnotes, GitHub's `[!NOTE]` alerts, `==highlights==`, `:emoji:` and inline HTML, drawn the way GitHub shows them
- **Never mangles a note** — whatever Kludd draws stays ordinary Markdown underneath, written back byte for byte (round-trip tested)
- **Autosave** — 400 ms after you stop typing and whenever Kludd hides, written atomically
- **Live reload** — edit a note in nvim and Kludd picks up the change
- **Any `.md` or `.txt` file** — `kludd notes.md` edits a file in place, and Kludd shows up in "Open with"
- **Folders** — add any folder, like a project's `docs/notes`, and its notes get a view of their own; new notes can start right there
- **Plain text stays plain** — a `.txt` file shows exactly what it holds: no formatting, saved exactly as typed
- **Six themes** — pink, yellow, mint, lavender, sky and dark, switched live from the palette button
- **Big notes stay smooth** — in the test suite a 5 000-line note full of links and code loads in about 70 ms and a keystroke costs about 0.1 ms

## Install

### From source

```bash
# GTK 4, the code font and a Rust toolchain (rustup works too)
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
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>X</kbd> | Strikethrough |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>H</kbd> | Highlight (`==text==`) |
| <kbd>Ctrl</kbd>+<kbd>1</kbd> … <kbd>6</kbd> | Heading 1 to 6 |
| <kbd>Ctrl</kbd>+<kbd>0</kbd> | Normal text |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>7</kbd> / <kbd>8</kbd> / <kbd>9</kbd> | Numbered list / bullet list / task list |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>.</kbd> | Quote |
| <kbd>Tab</kbd> / <kbd>Shift</kbd>+<kbd>Tab</kbd> | Nest / un-nest a list item; in a table, next / previous cell |
| <kbd>Enter</kbd> in a table | New row (on an empty last row: leave the table) |
| <kbd>Ctrl</kbd>+click | Open a link |
| <kbd>Ctrl</kbd>+<kbd>+</kbd> / <kbd>-</kbd> | Zoom the editor in / out |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>0</kbd> | Reset zoom |
| <kbd>Ctrl</kbd>+<kbd>Z</kbd> / <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>Z</kbd> | Undo / redo (formatting included) |

Typing `- ` or `* ` at the start of a line makes a bullet, `# ` and `## ` make headings, and Enter on an empty bullet ends the list. Select text to get the floating **B** *I* <u>U</u> • H1 H2 toolbar. ```` ``` ```` (or ```` ```rust ````) and <kbd>Enter</kbd> starts a code block, and a `| a | b |` line with a `|---|---|` line under it and <kbd>Enter</kbd> starts a table; see [Code blocks, tables and links](#code-blocks-tables-and-links).

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
| `Code` / <kbd>Key</kbd> | `` `code` `` / `<kbd>Key</kbd>` |
| Code block | ```` ``` ```` fences around the code |
| Link | `[text](https://…)`, or the address on its own |
| Table | `\| a \| b \|` rows, `\|---\|---\|` under the head |

`.txt` files in `notes_dir` show up in the notes too, named after the file. They are plain text: `# ` and `**` are just characters, the formatting shortcuts and toolbar are off, and the file is never renamed.

Files opened from elsewhere are edited where they are and are never renamed. Anything that is not a `.md` (or `.markdown`) file is opened as plain text. Deleting a note (right-click its card) moves it to the trash.

## Writing

Lines start as what their Markdown says, as you type it: `- ` or `* ` a bullet, `1. ` a numbered item, `[ ] ` a task (on a bullet too), `> ` a quote, and `# ` to `###### ` a heading. <kbd>Enter</kbd> carries a list or quote on to the next line (the next number, an open box), and <kbd>Enter</kbd> on an empty item ends it. <kbd>Backspace</kbd> at the start of an item takes it a level out, then back to text.

- **Numbered lists** count the way Markdown does, on from their first number, so `1.` `1.` `1.` reads 1, 2, 3; the numbers line up however many there are. <kbd>Tab</kbd> nests an item under the one before it.
- **Tasks** get a box; click it to tick the task off (it fades), or click again to bring it back.
- **Quotes** get a bar down their side, one per level. A quote that starts with `[!NOTE]`, `[!TIP]`, `[!IMPORTANT]`, `[!WARNING]` or `[!CAUTION]` is one of GitHub's alerts: coloured, with its icon and title.
- **Strikethrough** is `~~text~~`, from the toolbar's **S** or <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>X</kbd>.
- **A line across**: type `---` (or `***`) and press <kbd>Enter</kbd>. A heading underlined with `===` or `---` reads as one too.

## Code blocks, tables, links and the rest

Kludd draws these the way GitHub shows them, but underneath they stay ordinary Markdown: opening and saving a note never changes them. Their syntax hides while the cursor is away and shows, faintly, when it is there.

- **Code blocks** get a box in JetBrains Mono. The ```` ``` ```` fences hide until the cursor is in the block. Typing ```` ``` ```` (or ```` ```rust ````) and pressing <kbd>Enter</kbd> starts one, already closed, and <kbd>Enter</kbd> inside keeps the line's indentation. What you type or paste in a code block is kept as written: `**` stays two stars, and the format toolbar stays away.
- **`Code`** and **<kbd>keys</kbd>** (`<kbd>Ctrl</kbd>`) get pills; their backticks and tags show only while the cursor is at them.
- **Tables** get a grid with a shaded head. Typing in a cell lines the columns up again as you go, and <kbd>Tab</kbd> / <kbd>Shift</kbd>+<kbd>Tab</kbd> move between cells (<kbd>Tab</kbd> in the last cell adds a row). <kbd>Enter</kbd> adds a row; on an empty last row it leaves the table. To start a table, type its head (`| Name | Age |`), then a `|---|---|` line, and press <kbd>Enter</kbd>.
- **Links** read as their text, underlined; the `[`, `](…)` show while the cursor is at them, and an address written out (`https://…`) is a link too, as is a reference-style link (`[text][ref]`, with `[ref]: https://…` further down, set small and faint). <kbd>Ctrl</kbd>+click opens a link: a web address in your browser, and a link to a note (`[plan](plan.md)`, next to this one) right in Kludd.
- **Pictures** (`![alt](shot.png)`, next to the note) show under their line, rounded and sized to fit. Pictures on the web are not fetched: they read as their alt text, a link to them.
- **Math**, `$x^2$` in a line or `$$` blocks (type `$$` and press <kbd>Enter</kbd>), is set in a serif face, display math centred in a box. The LaTeX itself is shown, not typeset.
- **Footnotes**: a reference, `[^1]`, is raised and small; <kbd>Ctrl</kbd>+click goes to its text, `[^1]: …`, which is set small like a footnote.
- **`==Highlights==`** sit on the theme's colour, a softer shade of the selection's. Select text and press the toolbar's highlighter (or <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>H</kbd>) to write the `==` for you; again takes them off. **`:emoji:`** codes (GitHub's, `:tada:` gives 🎉) show as the emoji.
- **Indented code** (four spaces in) gets a code block's box.
- **HTML**: `<b>`, `<i>`, `<s>`, `<u>`, `<mark>`, `<sup>`, `<sub>`, `<code>`, `<a href>`, `<img src>` and `<summary>` look like what they mean; other tags and `<!-- comments -->` stay, faintly.

## Folders

Notes that live somewhere else, like a project's `docs/notes`, can have a place in Kludd too. Press the folder button next to **+**, type the folder's path (<kbd>Tab</kbd> completes folder names) and press <kbd>Enter</kbd>, or run `kludd ~/code/app/notes`. The folder gets a card above your notes; click it to see the `.md` and `.txt` files in it (not those in its subfolders), and press **+** there to start a note in that folder. Everything else stays in `notes_dir`.

Files in an added folder are edited where they are and keep their names, so links to them keep working. Only a note you start in Kludd is named after its title, while you write it. Right-click a folder card to give it a colour: each dot is one of the themes' accents, and the first one makes the card plain again. *Remove from Kludd*, in the same menu, takes the folder off the list; the folder and its files are left alone.

## License

[MIT](LICENSE): use, change and share Kludd freely; just keep the copyright notice. Kludd bundles the [Fredoka](https://github.com/hafontia/Fredoka-One) font ([OFL 1.1](assets/fonts/OFL.txt)) fourteen [Tabler Icons](https://tabler.io/icons) ([MIT](assets/icons/LICENSE-tabler)) and GitHub's [emoji shortcodes](https://github.com/github/gemoji) ([MIT](assets/LICENSE-gemoji)). Contributions are welcome, see [CONTRIBUTING.md](CONTRIBUTING.md).

---

<p align="center">
  Built by <a href="https://github.com/loveranmar">@loveranmar</a>
</p>
