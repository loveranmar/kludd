//! Markdown <-> Kludd's line model.
//!
//! One editor line is exactly one line of the file. Every line is either
//! rich (plain text, a `#` heading, a `- bullet`, a `1.` numbered item, a
//! `- [ ]` task or a `> quote`, with bold, italic, underline and
//! strikethrough inside) or raw (kept byte for byte and shown as written).
//!
//! Round-tripping is guaranteed by construction: a line is only treated as
//! rich if serializing what was parsed reproduces the original line exactly.
//! Everything else (code blocks, tables, rules, math, HTML, ...) stays raw,
//! so opening and saving a file never changes it.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::OnceLock;

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
    /// `~~struck through~~`.
    pub const STRIKE: Style = Style(128);

    /// The bits that change how text looks.
    pub const MARKUP: Style = Style(1 | 2 | 4 | 128);

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
    H3,
    H4,
    H5,
    H6,
    /// `indent` leading spaces, then `marker` (`-`, `*` or `+`) and a space,
    /// then its box if it is a task.
    Bullet {
        indent: u8,
        marker: u8,
        task: Option<Task>,
    },
    /// `indent` leading spaces, `number`, `delim` (`.` or `)`) and a space,
    /// then its box if it is a task. `number` is only what is written:
    /// Markdown counts a list on from its first item, and so does Kludd.
    Numbered {
        indent: u8,
        number: u32,
        delim: u8,
        task: Option<Task>,
    },
    /// `depth` `>`s; bit `i` of `spaces` is set when the `i`th is followed
    /// by a space.
    Quote {
        depth: u8,
        spaces: u8,
    },
    /// Unsupported Markdown, kept exactly as written.
    Raw,
}

/// A task list item's box: `[ ]`, or ticked, `[x]` (or `[X]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Task {
    Open,
    /// Ticked, with the letter it was ticked with.
    Done(u8),
}

impl Task {
    fn markdown(task: Option<Task>) -> String {
        match task {
            None => String::new(),
            Some(Task::Open) => "[ ] ".into(),
            Some(Task::Done(c)) => format!("[{}] ", c as char),
        }
    }
}

impl LineKind {
    pub fn bullet(indent: u8) -> LineKind {
        LineKind::Bullet {
            indent,
            marker: b'-',
            task: None,
        }
    }

    pub fn numbered(indent: u8, number: u32) -> LineKind {
        LineKind::Numbered {
            indent,
            number,
            delim: b'.',
            task: None,
        }
    }

    /// A quote `depth` deep, written `> > `.
    pub fn quote(depth: u8) -> LineKind {
        LineKind::Quote {
            depth,
            spaces: ((1u16 << depth) - 1) as u8,
        }
    }

    pub fn heading(level: u8) -> LineKind {
        match level {
            1 => LineKind::H1,
            2 => LineKind::H2,
            3 => LineKind::H3,
            4 => LineKind::H4,
            5 => LineKind::H5,
            _ => LineKind::H6,
        }
    }

    pub fn heading_level(self) -> Option<u8> {
        match self {
            LineKind::H1 => Some(1),
            LineKind::H2 => Some(2),
            LineKind::H3 => Some(3),
            LineKind::H4 => Some(4),
            LineKind::H5 => Some(5),
            LineKind::H6 => Some(6),
            _ => None,
        }
    }

    pub fn is_list(self) -> bool {
        matches!(self, LineKind::Bullet { .. } | LineKind::Numbered { .. })
    }

    pub fn list_indent(self) -> Option<u8> {
        match self {
            LineKind::Bullet { indent, .. } | LineKind::Numbered { indent, .. } => Some(indent),
            _ => None,
        }
    }

    /// The same list item at another indent.
    pub fn with_indent(self, indent: u8) -> LineKind {
        match self {
            LineKind::Bullet { marker, task, .. } => LineKind::Bullet {
                indent,
                marker,
                task,
            },
            LineKind::Numbered {
                number,
                delim,
                task,
                ..
            } => LineKind::Numbered {
                indent,
                number,
                delim,
                task,
            },
            other => other,
        }
    }

    /// A list item's box, if it is a task.
    pub fn task(self) -> Option<Task> {
        match self {
            LineKind::Bullet { task, .. } | LineKind::Numbered { task, .. } => task,
            _ => None,
        }
    }

    pub fn with_task(self, task: Option<Task>) -> LineKind {
        match self {
            LineKind::Bullet { indent, marker, .. } => LineKind::Bullet {
                indent,
                marker,
                task,
            },
            LineKind::Numbered {
                indent,
                number,
                delim,
                ..
            } => LineKind::Numbered {
                indent,
                number,
                delim,
                task,
            },
            other => other,
        }
    }

    /// Where a list item's text starts in the file: a line indented this
    /// far is inside the item (a nested item, say).
    pub fn content_column(self) -> usize {
        match self {
            LineKind::Bullet { indent, .. } => indent as usize + 2,
            LineKind::Numbered { indent, number, .. } => {
                indent as usize + number.to_string().len() + 2
            }
            _ => 0,
        }
    }

