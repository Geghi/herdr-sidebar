//! A small dependency-free markdown renderer for the preview pane: headings,
//! emphasis, inline code, links, lists, task boxes, quotes, fenced code, rules
//! and simple tables, as styled ratatui lines. It renders a pull request's
//! overview, and it is the fallback for markdown FILES when `glow` is not
//! installed — a raw dump of `#`/`**` markers is not a preview.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::ui::palette;

/// Render `text` as styled lines. `width` is used where the layout needs it
/// (tables, rules); everything else is wrapped by the pane itself.
pub fn render(text: &str, width: usize) -> Vec<Line<'static>> {
    let text = strip_html(text);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut fence: Option<String> = None;
    // Set by a heading, cleared by the next content row: the blank rows a
    // heading is followed by are swallowed, so a heading sits on its body.
    let mut after_heading = false;
    let mut lines = text.lines().peekable();
    while let Some(raw) = lines.next() {
        let trimmed = raw.trim_start();
        let was_heading = std::mem::take(&mut after_heading);
        if fence.is_some() {
            if trimmed.starts_with("```") {
                fence = None;
                continue;
            }
            out.push(code_line(raw));
            continue;
        }
        if let Some(language) = trimmed.strip_prefix("```") {
            let language = language.trim();
            if !language.is_empty() {
                out.push(Line::from(Span::styled(
                    format!("  {language}"),
                    Style::default().dim(),
                )));
            }
            fence = Some(language.to_string());
            continue;
        }
        if trimmed.is_empty() {
            after_heading = was_heading;
            if !was_heading && !matches!(out.last(), Some(line) if line.spans.is_empty()) {
                out.push(Line::default());
            }
            continue;
        }
        if let Some((level, rest)) = heading(trimmed) {
            // Exactly one blank row above a heading, never two.
            if !out.is_empty() && !matches!(out.last(), Some(line) if line.spans.is_empty()) {
                out.push(Line::default());
            }
            out.push(heading_line(level, rest));
            after_heading = true;
            continue;
        }
        if is_rule(trimmed) {
            out.push(Line::from(Span::styled(
                "─".repeat(width.clamp(8, 60)),
                Style::default().dim(),
            )));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('>') {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            let mut spans = vec![Span::styled("│ ", Style::default().dim())];
            spans.extend(inline(rest, Style::default().italic()));
            out.push(Line::from(spans));
            continue;
        }
        if let Some((indent, marker, rest)) = list_item(raw) {
            let mut spans = vec![Span::raw(" ".repeat(indent))];
            spans.push(match marker {
                Marker::Bullet => Span::styled("• ", Style::default().dim()),
                Marker::Number(number) => {
                    Span::styled(format!("{number}. "), Style::default().dim())
                }
                Marker::Todo(done) => {
                    let (glyph, style) = if done {
                        ("☑ ", Style::default().fg(palette().untracked))
                    } else {
                        ("☐ ", Style::default().dim())
                    };
                    Span::styled(glyph, style)
                }
            });
            spans.extend(inline(rest, Style::default()));
            out.push(Line::from(spans));
            continue;
        }
        if is_table_row(trimmed) && lines.peek().is_some_and(|next| is_table_separator(next)) {
            let mut rows = vec![cells(trimmed)];
            lines.next();
            while let Some(next) = lines.peek() {
                if !is_table_row(next) {
                    break;
                }
                rows.push(cells(next));
                lines.next();
            }
            out.extend(table_lines(&rows, width));
            continue;
        }
        out.push(Line::from(inline(trimmed, Style::default())));
    }
    out
}

/// A list marker.
enum Marker {
    Bullet,
    Number(u32),
    Todo(bool),
}

/// `(indent, marker, rest)` for a list line.
fn list_item(raw: &str) -> Option<(usize, Marker, &str)> {
    let indent = raw.len() - raw.trim_start().len();
    let trimmed = raw.trim_start();
    let (marker, rest) = if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        let marker = match rest {
            _ if rest.starts_with("[ ] ") => Marker::Todo(false),
            _ if rest.starts_with("[x] ") || rest.starts_with("[X] ") => Marker::Todo(true),
            _ => Marker::Bullet,
        };
        let rest = if matches!(marker, Marker::Todo(_)) {
            &rest[4..]
        } else {
            rest
        };
        (marker, rest)
    } else {
        let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
        let rest = trimmed.strip_prefix(&format!("{digits}. "))?;
        (Marker::Number(digits.parse().unwrap_or(1)), rest)
    };
    Some((indent.min(8), marker, rest))
}

