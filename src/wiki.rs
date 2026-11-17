//! The `jira_wiki` format: Jira wiki markup, as REST v2 takes and returns
//! descriptions and comments, converted to and from Markdown.
//!
//! [`to_markdown`] ports Tracon's SPA renderer (`spa/src/lib/jira-wiki.ts`):
//! the same line-based block parser and inline pass, writing Markdown where
//! that wrote HTML. It never writes HTML: text that Markdown would read as
//! markup is escaped, and only http(s), mailto and rooted links survive.
//! [`from_markdown`] walks a CommonMark (with GFM tables and strikethrough)
//! parse and writes the wiki markup for each construct.

use std::sync::LazyLock;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use regex::{Captures, Regex};

/// A held piece of finished Markdown, as one private-use character: the
/// inline pass's equivalent of the renderer's `\0N\0` holes.
const HOLE: u32 = 0xF0000;
const HOLE_END: u32 = 0xFFFFD;
/// A span's opening and closing marks, one character each.
const OPEN: u32 = 0xE000;
const CLOSE: u32 = 0xE100;
const SENTINEL_END: u32 = 0xE1FF;

fn is_hole(c: char) -> bool {
    (HOLE..=HOLE_END).contains(&(c as u32))
}

fn is_sentinel(c: char) -> bool {
    (OPEN..=SENTINEL_END).contains(&(c as u32))
}

fn hold(held: &mut Vec<String>, md: String) -> String {
    let i = held.len() as u32;
    held.push(md);
    char::from_u32(HOLE + i.min(HOLE_END - HOLE))
        .expect("a private-use character")
        .to_string()
}

macro_rules! re {
    ($name:ident, $pattern:expr) => {
        static $name: LazyLock<Regex> =
            LazyLock::new(|| Regex::new($pattern).expect("a valid pattern"));
    };
}

re!(FENCE, r"^\s*\{(code|noformat)(?::([^}]*))?\}(.*)$");
re!(QUOTE, r"^\s*\{quote\}(.*)$");
re!(HEADING, r"^\s*h([1-6])\.\s+(.*)$");
re!(BQ, r"^\s*bq\.\s+(.*)$");
re!(HR, r"^\s*-{4,}\s*$");
re!(LIST_ITEM, r"^\s*([*#]+|-)\s+(.*)$");
re!(CODE, r"\{\{(.+?)\}\}");
re!(ESCAPE, r"\\\\|\\([!-/:-@\[-`{-~])");
re!(COLOR, r"\{color:([^}]*)\}(.+?)\{color\}");
re!(LINK, r"\[([^\[\]]+)\]");
re!(
    BARE_URL,
    r#"\bhttps?://[^\s<>"'\[\]|{}\x00\x{E000}-\x{E1FF}\x{F0000}-\x{FFFFD}]+"#
);
re!(SCHEME, r"(?i)^[a-z][a-z0-9+.-]*:");

/// Where inline text lands, which decides what must be escaped.
#[derive(Clone, Copy, Default)]
struct Ctx {
    /// In a table cell, where `|` ends the cell.
    cell: bool,
    /// In a paragraph, where `\\` may break the line.
    breaks: bool,
}

/// The URL as given when its scheme is http, https or mailto, or it has
/// none; `None` otherwise. Control characters and spaces are ignored when
/// reading the scheme, as browsers ignore them.
fn safe_url(value: &str) -> Option<&str> {
    let href = value.trim();
    let compact: String = href
        .chars()
        .filter(|c| (*c as u32) > 0x20 && *c != '\u{7f}')
        .collect();
    if let Some(m) = SCHEME.find(&compact) {
        let scheme = m.as_str().trim_end_matches(':').to_ascii_lowercase();
        if !["http", "https", "mailto"].contains(&scheme.as_str()) {
            return None;
        }
    }
    Some(href)
}