    /// The kind of a line started under this one with Enter: a list goes on
    /// (an open box, the next number), a heading is followed by text.
    pub fn next(self) -> LineKind {
        match self {
            LineKind::Numbered {
                indent,
                number,
                delim,
                task,
            } => LineKind::Numbered {
                indent,
                number: number.saturating_add(1).min(999_999_999),
                delim,
                task: task.map(|_| Task::Open),
            },
            LineKind::Bullet { task, .. } => self.with_task(task.map(|_| Task::Open)),
            k if k.heading_level().is_some() => LineKind::Text,
            other => other,
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
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_MATH
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

    // what a top-level block may hold and still be rich lines
    #[derive(Clone, Copy, PartialEq)]
    enum Top {
        /// A paragraph or a one-line `#` heading: inline formatting only.
        Inline,
        /// Items of simple lines, nested lists.
        List,
        /// Paragraphs and quotes in quotes.
        Quote,
    }
    let mut depth = 0usize;
    let mut block: Option<(Range<usize>, Option<Top>)> = None;
    for (event, range) in Parser::new_ext(src, options()).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                if depth == 0 {
                    let top = match &tag {
                        Tag::Paragraph => Some(Top::Inline),
                        Tag::Heading { .. }
                            if src[range.clone()].starts_with('#')
                                && !src[range.clone()].trim_end().contains(['\n', '\r']) =>
                        {
                            Some(Top::Inline)
                        }
                        Tag::List(_) => Some(Top::List),
                        Tag::BlockQuote(None) => Some(Top::Quote),
                        _ => None,
                    };
                    block = Some((range.clone(), top));
                } else if let Some((_, top)) = block.as_mut() {
                    let inline = matches!(
                        tag,
                        Tag::Emphasis
                            | Tag::Strong
                            | Tag::Strikethrough
                            | Tag::Link { .. }
                            | Tag::Image { .. }
                    );
                    let simple = inline
                        || match top {
                            Some(Top::List) => {
                                matches!(tag, Tag::Item | Tag::Paragraph | Tag::List(_))
                            }
                            Some(Top::Quote) => {
                                matches!(tag, Tag::Paragraph | Tag::BlockQuote(None))
                            }
                            _ => false,
                        };
                    if !simple {
                        *top = None;
                    }
                }
                depth += 1;
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some((range, top)) = block.take()
                {
                    mark(&mut class, &line_of, range, top.is_some());
                }
            }
            Event::Rule | Event::DisplayMath(_) | Event::Html(_) if depth == 0 => {
                mark(&mut class, &line_of, range, false);
            }
            // display math in a paragraph spans lines of its own
            Event::DisplayMath(_) => {
                if let Some((_, top)) = block.as_mut() {
                    *top = None;
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

/// Plain text (a `.txt` file): every line raw, so nothing is formatting
/// and `\*` stays two characters.
pub fn parse_plain(src: &str) -> Vec<Line> {
    split_lines(src)
        .into_iter()
        .map(|(range, eol)| raw_line(&src[range], eol))
        .collect()
}

/// Recognises the line prefix: `#` to `######`, `>`s, a bullet or a
/// number (and a task box after either), or nothing.
fn line_prefix(content: &str) -> (LineKind, usize) {
    let bytes = content.as_bytes();
    let hashes = bytes.iter().take_while(|&&b| b == b'#').count();
    if (1..=6).contains(&hashes) && bytes.get(hashes) == Some(&b' ') {
        return (LineKind::heading(hashes as u8), hashes + 1);
    }
    if content.starts_with('>') {
        let (mut i, mut depth, mut spaces) = (0, 0u8, 0u8);
        while bytes.get(i) == Some(&b'>') && depth < 8 {
            i += 1;
            if bytes.get(i) == Some(&b' ') {
                spaces |= 1 << depth;
                i += 1;
            }
            depth += 1;
        }
        return (LineKind::Quote { depth, spaces }, i);
    }
    let indent = content.len() - content.trim_start_matches(' ').len();
    if indent > u8::MAX as usize {
        return (LineKind::Text, 0);
    }
    let rest = &bytes[indent..];
    if rest.len() >= 2 && matches!(rest[0], b'-' | b'*' | b'+') && rest[1] == b' ' {
        let (task, len) = task_box(&rest[2..]);
        let kind = LineKind::Bullet {
            indent: indent as u8,
            marker: rest[0],
            task,
        };
        return (kind, indent + 2 + len);
    }
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    // a leading zero would be lost: such items stay as written
    if (1..=9).contains(&digits)
        && !(rest[0] == b'0' && digits > 1)
        && matches!(rest.get(digits), Some(b'.' | b')'))
        && rest.get(digits + 1) == Some(&b' ')
    {
        let number = content[indent..indent + digits].parse().unwrap_or(0);
        let (task, len) = task_box(&rest[digits + 2..]);
        let kind = LineKind::Numbered {
            indent: indent as u8,
            number,
            delim: rest[digits],
            task,
        };
        return (kind, indent + digits + 2 + len);
    }
    (LineKind::Text, 0)
}

/// `[ ] `, `[x] ` or `[X] ` after a list marker.
fn task_box(rest: &[u8]) -> (Option<Task>, usize) {
    match rest {
        [b'[', b' ', b']', b' ', ..] => (Some(Task::Open), 4),
        [b'[', c @ (b'x' | b'X'), b']', b' ', ..] => (Some(Task::Done(*c)), 4),
        _ => (None, 0),
    }
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
        // the prefix in its plainest form: only its structure matters here
        let shown_prefix = match kind {
            LineKind::Bullet { task, .. } => format!("- {}", Task::markdown(task)),
            LineKind::Numbered { task, .. } => format!("1. {}", Task::markdown(task)),
            LineKind::Quote { depth, .. } => "> ".repeat(depth as usize),
            k => match k.heading_level() {
                Some(level) => format!("{} ", "#".repeat(level as usize)),
                None => String::new(),
            },
        };
        let source = format!("{shown_prefix}{core}");
        spans.extend(parse_inline(&source, shown_prefix.len(), kind)?);
    }
    if !trail.is_empty() {
        spans.push(Span::new(trail, Style::RAW));
    }
    Some((kind, merge(spans)))
}

/// What a line's prefix opens, in the order the parser reports it.
#[derive(Clone, Copy)]
enum Opening {
    Heading(HeadingLevel),
    Bullets,
    Numbers,
    Item,
    /// A task box, ticked or not (an event, not a tag: nothing closes it).
    Task(bool),
    Quote,
    Paragraph,
}

impl Opening {
    fn matches(self, event: &Event) -> bool {
        match (self, event) {
            (Opening::Heading(want), Event::Start(Tag::Heading { level, .. })) => *level == want,
            (Opening::Bullets, Event::Start(Tag::List(None))) => true,
            (Opening::Numbers, Event::Start(Tag::List(Some(_)))) => true,
            (Opening::Item, Event::Start(Tag::Item)) => true,
            (Opening::Task(want), Event::TaskListMarker(done)) => *done == want,
            (Opening::Quote, Event::Start(Tag::BlockQuote(None))) => true,
            (Opening::Paragraph, Event::Start(Tag::Paragraph)) => true,
            _ => false,
        }
    }

    fn of(kind: LineKind) -> Vec<Opening> {
        let task = |task: Option<Task>| task.map(|t| Opening::Task(t != Task::Open));
        match kind {
            LineKind::Bullet { task: t, .. } => {
                [Some(Opening::Bullets), Some(Opening::Item), task(t)]
                    .into_iter()
                    .flatten()
                    .collect()
            }
            LineKind::Numbered { task: t, .. } => {
                [Some(Opening::Numbers), Some(Opening::Item), task(t)]
                    .into_iter()
                    .flatten()
                    .collect()
            }
            LineKind::Quote { depth, .. } => {
                let mut out = vec![Opening::Quote; depth as usize];
                out.push(Opening::Paragraph);
                out
            }
            k => match k.heading_level() {
                Some(level) => vec![Opening::Heading(
                    HeadingLevel::try_from(level as usize).unwrap_or(HeadingLevel::H6),
                )],
                None => vec![Opening::Paragraph],
            },
        }
    }
}

fn parse_inline(src: &str, offset: usize, kind: LineKind) -> Option<Vec<Span>> {
    let mut events = Parser::new_ext(src, options()).into_offset_iter();

    // The block structure must be exactly what the prefix promises.
    let opening = Opening::of(kind);
    for check in &opening {
        let (event, _) = events.next()?;
        if !check.matches(&event) {
            return None;
        }
    }
    let closing = opening
        .iter()
        .filter(|o| !matches!(o, Opening::Task(_)))
        .count();

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
            // `~~x~~`; a single `~x~` is kept as written
            Event::Start(Tag::Strikethrough) if src[range.start..].starts_with("~~") => {
                stack.push(style);
                style = style.with(Style::STRIKE);
            }
            Event::End(TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough) => {
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
    // `>` and then a space typed: the space after `>` is the prefix's, so
    // the one typed needs another before it
    let kind = match kind {
        LineKind::Quote { depth, spaces }
            if depth > 0
                && spaces & (1 << (depth - 1)) == 0
                && spans
                    .first()
                    .is_some_and(|s| s.text.starts_with([' ', '\t'])) =>
        {
            LineKind::Quote {
                depth,
                spaces: spaces | 1 << (depth - 1),
            }
        }
        _ => kind,
    };
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
        (
            LineKind::Bullet {
                indent: x, task: s, ..
            },
            LineKind::Bullet {
                indent: y, task: t, ..
            },
        ) => x == y && s == t,
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
    '\\', '*', '_', '`', '[', ']', '<', '>', '~', '&', '!', '#', '|', '$',
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
        LineKind::Bullet {
            indent,
            marker,
            task,
        } => format!(
            "{}{} {}",
            " ".repeat(indent as usize),
            marker as char,
            Task::markdown(task)
        ),
        LineKind::Numbered {
            indent,
            number,
            delim,
            task,
        } => format!(
            "{}{number}{} {}",
            " ".repeat(indent as usize),
            delim as char,
            Task::markdown(task)
        ),
        LineKind::Quote { depth, spaces } => (0..depth)
            .map(|i| if spaces & (1 << i) != 0 { "> " } else { ">" })
            .collect(),
        k => match k.heading_level() {
            Some(level) => format!("{} ", "#".repeat(level as usize)),
            None => String::new(),
        },
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
    // (digits written inside markup, `**1.**`, start no list)
    let indent = chars.iter().take_while(|(c, _)| c.is_whitespace()).count();
    let bare = |i: usize| markup[i] == Style::NONE && !chars[i].1.has(Style::RAW);
    let digits = (indent..chars.len())
        .take_while(|&i| chars[i].0.is_ascii_digit() && bare(i))
        .count();
    let list_dot = (mode.escape
        && digits > 0
        && matches!(chars.get(indent + digits), Some(('.' | ')', _)))
        && bare(indent + digits))
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
        let mut missing: Vec<(usize, Style)> =
            [Style::UNDERLINE, Style::BOLD, Style::ITALIC, Style::STRIKE]
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
    } else if flag == Style::STRIKE {
        ("~~", "~~")
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
        '\\' | '*' | '_' | '`' | '[' | ']' | '<' | '~' | '&' | '!' | '$'
    ) || (line_start && matches!(c, '#' | '>' | '-' | '+' | '=' | '|'))
}

// ---------------------------------------------------------------------------
// What the editor draws on top of the lines. None of it changes what is
// saved: code blocks, tables, rules and math are raw lines, and links and
// code spans are raw runs inside rich lines.

/// Where a raw line sits in a block Kludd draws: a code block, a table,
/// display math, a rule, a heading underlined with `===`, a quote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Block {
    #[default]
    None,
    /// The ``` line that opens a code block.
    FenceOpen,
    Code,
    /// The ``` line that closes it.
    FenceClose,
    /// Code indented four spaces; the first and last line of its block.
    Indented {
        first: bool,
        last: bool,
    },
    TableHead,
    /// The `|---|---|` line under the head.
    TableRule,
    TableRow,
    /// The last row of a table.
    TableEnd,
    /// The `$$` lines around display math, and the math between them.
    MathOpen,
    Math,
    MathClose,
    /// `$$ x $$` on a line of its own.
    MathLine,
    /// A paragraph line with a `===` (level 1) or `---` (level 2) line
    /// under it: a heading.
    SetextText {
        level: u8,
    },
    SetextRule {
        level: u8,
    },
    /// `---`, `***` or `___`: a line across.
    Rule,
    /// A quote kept as written (`> - a list in it`, say), `depth` deep.
    Quote {
        depth: u8,
    },
}

impl Block {
    pub fn is_code(self) -> bool {
        matches!(
            self,
            Block::FenceOpen | Block::Code | Block::FenceClose | Block::Indented { .. }
        )
    }

    pub fn is_table(self) -> bool {
        matches!(
            self,
            Block::TableHead | Block::TableRule | Block::TableRow | Block::TableEnd
        )
    }

    pub fn is_math(self) -> bool {
        matches!(
            self,
            Block::MathOpen | Block::Math | Block::MathClose | Block::MathLine
        )
    }

    /// In a box: code, tables and display math.
    pub fn is_boxed(self) -> bool {
        self.is_code() || self.is_table() || self.is_math()
    }

    /// Lines drawn as a block of their own, kept as written.
    pub fn is_opaque(self) -> bool {
        matches!(
            self,
            Block::FenceOpen
                | Block::Code
                | Block::FenceClose
                | Block::Indented { .. }
                | Block::MathOpen
                | Block::Math
                | Block::MathClose
                | Block::MathLine
                | Block::TableRule
                | Block::SetextRule { .. }
                | Block::Rule
        )
    }
}

/// `next` goes on the block `prev` is in.
pub fn continues(prev: Block, next: Block) -> bool {
    use Block::*;
    matches!(
        (prev, next),
        (FenceOpen | Code, Code | FenceClose)
            | (Indented { last: false, .. }, Indented { .. })
            | (MathOpen | Math, Math | MathClose)
            | (SetextText { .. }, SetextRule { .. })
            | (TableHead, TableRule)
            | (TableRule | TableRow, TableRow | TableEnd)
    )
}

/// Finds the blocks among the lines, given as (kind, text). Only raw lines
/// start blocks, though inside a code block left open rich lines are code,
/// as Markdown has it, and a rich line can be a heading's text.
pub fn blocks(lines: &[(LineKind, &str)]) -> Vec<Block> {
    let raw = |n: usize| {
        lines
            .get(n)
            .and_then(|&(kind, text)| (kind == LineKind::Raw).then_some(text))
    };
    let blank = |n: usize| lines.get(n).is_none_or(|(_, text)| text.trim().is_empty());
    let mut out = vec![Block::None; lines.len()];
    let mut n = 0;
    while n < lines.len() {
        // a paragraph line underlined with === or ---
        if let Some(level) = raw(n + 1).and_then(setext_underline)
            && paragraph(lines[n])
        {
            out[n] = Block::SetextText { level };
            out[n + 1] = Block::SetextRule { level };
            n += 2;
            continue;
        }
        let Some(line) = raw(n) else {
            n += 1;
            continue;
        };
        if let Some(open) = fence(line) {
            out[n] = Block::FenceOpen;
            n += 1;
            while n < lines.len() {
                let closes = raw(n).is_some_and(|l| closes_fence(l, open));
                out[n] = if closes {
                    Block::FenceClose
                } else {
                    Block::Code
                };
                n += 1;
                if closes {
                    break;
                }
            }
            continue;
        }
        // $$ ... $$ (one left open is just text until it is closed)
        if math_fence(line)
            && let Some(close) = (n + 1..lines.len())
                .take_while(|&m| raw(m).is_some())
                .find(|&m| raw(m).is_some_and(math_fence))
        {
            out[n] = Block::MathOpen;
            out[n + 1..close].fill(Block::Math);
            out[close] = Block::MathClose;
            n = close + 1;
            continue;
        }
        if let Some(rule) = raw(n + 1)
            && table_rule(rule)
            && table_row(line)
            && table_cells(line).len() == table_cells(rule).len()
        {
            out[n] = Block::TableHead;
            out[n + 1] = Block::TableRule;
            n += 2;
            while let Some(row) = raw(n)
                && table_row(row)
            {
                out[n] = Block::TableRow;
                n += 1;
            }
            if out[n - 1] == Block::TableRow {
                out[n - 1] = Block::TableEnd;
            }
            continue;
        }
        // four spaces in, after a blank line (not in a list item: there it
        // is the item's text)
        let after_list = || {
            (0..n)
                .rev()
                .find(|&m| !blank(m))
                .is_some_and(|m| lines[m].0.is_list())
        };
        if indented(line) && (n == 0 || blank(n - 1)) && !after_list() {
            let mut end = n;
            while raw(end).is_some_and(|l| indented(l) || l.trim().is_empty()) {
                end += 1;
            }
            while end > n + 1 && blank(end - 1) {
                end -= 1;
            }
            for (m, block) in out.iter_mut().enumerate().take(end).skip(n) {
                *block = Block::Indented {
                    first: m == n,
                    last: m + 1 == end,
                };
            }
            n = end;
            continue;
        }
        let trimmed = line.trim();
        if trimmed.len() > 4 && trimmed.starts_with("$$") && trimmed.ends_with("$$") {
            out[n] = Block::MathLine;
        } else if thematic_break(line) {
            out[n] = Block::Rule;
        } else if let Some(depth) = quote_depth(line) {
            out[n] = Block::Quote { depth };
        }
        n += 1;
    }
    out
}

/// Where Tab and Shift+Tab move list item `n`: the indent that puts it
/// inside the item before it at its level (`None` if there is none), and
/// the indent that takes it out a level (`None` at the top already).
pub fn list_moves(kinds: &[LineKind], n: usize) -> (Option<u8>, Option<u8>) {
    let Some(indent) = kinds.get(n).and_then(|k| k.list_indent()) else {
        return (None, None);
    };
    let mut start = n;
    while start > 0 && kinds[start - 1].is_list() {
        start -= 1;
    }
    // the items open above, as (indent, where their text starts)
    let mut open: Vec<(usize, usize)> = Vec::new();
    for kind in &kinds[start..n] {
        let i = kind.list_indent().unwrap_or(0) as usize;
        while open.last().is_some_and(|o| i < o.1) {
            open.pop();
        }
        open.push((i, kind.content_column()));
    }
    let indent = indent as usize;
    let mut sibling = None;
    while open.last().is_some_and(|o| indent < o.1) {
        sibling = open.pop();
    }
    let inside = sibling.map(|(_, col)| col.min(u8::MAX as usize) as u8);
    let out = match open.last() {
        Some(&(parent, _)) => Some(parent as u8),
        None => (indent > 0).then_some(0),
    };
    (inside, out)
}

/// Up to three spaces in, as block syntax may be.
fn unindent(line: &str) -> Option<&str> {
    let rest = line.trim_start_matches(' ');
    (line.len() - rest.len() <= 3).then_some(rest)
}

/// `===` (level 1) or `---` (level 2) under a paragraph line.
fn setext_underline(line: &str) -> Option<u8> {
    let rest = unindent(line)?.trim_end();
    let c = rest.chars().next()?;
    (matches!(c, '=' | '-') && rest.chars().all(|x| x == c)).then_some(if c == '=' { 1 } else { 2 })
}

/// A line that can be a paragraph's: text, or raw text that starts no
/// other block.
fn paragraph((kind, text): (LineKind, &str)) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    match kind {
        LineKind::Text => true,
        LineKind::Raw => unindent(text).is_some_and(|rest| {
            !rest.starts_with(['>', '#', '<', '|', '`', '~', '$'])
                && !thematic_break(text)
                && line_prefix(text).0 == LineKind::Text
        }),
        _ => false,
    }
}

/// `---`, `***`, `___` (spaces between allowed), three or more.
fn thematic_break(line: &str) -> bool {
    let Some(rest) = unindent(line) else {
        return false;
    };
    let Some(c) = rest.chars().next().filter(|c| matches!(c, '-' | '*' | '_')) else {
        return false;
    };
    rest.chars().all(|x| x == c || x == ' ' || x == '\t') && rest.matches(c).count() >= 3
}

fn math_fence(line: &str) -> bool {
    unindent(line).is_some_and(|rest| rest.trim_end() == "$$")
}

fn indented(line: &str) -> bool {
    (line.starts_with("    ") || line.starts_with('\t')) && !line.trim().is_empty()
}

/// How many `>`s a quote line starts with.
fn quote_depth(line: &str) -> Option<u8> {
    let mut rest = unindent(line)?;
    let mut depth = 0u8;
    while let Some(after) = rest.strip_prefix('>') {
        depth = depth.saturating_add(1);
        rest = after.strip_prefix(' ').unwrap_or(after);
    }
    (depth > 0).then_some(depth)
}

/// Where a quote line's text starts, after its `>`s.
pub fn quote_text(line: &str) -> usize {
    let Some(mut rest) = unindent(line) else {
        return 0;
    };
    while let Some(after) = rest.strip_prefix('>') {
        rest = after.strip_prefix(' ').unwrap_or(after);
    }
    line.len() - rest.len()
}

/// The leading spaces or tab that make a line indented code.
pub fn code_indent(line: &str) -> usize {
    if line.starts_with('\t') { 1 } else { 4 }
}

/// GitHub's alerts: a quote starting with `[!NOTE]` and the like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alert {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

impl Alert {
    /// `[!NOTE]` (any case), alone on the line.
    pub fn parse(text: &str) -> Option<Alert> {
        let name = text.trim().strip_prefix("[!")?.strip_suffix(']')?;
        Some(match name.to_ascii_uppercase().as_str() {
            "NOTE" => Alert::Note,
            "TIP" => Alert::Tip,
            "IMPORTANT" => Alert::Important,
            "WARNING" => Alert::Warning,
            "CAUTION" => Alert::Caution,
            _ => return None,
        })
    }
}

/// What a line looks like beyond its kind and block, worked out from the
/// lines around it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Shape {
    /// How deep a list item is nested.
    pub level: u8,
    /// The number a numbered item shows, and the digits of the widest
    /// number in its list (they all line up after it).
    pub number: u32,
    pub digits: u8,
    /// The first and last line of a quote.
    pub quote_first: bool,
    pub quote_last: bool,
    /// The alert a quote is.
    pub alert: Option<Alert>,
}