/// `(level, text)` for an ATX heading.
fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|c| *c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = line[level..].strip_prefix(' ')?;
    Some((level, rest.trim_end_matches('#').trim_end()))
}

fn heading_line(level: usize, text: &str) -> Line<'static> {
    let style = if level <= 2 {
        Style::default()
            .fg(palette().header_accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    Line::from(Span::styled(text.to_string(), style))
}

/// `---`, `***` or `___` on its own line.
fn is_rule(line: &str) -> bool {
    let stripped: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    stripped.len() >= 3
        && (stripped.chars().all(|c| c == '-')
            || stripped.chars().all(|c| c == '*')
            || stripped.chars().all(|c| c == '_'))
}

/// A fenced code block's line: a quiet rail plus the code.
fn code_line(raw: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("  │ ", Style::default().dim()),
        Span::styled(raw.to_string(), Style::default().fg(palette().ignored)),
    ])
}

/// Inline markdown to styled spans: emphasis, code, links and strikethrough.
fn inline(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        let (kind, at) = next_marker(rest);
        let Some((kind, at)) = kind.map(|kind| (kind, at)) else {
            plain.push_str(rest);
            break;
        };
        plain.push_str(&rest[..at]);
        rest = &rest[at..];
        let consumed = match kind {
            Marker_::Bold => styled_pair(&mut spans, &mut plain, rest, "**", base, |style| {
                style.add_modifier(Modifier::BOLD)
            }),
            Marker_::Italic => styled_pair(&mut spans, &mut plain, rest, "*", base, |style| {
                style.add_modifier(Modifier::ITALIC)
            }),
            Marker_::Strike => styled_pair(&mut spans, &mut plain, rest, "~~", base, |style| {
                style.add_modifier(Modifier::CROSSED_OUT)
            }),
            Marker_::Code => styled_pair(&mut spans, &mut plain, rest, "`", base, |style| {
                style.fg(palette().code_fg)
            }),
            Marker_::Link => {
                if let Some((text, url, used)) = link(rest) {
                    flush(&mut spans, &mut plain, base);
                    let repeats_url = url == text;
                    spans.push(Span::styled(
                        text,
                        base.fg(palette().accent).add_modifier(Modifier::UNDERLINED),
                    ));
                    if !repeats_url {
                        spans.push(Span::styled(format!(" ({url})"), base.dim()));
                    }
                    used
                } else {
                    plain.push('[');
                    1
                }
            }
        };
        rest = &rest[consumed..];
    }
    flush(&mut spans, &mut plain, base);
    spans
}

/// The inline constructs the scanner looks for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Marker_ {
    Bold,
    Italic,
    Strike,
    Code,
    Link,
}

/// The earliest marker in `text`, with its byte offset.
fn next_marker(text: &str) -> (Option<Marker_>, usize) {
    let mut best: (Option<Marker_>, usize) = (None, text.len());
    let candidates: [(Marker_, &str); 5] = [
        (Marker_::Bold, "**"),
        (Marker_::Strike, "~~"),
        (Marker_::Code, "`"),
        (Marker_::Italic, "*"),
        (Marker_::Link, "["),
    ];
    for (kind, token) in candidates {
        if let Some(at) = text.find(token)
            && at < best.1
        {
            best = (Some(kind), at);
        }
    }
    best
}

/// A `**x**`-style pair into `spans`, returning how many bytes it consumed.
fn styled_pair(
    spans: &mut Vec<Span<'static>>,
    plain: &mut String,
    rest: &str,
    token: &str,
    base: Style,
    style: fn(Style) -> Style,
) -> usize {
    let Some(inner) = rest.strip_prefix(token) else {
        plain.push_str(token);
        return token.len();
    };
    let Some(end) = inner.find(token) else {
        plain.push_str(token);
        return token.len();
    };
    flush(spans, plain, base);
    spans.extend(inline(&inner[..end], style(base)));
    token.len() + end + token.len()
}

