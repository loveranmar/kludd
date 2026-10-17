//! Markdown <-> Kludd's line model.
//!
//! One editor line is exactly one line of the file. Every line is either
//! rich (plain text, `# H1`, `## H2` or a `- bullet`, with bold, italic and
//! underline inside) or raw (kept byte for byte and shown as plain text).
//!
//! Round-tripping is guaranteed by construction: a line is only treated as
//! rich if serializing what was parsed reproduces the original line exactly.
//! Everything else (code blocks, tables, quotes, numbered lists, links, ...)
//! stays raw, so opening and saving a file never changes it.

use std::ops::Range;

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Inline formatting of a run of text, as a small bit set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style(pub u8);

impl Style {
    pub const NONE: Style = Style(0);
    pub const BOLD: Style = Style(1);
    pub const ITALIC: Style = Style(2);
    pub const UNDERLINE: Style = Style(4);
    /// Written verbatim: never escaped, never wrapped in markup.
    pub const RAW: Style = Style(8);
    /// A backslash-escaped character: shows `*`, saves `\*`.
    pub const ESCAPED: Style = Style(16);
    /// Italic written as `_x_` rather than `*x*`.
    pub const ITALIC_US: Style = Style(32);
    /// Bold written as `__x__` rather than `**x**`.
    pub const BOLD_US: Style = Style(64);

    /// The bits that change how text looks.
    pub const MARKUP: Style = Style(1 | 2 | 4);