/// Works out the shapes. `text(n)` is line `n`'s text; it is only asked for
/// lines that are neither list items nor quotes, inside a list, and for
/// the first line of a quote.
pub fn shapes(kinds: &[LineKind], blocks: &[Block], text: impl Fn(usize) -> String) -> Vec<Shape> {
    let mut out = vec![Shape::default(); kinds.len()];

    // lists nest by indentation, as Markdown reads it: an item indented as
    // far as its parent's text is inside it
    struct Open {
        col: usize,
        list: usize,
        /// Delimiter and number shown, for a numbered item.
        number: Option<(u8, u32)>,
    }
    let mut stack: Vec<Open> = Vec::new();
    let mut digits: Vec<u8> = Vec::new();
    let mut list_of = vec![None; kinds.len()];
    for (n, &kind) in kinds.iter().enumerate() {
        let Some(indent) = kind.list_indent() else {
            if stack.is_empty() {
                continue;
            }
            if matches!(kind, LineKind::Text | LineKind::Raw) {
                let line = text(n);
                // a blank line keeps a list going
                if line.trim().is_empty() {
                    continue;
                }
                let lead = line.len() - line.trim_start().len();
                while stack.last().is_some_and(|o| lead < o.col) {
                    stack.pop();
                }
            } else {
                stack.clear();
            }
            continue;
        };
        let indent = indent as usize;
        // the item this one follows in the same list, if any
        let mut sibling = None;
        while stack.last().is_some_and(|o| indent < o.col) {
            sibling = stack.pop();
        }
        let number = match kind {
            LineKind::Numbered { number, delim, .. } => Some((delim, number)),
            _ => None,
        };
        let (list, number) = match (sibling, number) {
            (Some(s), Some((delim, _))) if s.number.is_some_and(|(d, _)| d == delim) => {
                let shown = s.number.map_or(0, |(_, n)| n).saturating_add(1);
                (s.list, Some((delim, shown)))
            }
            (Some(s), None) if s.number.is_none() => (s.list, None),
            (_, number) => {
                digits.push(0);
                (digits.len() - 1, number)
            }
        };
        if let Some((_, shown)) = number {
            digits[list] = digits[list].max(shown.to_string().len() as u8);
        }
        out[n].level = stack.len().min(u8::MAX as usize) as u8;
        out[n].number = number.map_or(0, |(_, shown)| shown);
        list_of[n] = Some(list);
        stack.push(Open {
            col: kind.content_column(),
            list,
            number,
        });
    }
    for (n, list) in list_of.into_iter().enumerate() {
        if let (Some(list), LineKind::Numbered { .. }) = (list, kinds[n]) {
            out[n].digits = digits[list];
        }
    }

    // quotes: lines in a row; an alert if the first says so
    let quoted = |n: usize| match (kinds[n], blocks.get(n).copied()) {
        (LineKind::Quote { depth, .. }, _) | (_, Some(Block::Quote { depth })) => depth,
        _ => 0,
    };
    let mut n = 0;
    while n < kinds.len() {
        if quoted(n) == 0 {
            n += 1;
            continue;
        }
        let first = n;
        while n < kinds.len() && quoted(n) > 0 {
            n += 1;
        }
        let alert = if quoted(first) == 1 {
            let line = text(first);
            let start = if kinds[first] == LineKind::Raw {
                quote_text(&line)
            } else {
                0
            };
            Alert::parse(&line[start..])
        } else {
            None
        };
        for (m, shape) in out.iter_mut().enumerate().take(n).skip(first) {
            shape.quote_first = m == first;
            shape.quote_last = m + 1 == n;
            shape.alert = alert;
        }
    }
    out
}