/// `[text](url)` at the start of `rest`: (text, url, bytes used).
fn link(rest: &str) -> Option<(String, String, usize)> {
    let inner = rest.strip_prefix('[')?;
    let close = inner.find(']')?;
    let text = &inner[..close];
    let after = &inner[close + 1..];
    let url = after.strip_prefix('(')?;
    let end = url.find(')')?;
    let url = &url[..end];
    // `[` text `]` `(` url `)` — the brackets and parens count too.
    Some((text.to_string(), url.to_string(), close + end + 4))
}

fn flush(spans: &mut Vec<Span<'static>>, plain: &mut String, base: Style) {
    if !plain.is_empty() {
        spans.push(Span::styled(std::mem::take(plain), base));
    }
}

/// Remove HTML comments and tags, which GitHub bodies carry but a terminal
/// cannot render.
fn strip_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => return out,
        }
    }
    out.push_str(rest);
    let mut cleaned = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(start) = rest.find('<') {
        cleaned.push_str(&rest[..start]);
        match rest[start..].find('>') {
            Some(end) => rest = &rest[start + end + 1..],
            None => break,
        }
    }
    cleaned.push_str(rest);
    cleaned
}

/// Whether a line is a table row (at least one `|`).
fn is_table_row(line: &str) -> bool {
    line.matches('|').count() >= 2
}

/// Whether a line is a table's `|---|:--:|` separator.
fn is_table_separator(line: &str) -> bool {
    let stripped: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    stripped.len() >= 3
        && stripped.contains('|')
        && stripped.chars().all(|c| matches!(c, '-' | '|' | ':'))
}

/// A row's cells, without the outer pipes.
fn cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