/// A link destination Markdown reads back as the same URL.
fn destination(href: &str, ctx: Ctx) -> String {
    let mut out = String::new();
    for c in href.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '|' if ctx.cell => out.push_str("%7C"),
            '(' | ')' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// `` `code` `` with a fence longer than any backtick run inside it.
fn code_span(code: &str, ctx: Ctx) -> String {
    let code = if ctx.cell {
        code.replace('|', "\\|")
    } else {
        code.to_string()
    };
    let longest = code.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    let pad = if code.starts_with('`') || code.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{code}{pad}{fence}")
}

fn link(inner: &str, held: &mut Vec<String>, ctx: Ctx) -> Option<String> {
    let bar = inner.find('|');
    let text = bar.map(|b| &inner[..b]);
    let target = match bar {
        None => inner,
        Some(b) => inner[b + 1..].split('|').next().unwrap_or_default(),
    }
    .trim();
    if let Some(user) = target.strip_prefix('~') {
        return Some(format!("@{}", escape_text(user, ctx)));
    }
    let rooted = target.starts_with('/') && !target.starts_with("//");
    if !SCHEME.is_match(target) && !rooted {
        return text.map(|t| inline_into(t, held, ctx));
    }
    let label = text.map(|t| inline_into(t, held, ctx));
    let Some(href) = safe_url(target) else {
        return Some(label.unwrap_or_else(|| escape_text(target, ctx)));
    };
    Some(match label {
        None if SCHEME.is_match(href) && !href.contains(char::is_whitespace) => {
            format!("<{}>", href.replace('<', "%3C").replace('>', "%3E"))
        }
        None => format!("[{}]({})", escape_text(href, ctx), destination(href, ctx)),
        Some(label) => format!("[{label}]({})", destination(href, ctx)),
    })
}

/// JavaScript's `\w`, which the renderer's span boundaries are written in.
fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_line_end(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// The renderer's span pattern for marker `m`:
/// `(^|[^\w m])m(?=\S)(.+?)(?<=\S)m(?![\w m])`, applied left to right,
/// each match replaced with the span's own opening and closing marks.
fn spans(chars: Vec<char>, m: char, k: u32) -> Vec<char> {
    let n = chars.len();
    let close_from = |open: usize| -> Option<usize> {
        let first = *chars.get(open + 1)?;
        if first.is_whitespace() || is_line_end(first) {
            return None;
        }
        for c in open + 2..n {
            if is_line_end(chars[c]) {
                return None;
            }
            let after_ok = chars.get(c + 1).is_none_or(|&a| !is_word(a) && a != m);
            if chars[c] == m && !chars[c - 1].is_whitespace() && after_ok {
                return Some(c);
            }
        }
        None
    };
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let mut found = None;
        if i == 0 && chars[0] == m {
            found = close_from(0).map(|c| (None, 0, c));
        }
        if found.is_none() && i + 1 < n && !is_word(chars[i]) && chars[i] != m && chars[i + 1] == m
        {
            found = close_from(i + 1).map(|c| (Some(chars[i]), i + 1, c));
        }
        match found {
            Some((before, open, close)) => {
                out.extend(before);
                out.push(char::from_u32(OPEN + k).expect("a private-use character"));
                out.extend_from_slice(&chars[open + 1..close]);
                out.push(char::from_u32(CLOSE + k).expect("a private-use character"));
                i = close + 1;
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

/// Markdown for each span, by the index it was marked with: strong, em,
/// deleted, inserted. Markdown has no insert mark, so `+text+` keeps only
/// its text.
const SPAN_MARKS: [(char, &str); 4] = [('*', "**"), ('_', "_"), ('-', "~~"), ('+', "")];

/// Plain text with everything Markdown would read as markup escaped.
fn escape_text(s: &str, ctx: Ctx) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        let prev = i.checked_sub(1).map(|p| chars[p]);
        let next = chars.get(i + 1).copied();
        let escape = match c {
            '\\' | '`' | '*' | '[' | ']' | '<' | '~' => true,
            // An underscore between two letters is never emphasis.
            '_' => {
                !(prev.is_some_and(|p| p.is_alphanumeric())
                    && next.is_some_and(|n| n.is_alphanumeric()))
            }
            '|' => ctx.cell,
            // Only what reads as an entity reference: `&amp;`, `&#38;`.
            '&' => {
                let rest: String = chars[i + 1..].iter().take(33).collect();
                let name = rest.trim_start_matches('#');
                let len = name.chars().take_while(char::is_ascii_alphanumeric).count();
                len > 0 && name[len..].starts_with(';')
            }
            _ => false,
        };
        if escape {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A line that would start a Markdown block if left as it is, escaped.
fn escape_line_start(s: String) -> String {
    let t = s.as_str();
    let all = |c: char| !t.is_empty() && t.chars().all(|x| x == c);
    if t.starts_with('#')
        || t.starts_with('>')
        || t.starts_with("+ ")
        || t == "+"
        || t.starts_with("- ")
        || t.starts_with('-') && t.trim_end().chars().all(|c| c == '-')
        || all('=')
    {
        return format!("\\{s}");
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if (1..=9).contains(&digits) {
        let rest = &t[digits..];
        if (rest.starts_with('.') || rest.starts_with(')'))
            && rest[1..].chars().next().is_none_or(char::is_whitespace)
        {
            return format!("{}\\{}", &t[..digits], rest);
        }
    }
    s
}

/// The renderer's inline pass, writing Markdown. Holes already in `src`
/// refer to `held`, so a link's text may be converted with its outer pass.
fn inline_into(src: &str, held: &mut Vec<String>, ctx: Ctx) -> String {
    let s = CODE.replace_all(src, |c: &Captures| hold(held, code_span(&c[1], ctx)));
    let s = ESCAPE
        .replace_all(&s, |c: &Captures| {
            let md = match c.get(1) {
                Some(ch) => escape_text(ch.as_str(), Ctx { cell: true, ..ctx }),
                None if ctx.breaks => "\\\n".to_string(),
                None => " ".to_string(),
            };
            hold(held, md)
        })
        .into_owned();
    let s = COLOR
        .replace_all(&s, |c: &Captures| {
            let text = inline_into(&c[2], held, ctx);
            hold(held, text)
        })
        .into_owned();
    let s = LINK
        .replace_all(&s, |c: &Captures| match link(&c[1], held, ctx) {
            Some(md) => hold(held, md),
            None => c[0].to_string(),
        })
        .into_owned();
    let s = BARE_URL
        .replace_all(&s, |c: &Captures| {
            let url = &c[0];
            let bare = url.trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
            let tail = &url[bare.len()..];
            format!("{}{tail}", hold(held, format!("<{bare}>")))
        })
        .into_owned();
    let mut chars: Vec<char> = s.chars().collect();
    for (k, (m, _)) in SPAN_MARKS.iter().enumerate() {
        chars = spans(chars, *m, k as u32);
    }
    let mut out = String::new();
    let mut run = String::new();
    for c in chars {
        let code = c as u32;
        if is_hole(c) || is_sentinel(c) {
            out.push_str(&escape_text(&run, ctx));
            run.clear();
        }
        if is_hole(c) {
            out.push_str(held.get((code - HOLE) as usize).map_or("", String::as_str));
        } else if (OPEN..CLOSE).contains(&code) {
            out.push_str(SPAN_MARKS[(code - OPEN) as usize].1);
        } else if (CLOSE..=SENTINEL_END).contains(&code) {
            out.push_str(SPAN_MARKS[(code - CLOSE) as usize].1);
        } else {
            run.push(c);
        }
    }
    out.push_str(&escape_text(&run, ctx));
    out
}

fn inline(src: &str, ctx: Ctx) -> String {
    // NUL and the characters the pass marks with never come from the text.
    let clean: String = src
        .chars()
        .filter(|&c| c != '\0' && !is_hole(c) && !is_sentinel(c))
        .collect();
    inline_into(&clean, &mut Vec::new(), ctx)
}

/// GFM table cells, split on bars outside links and `{{code}}`.
fn cells(row: &str) -> Vec<(bool, String)> {
    let chars: Vec<char> = row.chars().collect();
    let mut out = Vec::new();
    let mut cur: Option<(bool, String)> = None;
    let (mut brackets, mut braces) = (0, 0);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        // An escaped bar is the cell's text, not its end.
        if c == '\\' && next.is_some() {
            if let Some((_, text)) = cur.as_mut() {
                text.push(c);
                text.extend(next);
            }
            i += 2;
            continue;
        }
        if c == '|' && brackets == 0 && braces == 0 {
            out.extend(cur.take());
            let head = next == Some('|');
            if head {
                i += 1;
            }
            cur = Some((head, String::new()));
            i += 1;
            continue;
        }
        if c == '[' {
            brackets += 1;
        } else if c == ']' && brackets > 0 {
            brackets -= 1;
        } else if c == '{' && next == Some('{') {
            braces += 1;
        } else if c == '}' && next == Some('}') && braces > 0 {
            braces -= 1;
        }
        if let Some((_, text)) = cur.as_mut() {
            text.push(c);
        }
        i += 1;
    }
    if let Some(last) = cur.filter(|(_, t)| !t.trim().is_empty()) {
        out.push(last);
    }
    out
}

fn table(rows: &[&str]) -> String {
    let ctx = Ctx {
        cell: true,
        breaks: false,
    };
    let mut rows: Vec<Vec<(bool, String)>> = rows
        .iter()
        .map(|r| {
            cells(r.trim())
                .into_iter()
                .map(|(head, text)| (head, inline(text.trim(), ctx)))
                .collect()
        })
        .collect();
    let width = rows.iter().map(Vec::len).max().unwrap_or(0).max(1);
    let header = match rows.first() {
        Some(first) if !first.is_empty() && first.iter().all(|(head, _)| *head) => rows.remove(0),
        _ => Vec::new(),
    };
    let line = |cells: &[(bool, String)]| {
        let texts: Vec<&str> = (0..width)
            .map(|i| cells.get(i).map_or("", |(_, t)| t.as_str()))
            .collect();
        format!("| {} |", texts.join(" | "))
    };
    let mut out = vec![line(&header), format!("|{}", " --- |".repeat(width))];
    out.extend(rows.iter().map(|r| line(r)));
    out.join("\n")
}

fn list(items: &[(Vec<char>, &str)]) -> String {
    let width = |mark: char| if mark == '#' { 3 } else { 2 };
    items
        .iter()
        .map(|(marks, text)| {
            let (last, parents) = marks.split_last().expect("an item has a mark");
            let indent: usize = parents.iter().map(|m| width(*m)).sum();
            let bullet = if *last == '#' { "1." } else { "-" };
            let text = escape_line_start(inline(text, Ctx::default()));
            format!("{}{bullet} {text}", " ".repeat(indent))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn paragraph(lines: &[String]) -> String {
    let ctx = Ctx {
        cell: false,
        breaks: true,
    };
    lines
        .iter()
        .map(|l| escape_line_start(inline(l.trim(), ctx).trim().to_string()))
        .collect::<Vec<_>>()
        .join("\\\n")
}

/// A fenced block's text from its opening line on: what is inside, up to
/// the closing tag or the end, and what follows the tag on its line.
fn fenced(lines: &[&str], i: &mut usize, first: &str, close: &str) -> (String, String) {
    let mut buf = first.to_string();
    while !buf.contains(close) && *i + 1 < lines.len() {
        *i += 1;
        buf.push('\n');
        buf.push_str(lines[*i]);
    }
    let end = buf.find(close);
    let inner = end.map_or(buf.as_str(), |e| &buf[..e]);
    let inner = inner.strip_prefix('\n').unwrap_or(inner);
    let inner = inner.strip_suffix('\n').unwrap_or(inner);
    let after = end.map_or("", |e| &buf[e + close.len()..]).trim();
    (inner.to_string(), after.to_string())
}

fn code_block(code: &str, params: Option<&str>) -> String {
    let lang: String = params
        .into_iter()
        .flat_map(|p| p.split('|'))
        .map(str::trim)
        .find(|p| !p.is_empty() && !p.contains('='))
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '.' | '#'))
        .collect();
    let longest = code.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}{lang}\n{code}\n{fence}")
}

fn blocks(lines: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut para: Vec<String> = Vec::new();
    let flush = |para: &mut Vec<String>, out: &mut Vec<String>| {
        if !para.is_empty() {
            out.push(paragraph(para));
            para.clear();
        }
    };
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];

        if let Some(c) = FENCE.captures(line) {
            flush(&mut para, &mut out);
            let close = format!("{{{}}}", &c[1]);
            let (code, after) = fenced(lines, &mut i, &c[3], &close);
            let params = (&c[1] == "code")
                .then(|| c.get(2).map(|p| p.as_str()))
                .flatten();
            out.push(code_block(&code, params));
            if !after.is_empty() {
                para.push(after);
            }
        } else if let Some(c) = QUOTE.captures(line) {
            flush(&mut para, &mut out);
            let (inner, after) = fenced(lines, &mut i, &c[1], "{quote}");
            let md = to_markdown(&inner);
            let quoted: Vec<String> = md
                .lines()
                .map(|l| {
                    if l.is_empty() {
                        ">".into()
                    } else {
                        format!("> {l}")
                    }
                })
                .collect();
            out.push(if quoted.is_empty() {
                ">".into()
            } else {
                quoted.join("\n")
            });
            if !after.is_empty() {
                para.push(after);
            }
        } else if line.trim().is_empty() {
            flush(&mut para, &mut out);
        } else if let Some(c) = HEADING.captures(line) {
            flush(&mut para, &mut out);
            let level: usize = c[1].parse().expect("one digit");
            let text = inline(&c[2], Ctx::default());
            out.push(
                format!("{} {}", "#".repeat(level), text.trim())
                    .trim_end()
                    .to_string(),
            );
        } else if let Some(c) = BQ.captures(line) {
            flush(&mut para, &mut out);
            let text = escape_line_start(inline(&c[1], Ctx::default()).trim().to_string());
            out.push(format!("> {text}"));
        } else if HR.is_match(line) {
            flush(&mut para, &mut out);
            out.push("---".into());
        } else if let Some(mut item) = LIST_ITEM.captures(line) {
            flush(&mut para, &mut out);
            let mut items = Vec::new();
            loop {
                let marks: Vec<char> = if &item[1] == "-" {
                    vec!['*']
                } else {
                    item[1].chars().collect()
                };
                items.push((marks, item.get(2).map_or("", |t| t.as_str())));
                match lines.get(i + 1).and_then(|l| LIST_ITEM.captures(l)) {
                    Some(next) => {
                        item = next;
                        i += 1;
                    }
                    None => break,
                }
            }
            out.push(list(&items));
        } else if line.trim_start().starts_with('|') {
            flush(&mut para, &mut out);
            let mut rows = vec![line];
            while lines
                .get(i + 1)
                .is_some_and(|l| l.trim_start().starts_with('|'))
            {
                i += 1;
                rows.push(lines[i]);
            }
            out.push(table(&rows));
        } else {
            para.push(line.to_string());
        }
        i += 1;
    }
    flush(&mut para, &mut out);
    out
}

/// Jira wiki markup as Markdown. No HTML is ever written: text Markdown
/// would read as markup is escaped, and a link whose scheme is not http,
/// https or mailto keeps only its text.
pub fn to_markdown(wiki: &str) -> String {
    let src = wiki.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = src.split('\n').collect();
    blocks(&lines).join("\n\n")
}

/// Markdown as Jira wiki markup.
pub fn from_markdown(md: &str) -> String {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut w = Writer::default();
    for event in Parser::new_ext(md, options) {
        w.event(event);
    }
    w.flush();
    w.out.trim_end_matches('\n').to_string()
}

#[derive(Default)]
struct Writer {
    out: String,
    /// Text not yet written: consecutive text events, escaped as one so a
    /// mark's neighbours are known.
    pending: String,
    /// Buffers set aside while a link's text or an image's alt is written.
    saved: Vec<String>,
    /// Link destinations, innermost last.
    links: Vec<String>,
    /// Open lists, innermost last: true when ordered.
    lists: Vec<bool>,
    images: usize,
    in_code: bool,
    in_heading: bool,
    in_table: bool,
    in_head: bool,
    /// Just inside a container, where a block needs no blank line before it.
    fresh: bool,
}

impl Writer {
    fn newline(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
    }

    /// Start a block: on its own line, after a blank one at the top level.
    fn block(&mut self) {
        if std::mem::take(&mut self.fresh) || self.out.is_empty() {
            return;
        }
        self.newline();
        if self.lists.is_empty() && !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
    }

    /// Text as wiki markup reads it literally: a mark that could open or
    /// close a span, or a character that starts markup, is escaped.
    fn text(&mut self, s: &str) {
        if self.in_code {
            self.out.push_str(s);
            return;
        }
        let chars: Vec<char> = s.chars().collect();
        for (i, &c) in chars.iter().enumerate() {
            let prev = match i {
                0 => self
                    .out
                    .chars()
                    .last()
                    .or_else(|| self.saved.last().and_then(|s| s.chars().last())),
                _ => Some(chars[i - 1]),
            };
            let next = chars.get(i + 1).copied();
            let line_start = prev.is_none_or(|p| p == '\n');
            let could_open =
                prev.is_none_or(|p| !is_word(p)) && next.is_some_and(|n| !n.is_whitespace());
            let could_close =
                prev.is_some_and(|p| !p.is_whitespace()) && next.is_none_or(|n| !is_word(n));
            let escape = match c {
                '*' | '_' | '-' | '+' | '^' | '~' => {
                    could_open || could_close || (line_start && c != '_')
                }
                '#' => line_start,
                '{' | '[' => true,
                '!' => next.is_some_and(|n| !n.is_whitespace()),
                '|' => self.in_table || !self.links.is_empty() || line_start,
                ']' => !self.links.is_empty(),
                'h' | 'b' if line_start => {
                    let rest: String = chars[i..].iter().take(4).collect();
                    let heading = rest.len() >= 3
                        && rest.as_bytes()[0] == b'h'
                        && (b'1'..=b'6').contains(&rest.as_bytes()[1])
                        && rest.as_bytes()[2] == b'.';
                    if heading || rest.starts_with("bq.") {
                        // Escape the dot, not the letter.
                        self.out.push(c);
                        let dot = if heading { 2 } else { 3 };
                        for &d in &chars[i + 1..i + dot] {
                            self.out.push(d);
                        }
                        self.out.push('\\');
                        self.text_from(&chars[i + dot..]);
                        return;
                    }
                    false
                }
                _ => false,
            };
            if escape {
                self.out.push('\\');
            }
            self.out.push(c);
        }
    }

    fn text_from(&mut self, rest: &[char]) {
        let s: String = rest.iter().collect();
        self.text(&s);
    }

    fn line_break(&mut self) {
        if self.in_code {
            self.out.push('\n');
        } else if self.in_heading || self.in_table || !self.lists.is_empty() {
            self.out.push(' ');
        } else {
            self.out.push('\n');
        }
    }

    fn flush(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        if !pending.is_empty() {
            self.text(&pending);
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Text(t) if !self.in_code => return self.pending.push_str(&t),
            Event::InlineHtml(t) => return self.pending.push_str(&t),
            _ => self.flush(),
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(t) => {
                self.out.push_str("{{");
                self.out.push_str(&t);
                self.out.push_str("}}");
            }
            Event::Html(t) => self.text(t.trim_end_matches('\n')),
            Event::SoftBreak => {
                if self.in_code {
                    self.out.push('\n');
                } else {
                    self.out.push(' ');
                }
            }
            Event::HardBreak => self.line_break(),
            Event::Rule => {
                self.block();
                self.out.push_str("----\n");
            }
            Event::TaskListMarker(done) => {
                self.out.push_str(if done { "\\[x] " } else { "\\[ ] " });
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                if self.lists.is_empty() {
                    self.block();
                } else if !self.out.ends_with(' ') && !self.out.ends_with('\n') {
                    // A second paragraph in a list item: wiki has no way to
                    // hold it in the item but on the same line.
                    self.out.push(' ');
                }
            }
            Tag::Heading { level, .. } => {
                self.block();
                let n = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                };
                self.out.push_str(&format!("h{n}. "));
                self.in_heading = true;
            }
            Tag::BlockQuote(_) => {
                self.block();
                self.out.push_str("{quote}\n");
                self.fresh = true;
            }
            Tag::CodeBlock(kind) => {
                self.block();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .chars()
                        .filter(|c| {
                            c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '.' | '#')
                        })
                        .collect(),
                    CodeBlockKind::Indented => String::new(),
                };
                if lang.is_empty() {
                    self.out.push_str("{code}\n");
                } else {
                    self.out.push_str(&format!("{{code:{lang}}}\n"));
                }
                self.in_code = true;
            }
            Tag::HtmlBlock => self.block(),
            Tag::List(first) => {
                if self.lists.is_empty() {
                    self.block();
                }
                self.lists.push(first.is_some());
            }
            Tag::Item => {
                self.newline();
                let marks: String = self
                    .lists
                    .iter()
                    .map(|ordered| if *ordered { '#' } else { '*' })
                    .collect();
                self.out.push_str(&marks);
                self.out.push(' ');
            }
            Tag::Table(_) => {
                self.block();
                self.in_table = true;
            }
            Tag::TableHead => self.in_head = true,
            Tag::TableRow => {}
            Tag::TableCell => self.out.push_str(if self.in_head { "||" } else { "|" }),
            Tag::Emphasis => self.out.push('_'),
            Tag::Strong => self.out.push('*'),
            Tag::Strikethrough => self.out.push('-'),
            Tag::Link { dest_url, .. } => {
                self.saved.push(std::mem::take(&mut self.out));
                self.links.push(dest_url.to_string());
            }
            Tag::Image { dest_url, .. } => {
                self.saved.push(std::mem::take(&mut self.out));
                self.links.push(dest_url.to_string());
                self.images += 1;
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => {
                if self.lists.is_empty() {
                    self.out.push('\n');
                }
            }
            TagEnd::Heading(_) => {
                self.in_heading = false;
                self.out.push('\n');
            }
            TagEnd::BlockQuote(_) => {
                self.fresh = false;
                self.newline();
                self.out.push_str("{quote}\n");
            }
            TagEnd::CodeBlock => {
                self.in_code = false;
                self.newline();
                self.out.push_str("{code}\n");
            }
            TagEnd::List(_) => {
                self.lists.pop();
                self.newline();
            }
            TagEnd::Item => self.newline(),
            TagEnd::Table => self.in_table = false,
            TagEnd::TableHead => {
                self.in_head = false;
                self.out.push_str("||\n");
            }
            TagEnd::TableRow => self.out.push_str("|\n"),
            TagEnd::Emphasis => self.out.push('_'),
            TagEnd::Strong => self.out.push('*'),
            TagEnd::Strikethrough => self.out.push('-'),
            TagEnd::Link => {
                let text = std::mem::replace(&mut self.out, self.saved.pop().unwrap_or_default());
                let dest = self.links.pop().unwrap_or_default();
                let web = dest.starts_with("http://") || dest.starts_with("https://");
                if web && text.replace('\\', "") == dest {
                    // Written bare, Jira links it and reads it back the same.
                    self.out.push_str(&dest);
                } else {
                    self.out.push_str(&format!("[{text}|{dest}]"));
                }
            }
            TagEnd::Image => {
                self.out = self.saved.pop().unwrap_or_default();
                let dest = self.links.pop().unwrap_or_default();
                self.images -= 1;
                self.out.push_str(&format!("!{dest}!"));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(wiki: &str) -> String {
        to_markdown(wiki)
    }

    // The renderer's tests, each restated as the Markdown it now writes.

    #[test]
    fn headings_and_inline_emphasis() {
        assert_eq!(md("h2. Scope *now*"), "## Scope **now**");
        assert_eq!(
            md("a *bold* and _italic_ word"),
            "a **bold** and _italic_ word"
        );
        assert_eq!(md("-gone- and +added+"), "~~gone~~ and added");
    }

    #[test]
    fn markers_inside_words_are_left_alone() {
        assert_eq!(
            md("snake_case_name and a-b-c and 2*3*4"),
            "snake_case_name and a-b-c and 2\\*3\\*4"
        );
    }

    #[test]
    fn inline_code_is_literal() {
        assert_eq!(md("run {{*not bold* <b>}} now"), "run `*not bold* <b>` now");
        assert_eq!(md("{{a`b}}"), "``a`b``");
    }

    #[test]
    fn code_and_noformat_blocks_keep_their_text_verbatim() {
        let src = "before\n{code:php}\n$a = \"<x>\";\n*not bold*\n{code}\nafter";
        assert_eq!(
            md(src),
            "before\n\n```php\n$a = \"<x>\";\n*not bold*\n```\n\nafter"
        );
        assert_eq!(md("{noformat}h1. raw{noformat}"), "```\nh1. raw\n```");
        assert_eq!(md("{code}\nunterminated"), "```\nunterminated\n```");
        assert_eq!(md("{code:title=x.rs|java}\na\n{code}"), "```java\na\n```");
        assert_eq!(md("{code}\n```\n{code}"), "````\n```\n````");
    }

    #[test]
    fn nested_and_mixed_lists() {
        assert_eq!(
            md("* one\n** one.a\n* two\n# first"),
            "- one\n  - one.a\n- two\n1. first"
        );
        assert_eq!(md("# a\n#* b\n## c"), "1. a\n   - b\n   1. c");
        assert_eq!(md("- dash\n- item"), "- dash\n- item");
        assert_eq!(md("*bold* is not a list"), "**bold** is not a list");
    }

    #[test]
    fn tables_with_header_cells_and_links_that_contain_bars() {
        assert_eq!(
            md("||Name||Link||\n|a|[docs|https://example.com/x]|"),
            "| Name | Link |\n| --- | --- |\n| a | [docs](https://example.com/x) |"
        );
        // Without a header row, an empty one is supplied.
        assert_eq!(
            md("|a|{{x|y}}|\n|b|"),
            "|  |  |\n| --- | --- |\n| a | `x\\|y` |\n| b |  |"
        );
    }

    #[test]
    fn links_are_allow_listed_the_same_way_as_markdown() {
        assert_eq!(
            md("[docs|https://example.com/a?b=1&c=2]"),
            "[docs](https://example.com/a?b=1&c=2)"
        );
        assert_eq!(md("[https://example.com]"), "<https://example.com>");
        assert_eq!(md("[run|javascript:alert(1)]"), "run");
        assert!(!md("[run|java\nscript:alert(1)]").contains("]("));
        assert_eq!(md("[x|data:text/html,<script>]"), "x");
        assert_eq!(md("[x|//evil.example]"), "x");
        assert_eq!(md("[x|JavaScript:alert(1)]"), "x");
        assert_eq!(md("[home|/work]"), "[home](/work)");
        assert_eq!(md("see [~someone]"), "see @someone");
        assert_eq!(md("a [plain] bracket"), "a \\[plain\\] bracket");
        assert_eq!(
            md("[a (b)|https://e.com/x_(y)]"),
            "[a (b)](https://e.com/x_\\(y\\))"
        );
    }

    #[test]
    fn bare_urls_link_without_swallowing_trailing_punctuation() {
        assert_eq!(
            md("see https://example.com/x."),
            "see <https://example.com/x>."
        );
    }

    #[test]
    fn embedded_html_is_escaped_never_trusted() {
        let out = md("<img src=x onerror=\"alert(1)\"><script>alert(2)</script>");
        let unescaped = out
            .char_indices()
            .filter(|(i, c)| *c == '<' && !out[..*i].ends_with('\\'))
            .count();
        assert_eq!(unescaped, 0, "{out}");
        assert_eq!(
            md("[<b>x</b>|https://example.com/\"onmouseover=\"]"),
            "[\\<b>x\\</b>](https://example.com/\"onmouseover=\")"
        );
        assert_eq!(md("\u{0}0\u{0} *x*"), "0 **x**");
        // The characters the pass marks with cannot be smuggled in.
        assert_eq!(md("a\u{F0000}b\u{E000}c"), "abc");
    }

    #[test]
    fn paragraphs_keep_single_line_breaks_and_split_on_blank_lines() {
        assert_eq!(
            md("one\ntwo\n\nthree\r\n----\nbq. quoted"),
            "one\\\ntwo\n\nthree\n\n---\n\n> quoted"
        );
    }

    #[test]
    fn colour_markup_keeps_its_text() {
        assert_eq!(md("{color:red}Do not{color} delete"), "Do not delete");
        assert_eq!(md("{color:red;background:url(x)}hi{color}"), "hi");
    }

    // What the Markdown side adds.

    #[test]
    fn text_that_would_start_markdown_blocks_is_escaped() {
        assert_eq!(md("#1 is done"), "\\#1 is done");
        assert_eq!(md("1. not a list"), "1\\. not a list");
        assert_eq!(md("a\n==="), "a\\\n\\===");
        assert_eq!(md("> not a quote"), "\\> not a quote");
        assert_eq!(md("R&D &amp; more"), "R&D \\&amp; more");
        assert_eq!(md("a ~b~ `c`"), "a \\~b\\~ \\`c\\`");
    }

    #[test]
    fn a_quote_block_and_escapes_and_forced_breaks() {
        assert_eq!(
            md("{quote}\none\n\ntwo\n{quote}\nafter"),
            "> one\n>\n> two\n\nafter"
        );
        assert_eq!(md("not \\*bold\\*"), "not \\*bold\\*");
        assert_eq!(md("a\\\\b"), "a\\\nb");
    }

    fn wiki(md: &str) -> String {
        from_markdown(md)
    }

    #[test]
    fn markdown_blocks_become_wiki_blocks() {
        assert_eq!(wiki("## Scope"), "h2. Scope");
        assert_eq!(wiki("a\n\nb"), "a\n\nb");
        assert_eq!(wiki("a\nb"), "a b");
        assert_eq!(wiki("a\\\nb"), "a\nb");
        assert_eq!(wiki("---"), "----");
        assert_eq!(wiki("> a\n>\n> b"), "{quote}\na\n\nb\n{quote}");
        assert_eq!(
            wiki("```rust\nfn x() {}\n```"),
            "{code:rust}\nfn x() {}\n{code}"
        );
        assert_eq!(wiki("    indented"), "{code}\nindented\n{code}");
    }

    #[test]
    fn markdown_lists_nest_as_wiki_marks() {
        assert_eq!(
            wiki("- one\n  - one.a\n- two\n\n1. first\n   - sub"),
            "* one\n** one.a\n* two\n\n# first\n#* sub"
        );
        assert_eq!(wiki("- [x] done\n- [ ] not"), "* \\[x] done\n* \\[ ] not");
    }

    #[test]
    fn markdown_spans_links_and_images_become_wiki() {
        assert_eq!(wiki("**b** _e_ ~~s~~ `c`"), "*b* _e_ -s- {{c}}");
        assert_eq!(
            wiki("[docs](https://e.com/x) <https://e.com/y> ![alt](https://e.com/i.png)"),
            "[docs|https://e.com/x] https://e.com/y !https://e.com/i.png!"
        );
        assert_eq!(wiki("[a|b](https://e.com)"), "[a\\|b|https://e.com]");
    }

    #[test]
    fn markdown_tables_become_wiki_tables() {
        assert_eq!(
            wiki("| A | B |\n| --- | --- |\n| 1 | x\\|y |"),
            "||A||B||\n|1|x\\|y|"
        );
    }

    #[test]
    fn literal_wiki_marks_in_markdown_text_are_escaped() {
        assert_eq!(
            wiki("2 * 3 and a-b and snake_case"),
            "2 * 3 and a-b and snake_case"
        );
        assert_eq!(wiki("\\*not bold\\* {x} [y]"), "\\*not bold\\* \\{x} \\[y]");
        assert_eq!(wiki("\\- not a list"), "\\- not a list");
        assert_eq!(wiki("h1. not a heading"), "h1\\. not a heading");
        assert_eq!(wiki("<b>raw</b>"), "<b>raw</b>");
    }

    #[test]
    fn markdown_round_trips_through_wiki() {
        let docs = [
            "## Scope\n\nA **bold** and _italic_ and ~~gone~~ word with `code`.",
            "---\n\n> quoted text\n\n- one\n  - one.a\n- two\n\n1. first\n1. second",
            "| Name | Link |\n| --- | --- |\n| a | [docs](https://example.com/x) |",
            "```rust\nfn main() {}\n```\n\nSee [docs](https://example.com/a) and <https://example.com/b>.",
            "2 \\* 3 is 6, snake_case stays, and \\[brackets\\].\n\none\\\ntwo",
            "> one\n>\n> two\n\n1. a\n   - b",
        ];
        for doc in docs {
            let wiki = from_markdown(doc);
            assert_eq!(to_markdown(&wiki), doc, "through {wiki:?}");
        }
    }
}