/// An opening code fence: at most three spaces, then three or more
/// backticks or tildes. Returns the fence character and how many.
fn fence(line: &str) -> Option<(char, usize)> {
    let rest = line.trim_start_matches(' ');
    if line.len() - rest.len() > 3 {
        return None;
    }
    let c = rest.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let len = rest.chars().take_while(|&x| x == c).count();
    // a backtick fence's info string has no backticks (```x``` is inline code)
    (len >= 3 && !(c == '`' && rest[len..].contains('`'))).then_some((c, len))
}

fn closes_fence(line: &str, (c, len): (char, usize)) -> bool {
    let rest = line.trim_start_matches(' ');
    let run = rest.chars().take_while(|&x| x == c).count();
    line.len() - rest.len() <= 3 && run >= len && rest[run..].trim().is_empty()
}

/// The fence that closes a code block opened by `line`, if `line` opens one.
pub fn closing_fence(line: &str) -> Option<String> {
    fence(line).map(|(c, len)| c.to_string().repeat(len))
}

/// Char offsets of a table row's pipes; `\|` is a pipe inside a cell.
pub fn table_pipes(line: &str) -> Vec<usize> {
    let chars: Vec<char> = line.chars().collect();
    pipes(&chars)
}

fn pipes(chars: &[char]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '|' => out.push(i),
            _ => {}
        }
        i += 1;
    }
    out
}

/// Char ranges of a table row's cells: the text between its pipes. A pipe
/// at the very start or end starts or ends no cell.
pub fn table_cells(line: &str) -> Vec<Range<usize>> {
    let chars: Vec<char> = line.chars().collect();
    cells(&chars)
}

fn cells(chars: &[char]) -> Vec<Range<usize>> {
    let start = chars.iter().take_while(|c| c.is_whitespace()).count();
    let end = chars.len()
        - chars[start..]
            .iter()
            .rev()
            .take_while(|c| c.is_whitespace())
            .count();
    let mut out = Vec::new();
    let mut from = start;
    for p in pipes(chars) {
        if p != start {
            out.push(from..p);
        }
        from = p + 1;
    }
    if from < end {
        out.push(from..end);
    }
    out
}

/// A line with a pipe in it: a table row, if it follows a table.
pub fn table_row(line: &str) -> bool {
    !table_pipes(line).is_empty()
}

/// The `| --- | :-: |` line under a table's head.
pub fn table_rule(line: &str) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let cells = cells(&chars);
    !pipes(&chars).is_empty()
        && !cells.is_empty()
        && cells.iter().all(|range| {
            let cell: String = chars[range.clone()].iter().collect();
            let cell = cell.trim();
            let dashes = cell.trim_start_matches(':').trim_end_matches(':');
            !dashes.is_empty() && dashes.chars().all(|c| c == '-') && cell.len() - dashes.len() <= 2
        })
}

/// Lines up a table: every row gets every column, each cell padded to its
/// column's width with a space either side, and pipes at both ends. Row 1
/// is the rule. `cursor` (row, char offset) comes back as its new offset
/// in that row, still in the same place of its cell's text. In the cursor's
/// cell, spaces before the cursor are kept: they may be between two words.
pub fn format_table(
    rows: &[String],
    cursor: Option<(usize, usize)>,
) -> (Vec<String>, Option<usize>) {
    let rows: Vec<Vec<char>> = rows.iter().map(|r| r.chars().collect()).collect();
    let mut cursor_cell = None;
    let texts: Vec<Vec<String>> = rows
        .iter()
        .enumerate()
        .map(|(r, chars)| {
            let ranges = cells(chars);
            ranges
                .iter()
                .enumerate()
                .map(|(c, range)| {
                    let lead = chars[range.clone()]
                        .iter()
                        .take_while(|c| c.is_whitespace())
                        .count();
                    let start = range.start + lead;
                    let trail = chars[start..range.end]
                        .iter()
                        .rev()
                        .take_while(|c| c.is_whitespace())
                        .count();
                    let mut end = range.end - trail;
                    if let Some((row, at)) = cursor
                        && row == r
                        && (range.start..=range.end).contains(&at)
                        && cursor_cell.is_none()
                    {
                        end = end.max(at);
                        cursor_cell = Some((r, c, at.saturating_sub(start).min(end - start)));
                    }
                    chars[start..end].iter().collect()
                })
                .collect()
        })
        .collect();

    let cols = texts.iter().map(Vec::len).max().unwrap_or(0).max(1);
    let widths: Vec<usize> = (0..cols)
        .map(|c| {
            texts
                .iter()
                .enumerate()
                .filter(|(r, _)| *r != 1)
                .map(|(_, row)| row.get(c).map_or(0, |t| t.chars().count()))
                .max()
                .unwrap_or(0)
                .max(3)
        })
        .collect();

    let mut moved = None;
    let out: Vec<String> = texts
        .iter()
        .enumerate()
        .map(|(r, row)| {
            let mut line = String::from("|");
            let mut len = 1;
            for (c, &width) in widths.iter().enumerate() {
                let text = row.get(c).map_or("", String::as_str);
                let cell = if r == 1 {
                    rule_cell(text, width)
                } else {
                    text.to_string()
                };
                let n = cell.chars().count();
                if let Some((cr, cc, offset)) = cursor_cell
                    && (cr, cc) == (r, c)
                {
                    moved = Some(len + 1 + offset.min(n));
                }
                line.push(' ');
                line.push_str(&cell);
                line.extend(std::iter::repeat_n(' ', width.saturating_sub(n)));
                line.push_str(" |");
                len += width.max(n) + 3;
            }
            line
        })
        .collect();

    // a cursor outside every cell: before the first pipe, or after the last
    if let Some((row, at)) = cursor
        && moved.is_none()
        && let Some(line) = out.get(row)
    {
        let first = cells(&rows[row]).first().map_or(0, |r| r.start);
        moved = Some(if at < first { 0 } else { line.chars().count() });
    }
    (out, moved)
}

fn rule_cell(text: &str, width: usize) -> String {
    let left = text.starts_with(':');
    let right = text.len() > 1 && text.ends_with(':');
    let dashes = width - left as usize - right as usize;
    format!(
        "{}{}{}",
        if left { ":" } else { "" },
        "-".repeat(dashes),
        if right { ":" } else { "" }
    )
}

/// The cell after (or before) the one at `offset` in `row`, skipping the
/// rule: where its text ends, as (row, char offset). `None` past either end.
pub fn table_step(
    rows: &[String],
    row: usize,
    offset: usize,
    back: bool,
) -> Option<(usize, usize)> {
    let mut all = Vec::new();
    for (r, line) in rows.iter().enumerate() {
        if r == 1 {
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        for range in cells(&chars) {
            let trail = chars[range.clone()]
                .iter()
                .rev()
                .take_while(|c| c.is_whitespace())
                .count();
            let end = range.end - trail;
            // an empty cell: just inside it, after its first space
            let end = if end == range.start {
                (range.start + 1).min(range.end)
            } else {
                end
            };
            all.push((r, range, end));
        }
    }
    let in_row = |r: usize| all.iter().filter(move |(cr, ..)| *cr == r);
    let here = if row == 1 {
        // the rule: between the head and the first row
        in_row(0).count().checked_sub(1)?
    } else {
        let mine: Vec<usize> = (0..all.len()).filter(|&i| all[i].0 == row).collect();
        *mine
            .iter()
            .find(|&&i| all[i].1.start <= offset && offset <= all[i].1.end)
            .or_else(|| mine.iter().find(|&&i| offset < all[i].1.start))
            .or(mine.last())?
    };
    let next = if back { here.checked_sub(1)? } else { here + 1 };
    all.get(next).map(|(r, _, end)| (*r, *end))
}

/// What an inline thing the editor draws is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlineKind {
    Link,
    Url,
    Code,
    Kbd,
    /// `![alt](src)` or `<img src alt>`: `text` is the alt text, `url` the
    /// source.
    Image,
    /// Formatting written out in raw text (a table cell, a quote kept as
    /// written, a link's text, HTML tags).
    Bold,
    Italic,
    Strike,
    Underline,
    /// `==marked==` or `<mark>`.
    Highlight,
    Sup,
    Sub,
    /// `$math$`, and `$$math$$` inside a line.
    Math,
    /// A footnote reference, `[^1]`: `text` is its label.
    Footnote,
    /// `[^1]:` starting a footnote's text.
    FootnoteDef,
    /// `:smile:`; `url` is the emoji.
    Emoji,
    /// `<!-- a comment -->`.
    Comment,
    /// Any other HTML tag: only its syntax is drawn, faintly.
    Tag,
    /// A line break written out: a backslash ending the line, or `<br>`.
    Break,
}

/// Something drawn inside a line, in char offsets. `text` is what reads
/// as its content; the rest of `whole` is syntax.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inline {
    pub kind: InlineKind,
    pub whole: Range<usize>,
    pub text: Range<usize>,
    /// Where a link or image goes, or the emoji; empty otherwise.
    pub url: String,
}

impl Inline {
    fn new(kind: InlineKind, whole: Range<usize>, text: Range<usize>) -> Inline {
        Inline {
            kind,
            whole,
            text,
            url: String::new(),
        }
    }