/// A table as aligned columns: a bold header, a dim rule, then the rows. The
/// widest columns are trimmed so the table fits the pane.
fn table_lines(rows: &[Vec<String>], width: usize) -> Vec<Line<'static>> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return Vec::new();
    }
    let mut widths = vec![0usize; columns];
    for row in rows {
        for (column, cell) in row.iter().enumerate() {
            widths[column] = widths[column].max(cell.chars().count());
        }
    }
    let gaps = columns.saturating_sub(1) * 2;
    let budget = width.saturating_sub(gaps).max(8);
    while widths.iter().sum::<usize>() > budget {
        let Some((widest, _)) = widths.iter().enumerate().max_by_key(|(_, w)| **w) else {
            break;
        };
        if widths[widest] <= 4 {
            break;
        }
        widths[widest] -= 1;
    }
    let mut out = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (column, cell) in row.iter().enumerate() {
            if column > 0 {
                spans.push(Span::styled("  ", Style::default().dim()));
            }
            let room = widths.get(column).copied().unwrap_or(0);
            let text = crate::ui::truncate_to(cell.clone(), room);
            let pad = room.saturating_sub(text.chars().count());
            let style = if index == 0 {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            spans.push(Span::styled(text, style));
            if pad > 0 {
                spans.push(Span::raw(" ".repeat(pad)));
            }
        }
        out.push(Line::from(spans));
        if index == 0 {
            let rule = (widths.iter().sum::<usize>() + gaps).min(width);
            out.push(Line::from(Span::styled(
                "─".repeat(rule.max(4)),
                Style::default().dim(),
            )));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn styles(lines: &[Line<'static>]) -> Vec<(String, Style)> {
        lines
            .iter()
            .flat_map(|line| {
                line.spans
                    .iter()
                    .map(|span| (span.content.to_string(), span.style))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn headings_lose_their_markers_and_lead_with_a_break() {
        let lines = render("# Title\n\nbody", 40);
        assert_eq!(text(&lines), "Title\nbody", "a heading sits on its body");
        assert_eq!(
            text(&render("body\n## Sub", 40)),
            "body\n\nSub",
            "a heading opens a break before itself"
        );
        assert_eq!(
            text(&render("body\n\n\n## Sub\n\n\ntext", 40)),
            "body\n\nSub\ntext",
            "blank runs collapse to one above a heading and vanish below it"
        );
        let styles = styles(&lines);
        let (title, style) = styles.iter().find(|(t, _)| t == "Title").unwrap();
        assert_eq!(title, "Title");
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(style.fg, Some(palette().header_accent));
    }

    #[test]
    fn emphasis_code_links_and_strikethrough_render_as_spans() {
        let lines = render(
            "a **bold** b *it* c `code` d [site](https://x.dev) e ~~old~~",
            80,
        );
        let spans = styles(&lines);
        let find = |want: &str| {
            spans
                .iter()
                .find(|(content, _)| content == want)
                .unwrap_or_else(|| panic!("missing {want}: {spans:?}"))
                .1
        };
        assert!(find("bold").add_modifier.contains(Modifier::BOLD));
        assert!(find("it").add_modifier.contains(Modifier::ITALIC));
        // Inline code is foreground-only: no filled block over the card.
        assert_eq!(find("code").bg, None);
        assert_eq!(find("code").fg, Some(palette().code_fg));
        assert!(find("site").add_modifier.contains(Modifier::UNDERLINED));
        assert!(find("old").add_modifier.contains(Modifier::CROSSED_OUT));
        let all = text(&lines);
        assert!(!all.contains("**"), "no markers survive: {all}");
        assert!(all.contains("(https://x.dev)"), "the url is kept: {all}");
    }

    #[test]
    fn a_link_whose_text_is_the_url_does_not_repeat_it() {
        let lines = render("[https://x.dev](https://x.dev)", 80);
        assert_eq!(text(&lines), "https://x.dev");
    }

    #[test]
    fn lists_quotes_rules_and_task_boxes_get_their_own_markers() {
        let lines = render("- one\n- [x] done\n- [ ] todo\n1. first\n> quoted\n---", 40);
        let all = text(&lines);
        assert!(all.contains("• one"), "{all}");
        assert!(all.contains("☑ done"), "{all}");
        assert!(all.contains("☐ todo"), "{all}");
        assert!(all.contains("1. first"), "{all}");
        assert!(all.contains("│ quoted"), "{all}");
        assert!(all.contains('─'), "{all}");
    }

    #[test]
    fn nested_list_items_are_indented() {
        let lines = render("- outer\n  - inner", 40);
        let all = text(&lines);
        assert!(all.contains("• outer"), "{all}");
        assert!(all.contains("  • inner"), "{all}");
    }

    #[test]
    fn fenced_code_keeps_its_lines_behind_a_rail() {
        let lines = render("```rust\nlet x = 1;\n```\nafter", 40);
        let all = text(&lines);
        assert!(all.contains("rust"), "the language label: {all}");
        assert!(all.contains("│ let x = 1;"), "{all}");
        assert!(!all.contains("```"), "the fence itself is gone: {all}");
        assert!(all.contains("after"));
    }

    #[test]
    fn html_comments_and_tags_are_stripped() {
        let lines = render("<!-- hidden -->visible <b>bold</b>", 40);
        assert_eq!(text(&lines), "visible bold");
        let lines = render("a <!-- spans\nlines --> b", 40);
        assert_eq!(text(&lines), "a  b");
    }

    #[test]
    fn a_table_renders_as_aligned_columns() {
        let lines = render(
            "| name | value |\n|---|---|\n| a | 1 |\n| longer | 22 |",
            60,
        );
        let all = text(&lines);
        assert!(!all.contains('|'), "no pipes survive: {all}");
        assert!(all.contains("name"), "{all}");
        assert!(all.contains('─'), "a rule under the header: {all}");
        assert!(all.contains("longer"), "{all}");
        let rows: Vec<&Line<'static>> =
            lines.iter().filter(|line| !line.spans.is_empty()).collect();
        assert_eq!(rows.len(), 4, "header, rule and two rows");
    }

    #[test]
    fn a_pipe_line_that_is_not_a_table_stays_text() {
        let lines = render("a | b | c", 40);
        assert_eq!(text(&lines), "a | b | c");
    }

    #[test]
    fn plain_text_and_blank_lines_survive() {
        let lines = render("one\n\ntwo", 40);
        assert_eq!(text(&lines), "one\n\ntwo");
    }
}