    pub fn has(self, other: Style) -> bool {
        self.0 & other.0 == other.0
    }
    pub fn with(self, other: Style) -> Style {
        Style(self.0 | other.0)
    }
    pub fn without(self, other: Style) -> Style {
        Style(self.0 & !other.0)
    }
    pub fn and(self, other: Style) -> Style {
        Style(self.0 & other.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Text,
    H1,
    H2,
    /// `indent` leading spaces, then `marker` (`-`, `*` or `+`) and a space.
    Bullet {
        indent: u8,
        marker: u8,
    },
    /// Unsupported Markdown, kept exactly as written.
    Raw,
}

impl LineKind {
    pub fn bullet(indent: u8) -> LineKind {
        LineKind::Bullet {
            indent,
            marker: b'-',
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eol {
    None,
    Lf,
    CrLf,
    Cr,
}

impl Eol {
    pub fn as_str(self) -> &'static str {
        match self {
            Eol::None => "",
            Eol::Lf => "\n",
            Eol::CrLf => "\r\n",
            Eol::Cr => "\r",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

impl Span {
    pub fn new(text: impl Into<String>, style: Style) -> Span {
        Span {
            text: text.into(),
            style,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub spans: Vec<Span>,
    pub eol: Eol,
}

impl Line {
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH
}

// ---------------------------------------------------------------------------
// Parsing

/// Splits a file into (content, line ending) pairs. A trailing line ending
/// does not produce an extra empty line; an empty file is one empty line.
pub fn split_lines(src: &str) -> Vec<(Range<usize>, Eol)> {
    let bytes = src.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                lines.push((start..i, Eol::Lf));
                i += 1;
                start = i;
            }
            b'\r' if bytes.get(i + 1) == Some(&b'\n') => {
                lines.push((start..i, Eol::CrLf));
                i += 2;
                start = i;
            }
            b'\r' => {
                lines.push((start..i, Eol::Cr));
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < bytes.len() || lines.is_empty() {
        lines.push((start..bytes.len(), Eol::None));
    }
    lines
}

#[derive(Clone, Copy, PartialEq)]
enum Class {
    /// Not part of any block: blank, or something the parser swallowed
    /// (like a link reference definition).
    Uncovered,
    /// Inside a block Kludd can show as rich lines.
    Rich,
    Raw,
}

/// Parses a whole file into lines. `serialize(&parse(s)) == s` for every `s`.
pub fn parse(src: &str) -> Vec<Line> {
    let lines = split_lines(src);
    let mut class = vec![Class::Uncovered; lines.len()];
    let line_of = |offset: usize| -> usize {
        // index of the line whose content+eol contains `offset`
        lines
            .partition_point(|(r, _)| r.start <= offset)
            .saturating_sub(1)
    };

    let mut depth = 0usize;
    let mut block: Option<(Range<usize>, bool)> = None;
    for (event, range) in Parser::new_ext(src, options()).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                if depth == 0 {
                    let rich = match &tag {
                        Tag::Paragraph | Tag::List(None) => true,
                        Tag::Heading { level, .. } => {
                            matches!(level, HeadingLevel::H1 | HeadingLevel::H2)
                                && src[range.clone()].starts_with('#')
                                && !src[range.clone()].trim_end().contains(['\n', '\r'])
                        }
                        _ => false,
                    };
                    block = Some((range.clone(), rich));
                } else if let Some((_, rich)) = block.as_mut() {
                    // Lists are only rich if every item is a simple line.
                    let simple = matches!(
                        tag,
                        Tag::Item
                            | Tag::Paragraph
                            | Tag::List(None)
                            | Tag::Emphasis
                            | Tag::Strong
                            | Tag::Strikethrough
                            | Tag::Link { .. }
                            | Tag::Image { .. }
                    );
                    if !simple {
                        *rich = false;
                    }
                }
                depth += 1;
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some((range, rich)) = block.take()
                {
                    mark(&mut class, &line_of, range, rich);
                }
            }
            Event::Rule | Event::DisplayMath(_) | Event::Html(_) if depth == 0 => {
                mark(&mut class, &line_of, range, false);
            }
            Event::TaskListMarker(_) => {
                if let Some((_, rich)) = block.as_mut() {
                    *rich = false;
                }
            }
            _ => {}
        }
    }

    lines
        .into_iter()
        .zip(class)
        .map(|((range, eol), class)| {
            let content = &src[range];
            let blank = content.trim().is_empty();
            let rich = match class {
                Class::Rich => true,
                Class::Uncovered => blank,
                Class::Raw => false,
            };
            let parsed = if blank && class != Class::Raw {
                Some(blank_line(content))
            } else if rich {
                parse_line(content)
                    .filter(|(kind, spans)| render(*kind, spans, Mode::PLAIN) == content)
            } else {
                None
            };
            match parsed {
                Some((kind, spans)) => Line { kind, spans, eol },
                None => raw_line(content, eol),
            }
        })
        .collect()
}

fn mark(class: &mut [Class], line_of: &impl Fn(usize) -> usize, range: Range<usize>, rich: bool) {
    if range.is_empty() {
        return;
    }
    let (first, last) = (line_of(range.start), line_of(range.end - 1));
    for c in &mut class[first..=last] {
        *c = match (*c, rich) {
            (Class::Raw, _) | (_, false) => Class::Raw,
            _ => Class::Rich,
        };
    }
}

fn blank_line(content: &str) -> (LineKind, Vec<Span>) {
    let spans = if content.is_empty() {
        Vec::new()
    } else {
        vec![Span::new(content, Style::RAW)]
    };
    (LineKind::Text, spans)
}

pub fn raw_line(content: &str, eol: Eol) -> Line {
    let spans = if content.is_empty() {
        Vec::new()
    } else {
        vec![Span::new(content, Style::RAW)]
    };
    Line {
        kind: LineKind::Raw,
        spans,
        eol,
    }
}

/// Recognises the line prefix: `# `, `## `, `<indent>- ` or nothing.
fn line_prefix(content: &str) -> (LineKind, usize) {
    if content.starts_with("# ") {
        return (LineKind::H1, 2);
    }
    if content.starts_with("## ") {
        return (LineKind::H2, 3);
    }
    let indent = content.len() - content.trim_start_matches(' ').len();
    let rest = &content.as_bytes()[indent..];
    if indent <= u8::MAX as usize
        && rest.len() >= 2
        && matches!(rest[0], b'-' | b'*' | b'+')
        && rest[1] == b' '
    {
        let kind = LineKind::Bullet {
            indent: indent as u8,
            marker: rest[0],
        };
        return (kind, indent + 2);
    }
    (LineKind::Text, 0)
}

/// Parses one line on its own. Returns `None` if it isn't a single rich line.
/// The caller still has to check that rendering reproduces the input.
pub fn parse_line(content: &str) -> Option<(LineKind, Vec<Span>)> {
    let (kind, prefix_len) = line_prefix(content);
    let body = &content[prefix_len..];
    let lead_len = body.len() - body.trim_start_matches([' ', '\t']).len();
    let core_end = body.trim_end_matches([' ', '\t']).len().max(lead_len);
    let (lead, core, trail) = (
        &body[..lead_len],
        &body[lead_len..core_end],
        &body[core_end..],
    );

    let mut spans = Vec::new();
    if !lead.is_empty() {
        spans.push(Span::new(lead, Style::RAW));
    }
    if !core.is_empty() {
        let shown_prefix = match kind {
            LineKind::H1 => "# ",
            LineKind::H2 => "## ",
            LineKind::Bullet { .. } => "- ",
            _ => "",
        };
        let source = format!("{shown_prefix}{core}");
        spans.extend(parse_inline(&source, shown_prefix.len(), kind)?);
    }
    if !trail.is_empty() {
        spans.push(Span::new(trail, Style::RAW));
    }
    Some((kind, merge(spans)))
}

fn parse_inline(src: &str, offset: usize, kind: LineKind) -> Option<Vec<Span>> {
    let mut events = Parser::new_ext(src, options()).into_offset_iter();

    // The block structure must be exactly what the prefix promises.
    let opening: &[fn(&Event) -> bool] = match kind {
        LineKind::H1 => &[|e| {
            matches!(
                e,
                Event::Start(Tag::Heading {
                    level: HeadingLevel::H1,
                    ..
                })
            )
        }],
        LineKind::H2 => &[|e| {
            matches!(
                e,
                Event::Start(Tag::Heading {
                    level: HeadingLevel::H2,
                    ..
                })
            )
        }],
        LineKind::Bullet { .. } => &[
            |e| matches!(e, Event::Start(Tag::List(None))),
            |e| matches!(e, Event::Start(Tag::Item)),
        ],
        _ => &[|e| matches!(e, Event::Start(Tag::Paragraph))],
    };
    for check in opening {
        let (event, _) = events.next()?;
        if !check(&event) {
            return None;
        }
    }
    let closing = opening.len();

    let mut spans = Vec::new();
    let mut style = Style::NONE;
    let mut stack: Vec<Style> = Vec::new();
    let mut underline = 0u32;
    let mut leaf_end = offset; // end of the last text-carrying event
    let mut skip_until: Option<(usize, Range<usize>)> = None;
    let mut closed = 0;

    let raw = |spans: &mut Vec<Span>, range: Range<usize>, style: Style| {
        spans.push(Span::new(&src[range], style.with(Style::RAW)));
    };

    for (event, range) in events.by_ref() {
        if let Some((depth, ref whole)) = skip_until {
            // inside a link/image/strikethrough: emitted as one raw span
            match event {
                Event::Start(_) => skip_until = Some((depth + 1, whole.clone())),
                Event::End(_) if depth == 0 => {
                    let whole = whole.clone();
                    leaf_end = whole.end;
                    raw(&mut spans, whole, style);
                    skip_until = None;
                }
                Event::End(_) => skip_until = Some((depth - 1, whole.clone())),
                _ => {}
            }
            continue;
        }
        match event {
            Event::Text(text) => {
                let source = &src[range.clone()];
                if source == &*text {
                    let escaped = range.start > leaf_end
                        && src.as_bytes()[range.start - 1] == b'\\'
                        && text.starts_with(|c: char| c.is_ascii_punctuation());
                    if escaped {
                        let (first, rest) = text.split_at(1);
                        spans.push(Span::new(first, style.with(Style::ESCAPED)));
                        if !rest.is_empty() {
                            spans.push(Span::new(rest, style));
                        }
                    } else {
                        spans.push(Span::new(&*text, style));
                    }
                } else {
                    // entities and the like: keep the source
                    raw(&mut spans, range.clone(), style);
                }
                leaf_end = range.end;
            }
            Event::Start(Tag::Strong) => {
                stack.push(style);
                let us = src.as_bytes()[range.start] == b'_';
                style = style.with(Style::BOLD);
                if us {
                    style = style.with(Style::BOLD_US);
                }
            }
            Event::Start(Tag::Emphasis) => {
                stack.push(style);
                let us = src.as_bytes()[range.start] == b'_';
                style = style.with(Style::ITALIC);
                if us {
                    style = style.with(Style::ITALIC_US);
                }
            }
            Event::End(TagEnd::Strong | TagEnd::Emphasis) => {
                let underlined = style.has(Style::UNDERLINE);
                style = stack.pop()?;
                if underlined {
                    style = style.with(Style::UNDERLINE);
                } else {
                    style = style.without(Style::UNDERLINE);
                }
            }
            Event::InlineHtml(html) if &*html == "<u>" => {
                underline += 1;
                style = style.with(Style::UNDERLINE);
                leaf_end = range.end;
            }
            Event::InlineHtml(html) if &*html == "</u>" && underline > 0 => {
                underline -= 1;
                if underline == 0 {
                    style = style.without(Style::UNDERLINE);
                }
                leaf_end = range.end;
            }
            Event::Start(_) => {
                skip_until = Some((0, range));
            }
            Event::End(_) => {
                closed += 1;
                break;
            }
            Event::Code(_)
            | Event::InlineMath(_)
            | Event::InlineHtml(_)
            | Event::Html(_)
            | Event::FootnoteReference(_)
            | Event::TaskListMarker(_) => {
                raw(&mut spans, range.clone(), style);
                leaf_end = range.end;
            }
            Event::SoftBreak | Event::HardBreak | Event::Rule | Event::DisplayMath(_) => {
                return None;
            }
        }
    }
    // the remaining closing tags, then nothing else
    for (event, _) in events.by_ref() {
        if !matches!(event, Event::End(_)) {
            return None;
        }
        closed += 1;
    }
    if closed != closing || underline != 0 || !stack.is_empty() {
        return None;
    }
    Some(spans)
}

/// Joins neighbouring spans that share a style.
pub fn merge(spans: Vec<Span>) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.style == span.style => last.text.push_str(&span.text),
            _ => out.push(span),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Serializing

pub fn serialize(lines: &[Line]) -> String {
    let mut out = String::new();
    for line in lines {
        out.push_str(&serialize_line(line.kind, &line.spans));
        out.push_str(line.eol.as_str());
    }
    out
}

/// Turns one line back into Markdown. Text is only escaped when it would
/// otherwise be read back differently.
pub fn serialize_line(kind: LineKind, spans: &[Span]) -> String {
    let plain = render(kind, spans, Mode::PLAIN);
    if kind == LineKind::Raw || !needs_check(spans) {
        return plain;
    }
    let expected = normalized(spans);
    let reads_back = |md: &str| match parse_line(md) {
        Some((k, s)) => same_kind(k, kind) && normalized(&s) == expected,
        None => false,
    };
    if reads_back(&plain) {
        return plain;
    }
    for mode in [Mode::ESCAPED, Mode::ESCAPED_ALT] {
        let md = render(kind, spans, mode);
        if reads_back(&md) {
            return md;
        }
    }
    // The formatting cannot be expressed here (rare delimiter clashes):
    // keep every character and drop the formatting rather than the text.
    render(kind, spans, Mode::TEXT_ONLY)
}

fn same_kind(a: LineKind, b: LineKind) -> bool {
    match (a, b) {
        (LineKind::Bullet { indent: x, .. }, LineKind::Bullet { indent: y, .. }) => x == y,
        _ => a == b,
    }
}

/// Characters with the formatting a reader would see. Whitespace counts as
/// plain either way, and escaping is a detail of the source.
fn normalized(spans: &[Span]) -> Vec<(char, u8)> {
    spans
        .iter()
        .flat_map(|span| {
            span.text.chars().map(move |c| {
                // whitespace carries no visible style of its own
                let style = if c.is_whitespace() {
                    Style::NONE
                } else {
                    span.style.and(Style(Style::MARKUP.0 | Style::RAW.0))
                };
                (c, style.0)
            })
        })
        .collect()
}

const INLINE_SPECIAL: &[char] = &[
    '\\', '*', '_', '`', '[', ']', '<', '>', '~', '&', '!', '#', '|',
];

fn needs_check(spans: &[Span]) -> bool {
    let mut first = true;
    for span in spans {
        if span.style.and(Style::MARKUP) != Style::NONE {
            return true;
        }
        if span.style.has(Style::RAW) || span.style.has(Style::ESCAPED) {
            first = false;
            continue;
        }
        if span.text.contains(INLINE_SPECIAL) {
            return true;
        }
        if first {
            let c = span.text.chars().next().unwrap_or(' ');
            if c.is_whitespace() || c.is_ascii_digit() || "#>-+*=|`~<".contains(c) {
                return true;
            }
        }
        first = false;
    }
    false
}

fn prefix(kind: LineKind) -> String {
    match kind {
        LineKind::H1 => "# ".into(),
        LineKind::H2 => "## ".into(),
        LineKind::Bullet { indent, marker } => {
            let mut s = " ".repeat(indent as usize);
            s.push(marker as char);
            s.push(' ');
            s
        }
        LineKind::Text | LineKind::Raw => String::new(),
    }
}

#[derive(Clone, Copy)]
struct Mode {
    /// Backslash-escape every character that could start Markdown syntax.
    escape: bool,
    /// Write italics with the other delimiter (`_` instead of `*`).
    swap_italic: bool,
    /// Emit bold/italic/underline markup at all.
    markup: bool,
}

impl Mode {
    const PLAIN: Mode = Mode {
        escape: false,
        swap_italic: false,
        markup: true,
    };
    const ESCAPED: Mode = Mode {
        escape: true,
        swap_italic: false,
        markup: true,
    };
    const ESCAPED_ALT: Mode = Mode {
        escape: true,
        swap_italic: true,
        markup: true,
    };
    const TEXT_ONLY: Mode = Mode {
        escape: true,
        swap_italic: false,
        markup: false,
    };
}

/// Renders spans as Markdown source.
fn render(kind: LineKind, spans: &[Span], mode: Mode) -> String {
    let mut out = prefix(kind);
    if kind == LineKind::Raw {
        for span in spans {
            out.push_str(&span.text);
        }
        return out;
    }

    let chars: Vec<(char, Style)> = spans
        .iter()
        .flat_map(|s| s.text.chars().map(move |c| (c, s.style)))
        .collect();

    // Markup a character sits inside. Whitespace at the edge of a styled
    // run is moved outside it (`**bold **` would not parse as bold).
    let markup: Vec<Style> = {
        let solid: Vec<Option<Style>> = chars
            .iter()
            .map(|&(c, s)| (!c.is_whitespace()).then(|| s.and(Style::MARKUP)))
            .collect();
        let mut left = vec![Style::NONE; chars.len()];
        let mut prev = Style::NONE;
        for (i, s) in solid.iter().enumerate() {
            if let Some(s) = s {
                prev = *s;
            }
            left[i] = prev;
        }
        let mut right = vec![Style::NONE; chars.len()];
        let mut next = Style::NONE;
        for i in (0..chars.len()).rev() {
            if let Some(s) = solid[i] {
                next = s;
            }
            right[i] = next;
        }
        // markup every character of a whitespace run has itself: spaces stay
        // inside a run only if they were formatted too (`**a** **b**` stays
        // two runs, a bolded "a b" stays one)
        let mut own = vec![Style::NONE; chars.len()];
        let mut i = 0;
        while i < chars.len() {
            if solid[i].is_some() {
                i += 1;
                continue;
            }
            let start = i;
            let mut shared = Style::MARKUP;
            while i < chars.len() && solid[i].is_none() {
                shared = shared.and(chars[i].1);
                i += 1;
            }
            own[start..i].fill(shared);
        }
        (0..chars.len())
            .map(|i| match solid[i] {
                _ if !mode.markup => Style::NONE,
                Some(s) => s,
                None if i == 0 || i == chars.len() - 1 => Style::NONE,
                None => left[i].and(right[i]).and(own[i]),
            })
            .collect()
    };

    // `1. ` / `1) ` at the start would begin a numbered list: escape the dot
    let indent = chars.iter().take_while(|(c, _)| c.is_whitespace()).count();
    let digits = chars[indent..]
        .iter()
        .take_while(|(c, s)| c.is_ascii_digit() && *s == Style::NONE)
        .count();
    let list_dot = (mode.escape
        && digits > 0
        && matches!(chars.get(indent + digits), Some(('.' | ')', Style::NONE))))
    .then_some(indent + digits);

    let mut open: Vec<(Style, &'static str)> = Vec::new();
    let mut at_start = true;
    for (i, &(c, style)) in chars.iter().enumerate() {
        let want = markup[i];
        // close everything above the first flag we no longer want
        if let Some(pos) = open.iter().position(|(flag, _)| !want.has(*flag)) {
            while open.len() > pos {
                let (_, close) = open.pop().unwrap();
                out.push_str(close);
            }
        }
        // open what is missing, longest-lasting first so it nests outermost;
        // never right before whitespace (`** x**` is not bold)
        let mut missing: Vec<(usize, Style)> = [Style::UNDERLINE, Style::BOLD, Style::ITALIC]
            .into_iter()
            .filter(|flag| {
                !c.is_whitespace() && want.has(*flag) && !open.iter().any(|(f, _)| f == flag)
            })
            .map(|flag| (markup[i..].iter().take_while(|m| m.has(flag)).count(), flag))
            .collect();
        missing.sort_by_key(|&(run, _)| std::cmp::Reverse(run));
        for (_, flag) in missing {
            open_flag(&mut out, &mut open, flag, style, mode);
        }

        let escape = !style.has(Style::RAW)
            && (style.has(Style::ESCAPED)
                || (mode.escape && (needs_escape(c, at_start) || list_dot == Some(i))));
        if escape {
            out.push('\\');
        }
        out.push(c);
        at_start &= c.is_whitespace();
    }
    while let Some((_, close)) = open.pop() {
        out.push_str(close);
    }
    out
}

fn open_flag(
    out: &mut String,
    open: &mut Vec<(Style, &'static str)>,
    flag: Style,
    style: Style,
    mode: Mode,
) {
    let (start, end) = if flag == Style::UNDERLINE {
        ("<u>", "</u>")
    } else if flag == Style::BOLD {
        if style.has(Style::BOLD_US) {
            ("__", "__")
        } else {
            ("**", "**")
        }
    } else if style.has(Style::ITALIC_US) != mode.swap_italic {
        ("_", "_")
    } else {
        ("*", "*")
    };
    out.push_str(start);
    open.push((flag, end));
}

fn needs_escape(c: char, line_start: bool) -> bool {
    matches!(
        c,
        '\\' | '*' | '_' | '`' | '[' | ']' | '<' | '~' | '&' | '!'
    ) || (line_start && matches!(c, '#' | '>' | '-' | '+' | '=' | '|'))
}

// ---------------------------------------------------------------------------
// Helpers for the note list

/// The note title: text of the first `# ` heading outside code fences.
pub fn title(src: &str) -> Option<String> {
    let mut fence: Option<&str> = None;
    for line in src.lines() {
        let trimmed = line.trim_start();
        if let Some(f) = fence {
            if trimmed.starts_with(f) {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") {
            fence = Some("```");
        } else if trimmed.starts_with("~~~") {
            fence = Some("~~~");
        } else if let Some(rest) = line.strip_prefix("# ") {
            return Some(plain_text(rest.trim()));
        } else if line.trim_end() == "#" {
            return Some(String::new());
        }
    }
    None
}

/// A whole note as the text a reader sees, one line per block, for search
/// (`Call the **dentist**` matches "the dentist").
pub fn searchable(src: &str) -> String {
    let mut out = String::new();
    for event in Parser::new_ext(src, options()) {
        match event {
            Event::Text(t) | Event::Code(t) | Event::InlineHtml(t) | Event::Html(t) => {
                if !t.starts_with('<') {
                    out.push_str(&t);
                }
            }
            Event::SoftBreak | Event::HardBreak => out.push(' '),
            Event::End(
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::Item
                | TagEnd::CodeBlock
                | TagEnd::HtmlBlock
                | TagEnd::TableCell
                | TagEnd::BlockQuote(_),
            ) if !out.ends_with('\n') => out.push('\n'),
            _ => {}
        }
    }
    out
}

/// Inline Markdown to plain text (`**a** b` -> `a b`).
pub fn plain_text(src: &str) -> String {
    let mut out = String::new();
    for event in Parser::new_ext(src, options()) {
        match event {
            Event::Text(t) | Event::Code(t) => out.push_str(&t),
            Event::SoftBreak | Event::HardBreak => out.push(' '),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(src: &str) {
        let lines = parse(src);
        let out = serialize(&lines);
        assert_eq!(out, src, "round trip changed the file\nlines: {lines:#?}");
    }

    #[test]
    fn design_note_round_trips() {
        let src = "# Things to fix\nCall the **dentist** before <u>Friday</u>.\n- Buy milk\n- Push to GitHub\n## Later\nSome *italic* text here.\n";
        round_trip(src);
        let lines = parse(src);
        assert_eq!(lines[0].kind, LineKind::H1);
        assert_eq!(lines[0].text(), "Things to fix");
        assert_eq!(lines[1].kind, LineKind::Text);
        assert_eq!(
            lines[1].spans,
            vec![
                Span::new("Call the ", Style::NONE),
                Span::new("dentist", Style::BOLD),
                Span::new(" before ", Style::NONE),
                Span::new("Friday", Style::UNDERLINE),
                Span::new(".", Style::NONE),
            ]
        );
        assert_eq!(lines[2].kind, LineKind::bullet(0));
        assert_eq!(lines[4].kind, LineKind::H2);
        assert_eq!(lines[5].spans[1], Span::new("italic", Style::ITALIC));
    }

    #[test]
    fn unsupported_blocks_stay_raw() {
        let src = "# Title\n\n```rust\nfn main() { **not bold** }\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n> quote\n\n1. one\n2. two\n\n### deep\n\n---\n\n<div>\nhtml\n</div>\n\n[ref]: https://example.com\n";
        round_trip(src);
        let lines = parse(src);
        let raw: Vec<_> = lines
            .iter()
            .filter(|l| l.kind == LineKind::Raw)
            .map(|l| l.text())
            .collect();
        assert!(raw.contains(&"fn main() { **not bold** }".to_string()));
        assert!(raw.contains(&"| 1 | 2 |".to_string()));
        assert!(raw.contains(&"> quote".to_string()));
        assert!(raw.contains(&"1. one".to_string()));
        assert!(raw.contains(&"### deep".to_string()));
        assert!(raw.contains(&"[ref]: https://example.com".to_string()));
    }

    #[test]
    fn inline_extras_are_kept_verbatim() {
        let src = "See [the docs](https://x.y) and `code` with &amp; ~~old~~ <b>html</b>.\n";
        round_trip(src);
        let line = &parse(src)[0];
        assert_eq!(line.kind, LineKind::Text);
        assert!(
            line.spans
                .iter()
                .any(|s| s.text == "[the docs](https://x.y)" && s.style.has(Style::RAW))
        );
    }

    #[test]
    fn escapes_show_clean_and_save_the_same() {
        let src = "5 \\* 3 and \\_x\\_\n";
        round_trip(src);
        let line = &parse(src)[0];
        assert_eq!(line.text(), "5 * 3 and _x_");
        assert_eq!(line.kind, LineKind::Text);
    }

    #[test]
    fn nested_bullets_and_markers() {
        let src = "- one\n  - two\n    - three\n* star\n+ plus\n";
        round_trip(src);
        let lines = parse(src);
        assert_eq!(lines[1].kind, LineKind::bullet(2));
        assert_eq!(lines[2].kind, LineKind::bullet(4));
        assert_eq!(
            lines[3].kind,
            LineKind::Bullet {
                indent: 0,
                marker: b'*'
            }
        );
    }

    #[test]
    fn line_endings_and_whitespace() {
        for src in [
            "",
            "\n",
            "\n\n\n",
            "a",
            "a\r\nb\r\n",
            "a\rb\nc\r\n",
            "  indented\n",
            "trailing   \n",
            "\t# tab heading\n",
            "#\n##\n#notheading\n",
            "- \n-\n",
            "    code\n",
            "Title\n===\n",
            "a  \nb\n",
            "a\\\nb\n",
        ] {
            round_trip(src);
        }
    }

    #[test]
    fn emphasis_variants() {
        for src in [
            "**a***b*\n",
            "***both***\n",
            "__a__ _b_\n",
            "**bold **x\n",
            "* not a bullet*\n",
            "<u>**x**</u> <u>open\n",
            "a</u>b\n",
            "**a\n",
            "_intra_word and in*tra*word\n",
            "**a** **b**\n",
            "*a* *b* and _c_ _d_\n",
            "<u>a</u> <u>b</u>\n",
        ] {
            round_trip(src);
        }
        for src in ["**a** **b**", "*a* *b*", "_a_ _b_", "<u>a</u> <u>b</u>"] {
            assert_ne!(
                parse(src)[0].kind,
                LineKind::Raw,
                "{src} should show formatted"
            );
        }
        let line = &parse("__a__ _b_\n")[0];
        assert!(line.spans[0].style.has(Style::BOLD.with(Style::BOLD_US)));
    }

    #[test]
    fn serializer_escapes_only_when_needed() {
        let plain = |t: &str| serialize_line(LineKind::Text, &[Span::new(t, Style::NONE)]);
        assert_eq!(plain("5 * 3"), "5 * 3");
        assert_eq!(plain("C:\\Users"), "C:\\Users");
        assert_eq!(plain("*not italic*"), "\\*not italic\\*");
        assert_eq!(plain("# not a heading"), "\\# not a heading");
        assert_eq!(plain("- not a bullet"), "\\- not a bullet");
        assert_eq!(plain("1. not a list"), "1\\. not a list");
        assert_eq!(plain("a [link](x)"), "a \\[link\\](x)");
        assert_eq!(plain("Tom & Jerry"), "Tom & Jerry");
    }

    #[test]
    fn serializer_moves_spaces_out_of_markup() {
        let spans = [
            Span::new("hello ", Style::BOLD),
            Span::new("world", Style::NONE),
        ];
        assert_eq!(serialize_line(LineKind::Text, &spans), "**hello** world");
        let spans = [Span::new("a", Style::BOLD), Span::new("b", Style::ITALIC)];
        let md = serialize_line(LineKind::Text, &spans);
        let (_, back) = parse_line(&md).unwrap();
        assert_eq!(normalized(&back), normalized(&spans), "{md}");
    }

    /// Tiny xorshift so the fuzz tests need no dependencies.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items[(self.next() % items.len() as u64) as usize]
        }
    }

    #[test]
    fn fuzz_any_file_round_trips() {
        let pieces = [
            "a",
            "word",
            " ",
            "  ",
            "\t",
            "\n",
            "\n",
            "\r\n",
            "\r",
            "*",
            "**",
            "_",
            "__",
            "`",
            "```",
            "~",
            "~~",
            "#",
            "# ",
            "## ",
            "### ",
            "- ",
            "* ",
            "+ ",
            "1. ",
            "2) ",
            "> ",
            "    ",
            "<u>",
            "</u>",
            "<b>",
            "[",
            "]",
            "(",
            ")",
            "](x)",
            "![",
            "\\",
            "\\*",
            "&amp;",
            "&",
            "|",
            "---",
            "===",
            "<!-- c -->",
            "<div>",
            "é",
            "🙂",
            "[^1]",
            "$x$",
            "- [ ] ",
        ];
        let mut rng = Rng(0x9E3779B97F4A7C15);
        for _ in 0..20_000 {
            let n = 1 + rng.next() % 24;
            let src: String = (0..n).map(|_| rng.pick(&pieces)).collect();
            let out = serialize(&parse(&src));
            assert_eq!(out, src, "round trip failed for {src:?}");
        }
    }

    #[test]
    fn fuzz_editor_lines_read_back() {
        // Whatever the user types and formats must load back with the same
        // look. Lines that cannot be expressed are allowed to load raw, but
        // never to change their text.
        let words = [
            "a", "word", " ", "*", "_", "#", "-", "1.", "[x]", "<", "&", "\\", "`", "é", "!",
        ];
        let styles = [
            Style::NONE,
            Style::BOLD,
            Style::ITALIC,
            Style::UNDERLINE,
            Style::BOLD.with(Style::ITALIC),
            Style::BOLD.with(Style::UNDERLINE),
        ];
        let kinds = [
            LineKind::Text,
            LineKind::H1,
            LineKind::H2,
            LineKind::bullet(0),
            LineKind::bullet(2),
        ];
        let mut rng = Rng(0xDEADBEEF12345678);
        let mut rich = 0;
        let mut unformatted = 0;
        for _ in 0..20_000 {
            let kind = kinds[(rng.next() % kinds.len() as u64) as usize];
            let spans: Vec<Span> = (0..1 + rng.next() % 6)
                .map(|_| {
                    let style = styles[(rng.next() % styles.len() as u64) as usize];
                    let text: String = (0..1 + rng.next() % 3).map(|_| rng.pick(&words)).collect();
                    Span::new(text, style)
                })
                .collect();
            let spans = merge(spans);
            let md = serialize_line(kind, &spans);
            let line = &parse(&md)[0];
            let text: String = spans.iter().map(|s| s.text.as_str()).collect();
            if line.kind == LineKind::Raw {
                assert_eq!(line.text(), md);
            } else {
                rich += 1;
                assert_eq!(line.text(), text, "text changed: {spans:?} -> {md:?}");
                if md == render(kind, &spans, Mode::TEXT_ONLY)
                    && normalized(&line.spans) != normalized(&spans)
                {
                    // e.g. `*!*é`: CommonMark has no way to italicise
                    // punctuation glued to a letter; the text survives
                    unformatted += 1;
                } else if !text.starts_with(char::is_whitespace)
                    && !text.ends_with(char::is_whitespace)
                {
                    assert_eq!(
                        normalized(&line.spans),
                        normalized(&spans),
                        "style changed: {spans:?} -> {md:?}"
                    );
                }
            }
        }
        assert!(rich > 15_000, "too many lines fell back to raw: {rich}");
        // this corpus is mostly punctuation; real text is checked below
        assert!(
            unformatted < 4_000,
            "too many lines lost formatting: {unformatted}"
        );
    }

    #[test]
    fn fuzz_realistic_formatting_survives() {
        let words = [
            "hello",
            "world",
            "Friday",
            "dentist",
            "é",
            "42",
            " ",
            " ",
            " ",
            ", ",
            ". ",
            "!",
            "?",
            "(maybe)",
            "C#",
            "5 * 3",
            "snake_case",
            "a&b",
            "x<y",
            "it's",
            "\\path",
        ];
        let styles = [
            Style::NONE,
            Style::BOLD,
            Style::ITALIC,
            Style::UNDERLINE,
            Style::BOLD.with(Style::ITALIC),
        ];
        let kinds = [
            LineKind::Text,
            LineKind::H1,
            LineKind::H2,
            LineKind::bullet(0),
        ];
        let mut rng = Rng(0x1234_5678_9ABC_DEF1);
        for _ in 0..20_000 {
            let kind = kinds[(rng.next() % kinds.len() as u64) as usize];
            // formatting is applied to whole words, as a person would
            let spans: Vec<Span> = (0..1 + rng.next() % 8)
                .flat_map(|i| {
                    let style = styles[(rng.next() % styles.len() as u64) as usize];
                    let mut word = rng.pick(&words);
                    if i == 0 && word.trim().is_empty() {
                        word = "hello";
                    }
                    let gap = if i == 0 { "" } else { " " };
                    [Span::new(gap, Style::NONE), Span::new(word, style)]
                })
                .collect();
            let spans = merge(spans);
            let md = serialize_line(kind, &spans);
            let line = &parse(&md)[0];
            assert_ne!(line.kind, LineKind::Raw, "{spans:?} -> {md:?}");
            let text: String = spans.iter().map(|s| s.text.as_str()).collect();
            assert_eq!(line.text(), text, "{md:?}");
            if !text.starts_with(' ') && !text.ends_with(' ') {
                assert_eq!(
                    normalized(&line.spans),
                    normalized(&spans),
                    "{spans:?} -> {md:?}"
                );
            }
        }
    }

    #[test]
    fn search_sees_rendered_text() {
        let text = searchable("# Things\nCall the **dentist** before <u>Friday</u>.\n- a *b*\n");
        assert!(text.contains("Call the dentist before Friday."), "{text:?}");
        assert!(text.contains("a b"), "{text:?}");
        assert!(!text.contains("**"));
    }

    #[test]
    fn titles() {
        assert_eq!(
            title("# Things to fix\nbody").as_deref(),
            Some("Things to fix")
        );
        assert_eq!(
            title("intro\n# **Bold** title\n").as_deref(),
            Some("Bold title")
        );
        assert_eq!(
            title("```\n# not a title\n```\n# Real\n").as_deref(),
            Some("Real")
        );
        assert_eq!(title("no heading"), None);
    }
}