    /// Whether `i` is taken by this, so nothing else is found there: all
    /// of a code span or link, just the syntax of formatting around text.
    fn claims(&self, i: usize) -> bool {
        use InlineKind::*;
        let around = matches!(
            self.kind,
            Bold | Italic | Strike | Underline | Highlight | Sup | Sub
        );
        self.whole.contains(&i) && !(around && self.text.contains(&i))
    }
}

fn claimed(out: &[Inline], i: usize) -> bool {
    out.iter().any(|x| x.claims(i))
}

/// Finds what is drawn inside a line: links, bare URLs, code spans,
/// `<kbd>` keys, images, math, footnotes, HTML and the formatting in raw
/// text. Only raw runs are Markdown (text typed in Kludd is literal), but
/// a bare URL, a footnote, a highlight or an emoji reads as one wherever it
/// is, as it would on GitHub.
pub fn inlines(spans: &[Span]) -> Vec<Inline> {
    let markdown = spans
        .iter()
        .any(|s| s.style.has(Style::RAW) && !s.text.trim().is_empty());
    let hint = |s: &Span| s.text.contains([':', '=', '[', '\\']);
    if !markdown && !spans.iter().any(hint) {
        return Vec::new();
    }
    let chars: Vec<(char, bool)> = spans
        .iter()
        .flat_map(|s| {
            let raw = s.style.has(Style::RAW);
            s.text.chars().map(move |c| (c, raw))
        })
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].1 {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i].1 {
            i += 1;
        }
        let run: String = chars[start..i].iter().map(|(c, _)| c).collect();
        raw_inlines(&run, start, &mut out);
    }
    kbd_keys(&chars, &mut out);
    html_tags(&chars, &mut out);
    footnotes(&chars, &mut out);
    bare_urls(&chars, &mut out);
    highlights(&chars, &mut out);
    emoji_codes(&chars, &mut out);
    // a backslash ending the line breaks it (and an escaped one is literal)
    if let Some(last) = spans.last()
        && last.text.ends_with('\\')
        && !last.style.has(Style::ESCAPED)
    {
        let n = chars.len();
        let slashes = chars.iter().rev().take_while(|(c, _)| *c == '\\').count();
        if (slashes % 2 == 1 || !last.style.has(Style::RAW)) && !claimed(&out, n - 1) {
            out.push(Inline::new(InlineKind::Break, n - 1..n, n..n));
        }
    }
    out.sort_by_key(|i| i.whole.start);
    out
}

/// Links, code spans, images, math and formatting in a run of Markdown
/// source starting at char `base`.
fn raw_inlines(run: &str, base: usize, out: &mut Vec<Inline>) {
    let lead = run.len() - run.trim_start().len();
    let src = &run[lead..];
    let at = |byte: usize| base + run[..lead + byte].chars().count();
    // open links and images: whole source, destination, extent of the text
    type Open = (InlineKind, Range<usize>, String, Option<Range<usize>>);
    let mut links: Vec<Open> = Vec::new();
    let mut marks: Vec<(InlineKind, Range<usize>)> = Vec::new();
    for (event, range) in Parser::new_ext(src, options()).into_offset_iter() {
        let in_image = links.iter().any(|(k, ..)| *k == InlineKind::Image);
        match &event {
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                ..
            }) if !in_image => {
                let url = if *link_type == pulldown_cmark::LinkType::Email {
                    format!("mailto:{dest_url}")
                } else {
                    dest_url.to_string()
                };
                links.push((InlineKind::Link, range, url, None));
                continue;
            }
            Event::Start(Tag::Image { dest_url, .. }) if !in_image => {
                links.push((InlineKind::Image, range, dest_url.to_string(), None));
                continue;
            }
            Event::End(TagEnd::Link | TagEnd::Image)
                if links.last().is_some_and(|(k, ..)| {
                    (*k == InlineKind::Image) == matches!(event, Event::End(TagEnd::Image))
                }) =>
            {
                if let Some((kind, whole, url, text)) = links.pop() {
                    let open = if kind == InlineKind::Image { 2 } else { 1 };
                    let text = text.unwrap_or(whole.start + open..whole.start + open);
                    out.push(Inline {
                        kind,
                        whole: at(whole.start)..at(whole.end),
                        text: at(text.start)..at(text.end),
                        url,
                    });
                }
                continue;
            }
            _ if in_image => {}
            Event::Start(tag @ (Tag::Strong | Tag::Emphasis | Tag::Strikethrough)) => {
                let kind = match tag {
                    Tag::Strong => InlineKind::Bold,
                    Tag::Emphasis => InlineKind::Italic,
                    _ => InlineKind::Strike,
                };
                marks.push((kind, range.clone()));
            }
            Event::End(TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough) => {
                if let Some((kind, whole)) = marks.pop() {
                    // the delimiter run: `**`, `_`, `~~`...
                    let first = src.as_bytes()[whole.start];
                    let n = src[whole.clone()]
                        .bytes()
                        .take_while(|&b| b == first)
                        .count()
                        .min(if kind == InlineKind::Italic { 1 } else { 2 });
                    if whole.len() > 2 * n {
                        out.push(Inline::new(
                            kind,
                            at(whole.start)..at(whole.end),
                            at(whole.start + n)..at(whole.end - n),
                        ));
                    }
                }
            }
            Event::Code(_) => {
                let ticks = src[range.clone()].chars().take_while(|&c| c == '`').count();
                out.push(Inline::new(
                    InlineKind::Code,
                    at(range.start)..at(range.end),
                    at(range.start + ticks)..at(range.end - ticks),
                ));
            }
            Event::InlineMath(_) | Event::DisplayMath(_) => {
                let dollars = src[range.clone()]
                    .chars()
                    .take_while(|&c| c == '$')
                    .count()
                    .min(range.len() / 2);
                out.push(Inline::new(
                    InlineKind::Math,
                    at(range.start)..at(range.end),
                    at(range.start + dollars)..at(range.end - dollars),
                ));
            }
            _ => {}
        }
        // everything inside a link is its text
        if let Some((_, _, _, text)) = links.last_mut() {
            *text = Some(match text.take() {
                Some(t) => t.start.min(range.start)..t.end.max(range.end),
                None => range,
            });
        }
    }
}

/// `<kbd>key</kbd>`, with the tags written as Markdown (raw).
fn kbd_keys(chars: &[(char, bool)], out: &mut Vec<Inline>) {
    let lower: Vec<char> = chars.iter().map(|(c, _)| c.to_ascii_lowercase()).collect();
    let raw_at = |i: usize, tag: &str| {
        let n = tag.chars().count();
        i + n <= chars.len()
            && lower[i..i + n].iter().copied().eq(tag.chars())
            && chars[i..i + n].iter().all(|(_, raw)| *raw)
    };
    let mut i = 0;
    while i < chars.len() {
        if !raw_at(i, "<kbd>") {
            i += 1;
            continue;
        }
        let text = i + 5;
        let close = (text..chars.len()).find(|&j| chars[j].0 == '<');
        match close {
            Some(j) if j > text && raw_at(j, "</kbd>") => {
                out.push(Inline::new(InlineKind::Kbd, i..j + 6, text..j));
                i = j + 6;
            }
            _ => i = text,
        }
    }
}

/// An HTML tag written as Markdown (raw) at `i`: its end, name (lower
/// case), whether it closes, and its attributes' source.
fn html_tag(chars: &[(char, bool)], i: usize) -> Option<(usize, String, bool, String)> {
    if chars.get(i)?.0 != '<' || !chars[i].1 {
        return None;
    }
    let mut j = i + 1;
    let closing = chars.get(j)?.0 == '/';
    if closing {
        j += 1;
    }
    let name_start = j;
    while chars
        .get(j)
        .is_some_and(|(c, _)| c.is_ascii_alphanumeric() || *c == '-')
    {
        j += 1;
    }
    if j == name_start || !chars[name_start].0.is_ascii_alphabetic() {
        return None;
    }
    let name: String = chars[name_start..j]
        .iter()
        .map(|(c, _)| c.to_ascii_lowercase())
        .collect();
    let attrs_start = j;
    let mut quote = None;
    while let Some(&(c, raw)) = chars.get(j) {
        if !raw {
            return None;
        }
        match (quote, c) {
            (None, '>') => break,
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            _ => {}
        }
        j += 1;
    }
    chars.get(j)?;
    let attrs = chars[attrs_start..j].iter().map(|(c, _)| c).collect();
    Some((j + 1, name, closing, attrs))
}

/// The value of attribute `name` in a tag's attributes.
fn attribute(attrs: &str, name: &str) -> Option<String> {
    let lower = attrs.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find(name) {
        let at = from + found;
        from = at + name.len();
        let before = lower[..at].chars().last();
        if before.is_some_and(|c| !c.is_whitespace()) {
            continue;
        }
        let rest = attrs[from..].trim_start().strip_prefix('=')?.trim_start();
        return Some(match rest.chars().next()? {
            q @ ('"' | '\'') => rest[1..].split(q).next()?.to_string(),
            _ => rest
                .split(|c: char| c.is_whitespace() || c == '/')
                .next()?
                .to_string(),
        });
    }
    None
}

/// Inline HTML and comments written as Markdown (raw).
fn html_tags(chars: &[(char, bool)], out: &mut Vec<Inline>) {
    let text: Vec<char> = chars.iter().map(|(c, _)| *c).collect();
    let starts_with = |i: usize, s: &str| {
        let n = s.chars().count();
        i + n <= text.len() && text[i..i + n].iter().copied().eq(s.chars())
    };
    let mut i = 0;
    while i < chars.len() {
        if chars[i].0 != '<' || !chars[i].1 || claimed(out, i) {
            i += 1;
            continue;
        }
        if starts_with(i, "<!--") {
            let end = (i + 4..text.len())
                .find(|&j| starts_with(j, "-->"))
                .map_or(text.len(), |j| j + 3);
            let inner = (i + 4).min(end)..end.saturating_sub(3).max(i + 4).min(end);
            out.push(Inline::new(InlineKind::Comment, i..end, inner));
            i = end;
            continue;
        }
        let Some((end, name, closing, attrs)) = html_tag(chars, i) else {
            i += 1;
            continue;
        };
        if name == "kbd" {
            i = end;
            continue;
        }
        let void = matches!(name.as_str(), "br" | "img" | "hr" | "wbr") || attrs.ends_with('/');
        let kind = match name.as_str() {
            "b" | "strong" | "summary" => Some(InlineKind::Bold),
            "i" | "em" | "cite" | "var" => Some(InlineKind::Italic),
            "s" | "del" | "strike" => Some(InlineKind::Strike),
            "u" | "ins" => Some(InlineKind::Underline),
            "mark" => Some(InlineKind::Highlight),
            "sup" => Some(InlineKind::Sup),
            "sub" => Some(InlineKind::Sub),
            "code" | "samp" => Some(InlineKind::Code),
            "a" => Some(InlineKind::Link),
            _ => None,
        };
        // an element closed later on the line draws its content; anything
        // else is just a tag
        let close = (!closing && !void && kind.is_some())
            .then(|| {
                let mut j = end;
                while j < chars.len() {
                    if let Some((e, n, true, _)) = html_tag(chars, j) {
                        if n == name {
                            return Some((j, e));
                        }
                        j = e;
                    } else {
                        j += 1;
                    }
                }
                None
            })
            .flatten();
        match (kind, close) {
            (Some(kind), Some((j, e))) => {
                let mut item = Inline::new(kind, i..e, end..j);
                if kind == InlineKind::Link {
                    item.url = attribute(&attrs, "href").unwrap_or_default();
                }
                out.push(item);
                i = end;
            }
            _ if name == "img" => {
                let mut item = Inline::new(InlineKind::Image, i..end, end..end);
                item.url = attribute(&attrs, "src").unwrap_or_default();
                out.push(item);
                i = end;
            }
            _ => {
                let kind = if name == "br" {
                    InlineKind::Break
                } else {
                    InlineKind::Tag
                };
                out.push(Inline::new(kind, i..end, end..end));
                i = end;
            }
        }
    }
}

/// `[^label]` references, and `[^label]:` starting a footnote's text.
fn footnotes(chars: &[(char, bool)], out: &mut Vec<Inline>) {
    let text: Vec<char> = chars.iter().map(|(c, _)| *c).collect();
    let mut i = 0;
    while i + 3 < text.len() {
        if text[i] != '[' || text[i + 1] != '^' || claimed(out, i) {
            i += 1;
            continue;
        }
        let label = i + 2;
        let Some(close) = (label..text.len())
            .take_while(|&j| !text[j].is_whitespace() && text[j] != '[')
            .find(|&j| text[j] == ']')
            .filter(|&j| j > label)
        else {
            i += 1;
            continue;
        };
        // a footnote's text is a raw line: typed, `[^1]:` is just text
        let at_start = text[..i].iter().all(|c| *c == ' ') && i <= 3 && chars[i].1;
        if at_start && text.get(close + 1) == Some(&':') {
            out.push(Inline::new(
                InlineKind::FootnoteDef,
                i..close + 2,
                label..close,
            ));
            i = close + 2;
        } else {
            out.push(Inline::new(
                InlineKind::Footnote,
                i..close + 1,
                label..close,
            ));
            i = close + 1;
        }
    }
}

/// `==marked==`, as Obsidian and Typora have it.
fn highlights(chars: &[(char, bool)], out: &mut Vec<Inline>) {
    let text: Vec<char> = chars.iter().map(|(c, _)| *c).collect();
    let pair = |i: usize| i + 1 < text.len() && text[i] == '=' && text[i + 1] == '=';
    let free = |out: &[Inline], i: usize| !claimed(out, i) && !claimed(out, i + 1);
    let mut i = 0;
    while i + 4 < text.len() {
        let opens = pair(i)
            && (i == 0 || text[i - 1] != '=')
            && !text[i + 2].is_whitespace()
            && text[i + 2] != '='
            && free(out, i);
        if !opens {
            i += 1;
            continue;
        }
        let close = (i + 3..text.len().saturating_sub(1)).find(|&j| {
            pair(j) && !text[j - 1].is_whitespace() && text.get(j + 2) != Some(&'=') && free(out, j)
        });
        match close {
            Some(j) => {
                out.push(Inline::new(InlineKind::Highlight, i..j + 2, i + 2..j));
                i = j + 2;
            }
            None => i += 2,
        }
    }
}

/// `:shortcode:` emoji, as GitHub has them.
fn emoji_codes(chars: &[(char, bool)], out: &mut Vec<Inline>) {
    let text: Vec<char> = chars.iter().map(|(c, _)| *c).collect();
    let name_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || "_+-".contains(c);
    let mut i = 0;
    while i + 2 < text.len() {
        if text[i] != ':' || (i > 0 && text[i - 1].is_alphanumeric()) || claimed(out, i) {
            i += 1;
            continue;
        }
        let end = (i + 1..text.len())
            .take_while(|&j| name_char(text[j]) || text[j] == ':')
            .find(|&j| text[j] == ':');
        let Some(end) = end.filter(|&j| j > i + 1) else {
            i += 1;
            continue;
        };
        let name: String = text[i + 1..end].iter().collect();
        let after = text.get(end + 1);
        match emoji(&name) {
            Some(emoji) if !after.is_some_and(|c| c.is_alphanumeric()) && !claimed(out, end) => {
                let mut item = Inline::new(InlineKind::Emoji, i..end + 1, i..end + 1);
                item.url = emoji.to_string();
                out.push(item);
                i = end + 1;
            }
            _ => i = end,
        }
    }
}

/// The emoji for a GitHub shortcode (`smile` gives 😄).
pub fn emoji(name: &str) -> Option<&'static str> {
    static TABLE: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table: Vec<(&str, &str)> = include_str!("../assets/emoji.txt")
            .lines()
            .filter_map(|line| line.split_once(' '))
            .collect();
        table.sort_unstable_by_key(|(name, _)| *name);
        table
    });
    table
        .binary_search_by_key(&name, |(n, _)| n)
        .ok()
        .map(|i| table[i].1)
}

/// A link reference definition, `[label]: https://…`: its label (as
/// Markdown matches labels, case and spacing aside) and destination.
pub fn reference_definition(line: &str) -> Option<(String, String)> {
    let rest = unindent(line)?.strip_prefix('[')?;
    let close = rest.find("]:")?;
    let label = &rest[..close];
    if label.trim().is_empty() || label.starts_with('^') || label.contains(['[', ']']) {
        return None;
    }
    let dest = rest[close + 2..].split_whitespace().next()?;
    let dest = dest
        .strip_prefix('<')
        .and_then(|d| d.strip_suffix('>'))
        .unwrap_or(dest);
    Some((reference_label(label), dest.to_string()))
}

fn reference_label(label: &str) -> String {
    label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Links to the note's reference definitions: `[text][label]`,
/// `[label][]` and `[label]`, where `label` is defined in `refs`.
pub fn reference_links(spans: &[Span], refs: &HashMap<String, String>, out: &mut Vec<Inline>) {
    if refs.is_empty() {
        return;
    }
    let text: Vec<char> = spans.iter().flat_map(|s| s.text.chars()).collect();
    // `[`...`]` from `open`, with no brackets between: where `]` is
    let close = |open: usize| {
        (open + 1..text.len())
            .take_while(|&j| text[j] != '[')
            .find(|&j| text[j] == ']')
    };
    let found = out.len();
    let mut i = 0;
    while i < text.len() {
        if text[i] != '[' || text.get(i + 1) == Some(&'^') || claimed(&out[..found], i) {
            i += 1;
            continue;
        }
        let Some(first) = close(i) else {
            i += 1;
            continue;
        };
        let inner: String = text[i + 1..first].iter().collect();
        let (end, label) = match text.get(first + 1) {
            Some('[') => match close(first + 1) {
                Some(second) if second == first + 2 => (second + 1, inner.clone()),
                Some(second) => (second + 1, text[first + 2..second].iter().collect()),
                None => {
                    i = first + 1;
                    continue;
                }
            },
            // `[x](…)` is an inline link, `[x]:` a definition
            Some('(' | ':') => {
                i = first + 1;
                continue;
            }
            _ => (first + 1, inner.clone()),
        };
        match refs.get(&reference_label(&label)) {
            Some(url) if !inner.trim().is_empty() && !claimed(&out[..found], end - 1) => {
                let mut item = Inline::new(InlineKind::Link, i..end, i + 1..first);
                item.url = url.clone();
                out.push(item);
                i = end;
            }
            _ => i = first + 1,
        }
    }
    out.sort_by_key(|i| i.whole.start);
}

/// `https://…` written out, outside what was found already. Punctuation at
/// the end (`see https://x.y.`) is not part of it.
fn bare_urls(chars: &[(char, bool)], out: &mut Vec<Inline>) {
    let text: Vec<char> = chars.iter().map(|(c, _)| *c).collect();
    let starts_with = |i: usize, s: &str| {
        let n = s.chars().count();
        i + n <= text.len() && text[i..i + n].iter().copied().eq(s.chars())
    };
    let found = out.len();
    let mut i = 0;
    while i < text.len() {
        let scheme = ["https://", "http://"]
            .into_iter()
            .find(|s| starts_with(i, s))
            .map(|s| s.len());
        let fresh = i == 0 || !text[i - 1].is_alphanumeric();
        let Some(scheme) = scheme.filter(|_| fresh && !claimed(&out[..found], i)) else {
            i += 1;
            continue;
        };
        let mut end = i;
        while end < text.len()
            && !text[end].is_whitespace()
            && text[end] != '<'
            && !claimed(&out[..found], end)
        {
            end += 1;
        }
        loop {
            let last = text[end - 1];
            let unbalanced = last == ')' && {
                let url = &text[i..end];
                url.iter().filter(|&&c| c == '(').count()
                    < url.iter().filter(|&&c| c == ')').count()
            };
            if ".,:;!?*_~'\"".contains(last) || unbalanced {
                end -= 1;
            } else {
                break;
            }
        }
        if end > i + scheme {
            let mut item = Inline::new(InlineKind::Url, i..end, i..end);
            item.url = text[i..end].iter().collect();
            out.push(item);
        }
        i = end.max(i + 1);
    }
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
        assert!(raw.contains(&"---".to_string()));
        assert!(raw.contains(&"<div>".to_string()));
        assert!(raw.contains(&"[ref]: https://example.com".to_string()));
        // quotes, numbered lists and deeper headings are rich lines now
        let kind_of = |text: &str| lines.iter().find(|l| l.text() == text).map(|l| l.kind);
        assert_eq!(kind_of("quote"), Some(LineKind::quote(1)));
        assert_eq!(kind_of("one"), Some(LineKind::numbered(0, 1)));
        assert_eq!(kind_of("deep"), Some(LineKind::H3));
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
    fn plain_text_is_never_markdown() {
        for src in [
            "# not a heading\r\n- not a bullet\n**not bold** 5 \\* 3\n\n    indented\n",
            "no final newline",
            "",
            "\n\n",
            "a\rb\r\n",
        ] {
            let lines = parse_plain(src);
            assert!(lines.iter().all(|l| l.kind == LineKind::Raw), "{lines:#?}");
            assert_eq!(serialize(&lines), src);
        }
        assert_eq!(parse_plain("**x**")[0].text(), "**x**");
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
                marker: b'*',
                task: None,
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
            Style::STRIKE,
            Style::BOLD.with(Style::ITALIC),
            Style::BOLD.with(Style::UNDERLINE),
        ];
        let kinds = [
            LineKind::Text,
            LineKind::H1,
            LineKind::H2,
            LineKind::H4,
            LineKind::bullet(0),
            LineKind::bullet(2),
            LineKind::numbered(0, 3),
            LineKind::bullet(0).with_task(Some(Task::Open)),
            LineKind::numbered(3, 1).with_task(Some(Task::Done(b'x'))),
            LineKind::quote(1),
            LineKind::quote(2),
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

    fn blocks_of(src: &str) -> Vec<Block> {
        let lines = parse(src);
        let texts: Vec<String> = lines.iter().map(Line::text).collect();
        let lines: Vec<(LineKind, &str)> = lines
            .iter()
            .zip(&texts)
            .map(|(l, t)| (l.kind, t.as_str()))
            .collect();
        blocks(&lines)
    }

    #[test]
    fn finds_code_blocks_and_tables() {
        use Block::*;
        let src = "Intro\n```rust\nlet a = 1;\n\n~~~\n````\n\n| a | b |\n|---|:-:|\n| 1 | 2 |\n| 3 | 4 |\nafter\n";
        assert_eq!(
            blocks_of(src),
            [
                None, FenceOpen, Code, Code, Code, FenceClose, None, TableHead, TableRule,
                TableRow, TableEnd, None
            ]
        );
        // an open block runs to the end; ~~~ is closed only by ~~~
        assert_eq!(
            blocks_of("~~~\n```\nx"),
            [FenceOpen, Code, Code],
            "unclosed"
        );
        // a head without its rule is no table, and ```x``` is inline code
        assert_eq!(blocks_of("| a | b |\nnext\n"), [None, None]);
        assert_eq!(blocks_of("```x```\n"), [None]);
        assert_eq!(closing_fence("````js"), Some("````".into()));
        assert_eq!(closing_fence("``x`"), Option::None);
    }

    #[test]
    fn table_cells_and_rules() {
        assert_eq!(table_cells("| a | b |"), [1..4, 5..8]);
        assert_eq!(table_cells("a | b"), [0..2, 3..5]);
        assert_eq!(table_cells("| a \\| b |").len(), 1);
        assert_eq!(table_pipes("| a \\| b |"), [0, 9]);
        assert!(table_rule("|---|:--:|"));
        assert!(table_rule(" --- | -: "));
        assert!(!table_rule("| a | - |"));
        assert!(!table_rule("---"), "a rule needs a pipe");
        assert!(!table_rule("|::|"));
    }

    #[test]
    fn tables_line_up() {
        let rows: Vec<String> = ["|Name|Age|", "|-|:-:|", "| Ada Lovelace | 36", "|x"]
            .map(String::from)
            .to_vec();
        let (out, _) = format_table(&rows, None);
        assert_eq!(
            out,
            [
                "| Name         | Age |",
                "| ------------ | :-: |",
                "| Ada Lovelace | 36  |",
                "| x            |     |",
            ]
        );
        // formatting a formatted table changes nothing
        assert_eq!(format_table(&out, None).0, out);

        // typing in a cell: the cursor stays after what was typed, and a
        // space typed between words survives
        let rows: Vec<String> = ["| a   | b   |", "| --- | --- |", "| one  | x   |"]
            .map(String::from)
            .to_vec();
        let (out, at) = format_table(&rows, Some((2, 6)));
        assert_eq!(out[2], "| one  | x   |");
        assert_eq!(at, Some(6));
        let rows: Vec<String> = ["| a   | b   |", "| --- | --- |", "| ones | x   |"]
            .map(String::from)
            .to_vec();
        let (out, at) = format_table(&rows, Some((2, 6)));
        assert_eq!(out, ["| a    | b   |", "| ---- | --- |", "| ones | x   |"]);
        assert_eq!(at, Some(6));
        // a pipe typed in a cell adds a column everywhere
        let rows: Vec<String> = ["| a | b |", "|---|---|", "| 1 | 2 | 3 |"]
            .map(String::from)
            .to_vec();
        let (out, _) = format_table(&rows, None);
        assert_eq!(out[0], "| a   | b   |     |");
        assert_eq!(out[1], "| --- | --- | --- |");
    }

    #[test]
    fn tab_walks_the_cells() {
        let rows: Vec<String> = ["| a   | bb  |", "| --- | --- |", "| 1   |     |"]
            .map(String::from)
            .to_vec();
        assert_eq!(table_step(&rows, 0, 3, false), Some((0, 10)));
        assert_eq!(
            table_step(&rows, 0, 9, false),
            Some((2, 3)),
            "skips the rule"
        );
        assert_eq!(
            table_step(&rows, 2, 3, false),
            Some((2, 8)),
            "into an empty cell"
        );
        assert_eq!(table_step(&rows, 2, 8, false), None, "past the end");
        assert_eq!(table_step(&rows, 2, 3, true), Some((0, 10)));
        assert_eq!(table_step(&rows, 0, 0, true), None);
        assert_eq!(table_step(&rows, 1, 4, false), Some((2, 3)));
    }

    fn found(spans: &[Span]) -> Vec<(InlineKind, String, String)> {
        let text: Vec<char> = spans.iter().flat_map(|s| s.text.chars()).collect();
        inlines(spans)
            .into_iter()
            .map(|i| (i.kind, text[i.text.clone()].iter().collect(), i.url))
            .collect()
    }

    #[test]
    fn finds_links_code_and_keys() {
        use InlineKind::*;
        let line = &parse(
            "See [the docs](https://x.y), `a | b`, <kbd>Ctrl</kbd> and https://kludd.dev/a_(b).\n",
        )[0];
        assert_eq!(line.kind, LineKind::Text);
        assert_eq!(
            found(&line.spans),
            [
                (Link, "the docs".into(), "https://x.y".into()),
                (Code, "a | b".into(), String::new()),
                (Kbd, "Ctrl".into(), String::new()),
                (
                    Url,
                    "https://kludd.dev/a_(b)".into(),
                    "https://kludd.dev/a_(b)".into()
                ),
            ]
        );
        // where the syntax is: everything around the text
        let inline = &inlines(&line.spans)[0];
        assert_eq!((inline.whole.clone(), inline.text.clone()), (4..27, 5..13));

        // text typed in Kludd is literal, except for a bare URL
        let typed = [Span::new("[x](y) `z` http://a.b", Style::NONE)];
        assert_eq!(
            found(&typed),
            [(Url, "http://a.b".into(), "http://a.b".into())]
        );

        // raw lines (a table row, a list in a quote) have them too, and
        // their formatting; autolinks and mail
        let row = &parse("> - [**b**](u) <https://a.b> <me@x.y> ``a`b``\n")[0];
        assert_eq!(row.kind, LineKind::Raw);
        assert_eq!(
            found(&row.spans),
            [
                (Link, "**b**".into(), "u".into()),
                (Bold, "b".into(), String::new()),
                (Link, "https://a.b".into(), "https://a.b".into()),
                (Link, "me@x.y".into(), "mailto:me@x.y".into()),
                (Code, "a`b".into(), String::new()),
            ]
        );
        // an image is no link
        assert_eq!(
            found(&parse("![alt](pic.png)\n")[0].spans),
            [(Image, "alt".into(), "pic.png".into())]
        );
    }

    #[test]
    fn new_kinds_round_trip() {
        let src = "### Three\n#### Four\n##### Five\n###### Six\n\
            1. one\n2. two\n10. ten\n3) paren\n   1. nested\n\
            - [ ] todo\n- [x] done\n- [X] Done\n  * [ ] deep\n1. [ ] numbered task\n- [ ] \n\
            > quote\n>\n> more *of* it\n>no space\n> > nested\n>> tight\n\n\
            ~~gone~~ and ~one~ tilde, $x^2$ math\n";
        round_trip(src);
        let lines = parse(src);
        let kinds: Vec<LineKind> = lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds[0..4],
            [LineKind::H3, LineKind::H4, LineKind::H5, LineKind::H6]
        );
        assert_eq!(kinds[4], LineKind::numbered(0, 1));
        assert_eq!(kinds[6], LineKind::numbered(0, 10));
        assert!(matches!(
            kinds[7],
            LineKind::Numbered {
                number: 3,
                delim: b')',
                ..
            }
        ));
        assert_eq!(kinds[8].list_indent(), Some(3));
        assert_eq!(kinds[9].task(), Some(Task::Open));
        assert_eq!(kinds[10].task(), Some(Task::Done(b'x')));
        assert_eq!(kinds[11].task(), Some(Task::Done(b'X')));
        assert!(matches!(
            kinds[12],
            LineKind::Bullet {
                indent: 2,
                marker: b'*',
                task: Some(Task::Open)
            }
        ));
        assert!(matches!(
            kinds[13],
            LineKind::Numbered {
                task: Some(Task::Open),
                ..
            }
        ));
        assert_eq!(kinds[14].task(), Some(Task::Open), "an empty task");
        assert_eq!(kinds[15], LineKind::quote(1));
        assert_eq!(
            kinds[16],
            LineKind::Quote {
                depth: 1,
                spaces: 0
            }
        );
        assert_eq!(lines[17].text(), "more of it");
        assert_eq!(
            kinds[18],
            LineKind::Quote {
                depth: 1,
                spaces: 0
            }
        );
        assert_eq!(kinds[19], LineKind::quote(2));
        assert_eq!(
            kinds[20],
            LineKind::Quote {
                depth: 2,
                spaces: 2
            }
        );
        // ~~x~~ is strikethrough; ~x~ and math stay as written
        let last = &lines[22];
        assert_eq!(last.kind, LineKind::Text);
        assert_eq!(last.spans[0], Span::new("gone", Style::STRIKE));
        assert!(
            last.spans
                .iter()
                .any(|s| s.text == "~one~" && s.style.has(Style::RAW))
        );
        assert!(
            last.spans
                .iter()
                .any(|s| s.text == "$x^2$" && s.style.has(Style::RAW))
        );
    }

    #[test]
    fn typed_text_in_new_kinds_stays_literal() {
        // what is typed reads back as typed, whatever the line is
        for (kind, text) in [
            (LineKind::quote(1), "> not deeper"),
            (LineKind::bullet(0), "[ ] not a task"),
            (LineKind::numbered(0, 1), "2. not nested"),
            (LineKind::Text, "costs $5-$10"),
            (LineKind::Text, "[^1]: not a footnote"),
            (LineKind::H3, "### not deeper"),
        ] {
            let spans = vec![Span::new(text, Style::NONE)];
            let md = serialize_line(kind, &spans);
            let line = &parse(&md)[0];
            assert_eq!((line.kind, line.text()), (kind, text.to_string()), "{md:?}");
        }
        assert_eq!(
            serialize_line(LineKind::Text, &[Span::new("x", Style::STRIKE)]),
            "~~x~~"
        );
        assert_eq!(
            serialize_line(
                LineKind::numbered(2, 7).with_task(Some(Task::Done(b'x'))),
                &[]
            ),
            "  7. [x] "
        );
    }

    fn kinds_and_blocks(src: &str) -> (Vec<LineKind>, Vec<Block>) {
        let lines = parse(src);
        let kinds = lines.iter().map(|l| l.kind).collect();
        (kinds, blocks_of(src))
    }

    #[test]
    fn finds_rules_headings_math_and_indented_code() {
        use Block::*;
        let src = "Title\n=====\n\nSub\n---\n\n***\n\n- - -\n\n    code\n\n    more\n\nafter\n\n$$\nx^2\n$$\n\n$$ y $$\n\n> - quoted list\n";
        round_trip(src);
        let (_, blocks) = kinds_and_blocks(src);
        assert_eq!(
            blocks,
            [
                SetextText { level: 1 },
                SetextRule { level: 1 },
                None,
                SetextText { level: 2 },
                SetextRule { level: 2 },
                None,
                Rule,
                None,
                Rule,
                None,
                Indented {
                    first: true,
                    last: false
                },
                Indented {
                    first: false,
                    last: false
                },
                Indented {
                    first: false,
                    last: true
                },
                None,
                None,
                None,
                MathOpen,
                Math,
                MathClose,
                None,
                MathLine,
                None,
                Quote { depth: 1 },
            ]
        );
        // $$ left open is no block yet; a list item's indented text is no code
        assert_eq!(blocks_of("$$\nx\n"), [None, None]);
        assert_eq!(blocks_of("- a\n\n    b\n"), [None, None, None]);
    }

    #[test]
    fn lists_nest_and_count_like_markdown() {
        let src = "1. a\n1. b\n   - c\n   - d\n     1. e\n1. f\n\n1. g\ntext\n5. h\n6. i\n- [ ] j\n  - k\n";
        let lines = parse(src);
        let kinds: Vec<LineKind> = lines.iter().map(|l| l.kind).collect();
        let texts: Vec<String> = lines.iter().map(Line::text).collect();
        let shapes = shapes(&kinds, &vec![Block::None; kinds.len()], |n| {
            texts[n].clone()
        });
        let numbers: Vec<(u8, u32)> = shapes.iter().map(|s| (s.level, s.number)).collect();
        assert_eq!(
            numbers,
            [
                (0, 1),
                (0, 2),
                (1, 0),
                (1, 0),
                (2, 1),
                (0, 3),
                (0, 0),
                (0, 4),
                (0, 0),
                (0, 5),
                (0, 6),
                (0, 0),
                (1, 0),
            ],
            "a blank line keeps a list going, text ends it; a list counts on from its first number"
        );
        assert_eq!(shapes[9].digits, 1);
        let long = (1..=12).map(|i| format!("{i}. x\n")).collect::<String>();
        let kinds: Vec<LineKind> = parse(&long).iter().map(|l| l.kind).collect();
        let wide = super::shapes(&kinds, &[Block::None; 12], |_| String::new());
        assert!(wide.iter().all(|s| s.digits == 2), "they line up after 12.");
        // Tab goes in under the item before, Shift+Tab out to the parent
        let kinds: Vec<LineKind> = parse("- a\n- b\n  - c\n10. d\n11. e\n")
            .iter()
            .map(|l| l.kind)
            .collect();
        assert_eq!(list_moves(&kinds, 1), (Some(2), None));
        assert_eq!(list_moves(&kinds, 2), (None, Some(0)));
        assert_eq!(list_moves(&kinds, 4), (Some(4), None));
        assert_eq!(list_moves(&kinds, 0), (None, None));
    }

    #[test]
    fn quotes_and_alerts() {
        let src = "> [!WARNING]\n> Careful.\n\n> plain\n> quote\n\n> [!nope]\n";
        let lines = parse(src);
        let kinds: Vec<LineKind> = lines.iter().map(|l| l.kind).collect();
        let texts: Vec<String> = lines.iter().map(Line::text).collect();
        let shapes = shapes(&kinds, &blocks_of(src), |n| texts[n].clone());
        assert_eq!(shapes[0].alert, Some(Alert::Warning));
        assert_eq!(shapes[1].alert, Some(Alert::Warning));
        assert!(shapes[0].quote_first && shapes[1].quote_last);
        assert_eq!(shapes[3].alert, None);
        assert!(shapes[3].quote_first && !shapes[3].quote_last && shapes[4].quote_last);
        assert_eq!(shapes[6].alert, None);
        assert_eq!(Alert::parse(" [!tip] "), Some(Alert::Tip));
    }

    #[test]
    fn finds_the_rest_inline() {
        use InlineKind::*;
        let spans = |src: &str| parse(src).remove(0).spans;
        assert_eq!(
            found(&spans("a ==mark== b and a == b == c\n")),
            [(Highlight, "mark".into(), String::new())]
        );
        assert_eq!(
            found(&spans("see[^note] and :tada: but not a:b:c or :nope:\n")),
            [
                (Footnote, "note".into(), String::new()),
                (Emoji, ":tada:".into(), "🎉".into()),
            ]
        );
        assert_eq!(
            found(&spans("[^note]: the text\n")),
            [(FootnoteDef, "note".into(), String::new())]
        );
        assert_eq!(
            found(&spans("x $a+b$ y\n")),
            [(Math, "a+b".into(), String::new())]
        );
        assert_eq!(
            found(&spans(
                "H<sub>2</sub>O <mark>m</mark> <br> <!-- c --> <a href=\"u\">t</a> <img src=\"p.png\">\n"
            )),
            [
                (Sub, "2".into(), String::new()),
                (Highlight, "m".into(), String::new()),
                (Break, String::new(), String::new()),
                (Comment, " c ".into(), String::new()),
                (Link, "t".into(), "u".into()),
                (Image, String::new(), "p.png".into()),
            ]
        );
        assert_eq!(
            found(&spans("a ~b~ c\n")),
            [(Strike, "b".into(), String::new())]
        );
        assert_eq!(
            found(&[Span::new("hard break\\", Style::NONE)]),
            [(Break, String::new(), String::new())]
        );
        assert!(found(&[Span::new("escaped\\", Style::ESCAPED)]).is_empty());
        // typed (not raw), `[^1]:` is text; `$$$$` is empty math
        assert_eq!(
            found(&[Span::new("[^1]: typed", Style::NONE)]),
            [(Footnote, "1".into(), String::new())]
        );
        let empty = inlines(&parse("a $$$$ b\n").remove(0).spans);
        assert!(empty.iter().all(|i| i.text.start <= i.text.end));
        assert_eq!(emoji("+1"), Some("👍"));
        assert_eq!(emoji("no_such_emoji"), None);
    }

    #[test]
    fn reference_links_find_their_definitions() {
        let src = "See [the docs][docs], [Docs][] and [docs], not [nope] or [x](y).\n\n[docs]: <https://x.y> \"Title\"\n";
        round_trip(src);
        let lines = parse(src);
        assert_eq!(lines[2].kind, LineKind::Raw);
        let (label, url) = reference_definition(&lines[2].text()).unwrap();
        assert_eq!((label.as_str(), url.as_str()), ("docs", "https://x.y"));
        let refs = HashMap::from([(label, url)]);
        let mut out = inlines(&lines[0].spans);
        reference_links(&lines[0].spans, &refs, &mut out);
        let links: Vec<(String, String)> = out
            .iter()
            .filter(|i| i.kind == InlineKind::Link)
            .map(|i| {
                (
                    lines[0]
                        .text()
                        .chars()
                        .skip(i.text.start)
                        .take(i.text.len())
                        .collect(),
                    i.url.clone(),
                )
            })
            .collect();
        assert_eq!(
            links,
            [
                ("the docs".to_string(), "https://x.y".to_string()),
                ("Docs".to_string(), "https://x.y".to_string()),
                ("docs".to_string(), "https://x.y".to_string()),
                ("x".to_string(), "y".to_string()),
            ]
        );
        assert_eq!(reference_definition("[^1]: a footnote"), None);
    }
}
