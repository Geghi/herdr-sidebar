//! VS Code-style diff rendering: dual line-number gutters, syntax-highlighted
//! code over red/green row tints, and a darker word-level tint on the changed
//! segment of paired lines. Input is plain `git diff` output (no ANSI) — the
//! parsing is ours, so the look is too.

use std::collections::HashMap;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::syntax::LineHighlighter;
use crate::ui::palette;

// Row tints live in `ui::Palette` (`diff_*`) so the light theme can swap them
// for GitHub-light tints without a second copy of this renderer.

/// One parsed diff line, before rendering.
#[derive(Debug, PartialEq, Eq)]
enum Ev {
    /// A new hunk begins (rendered as a dim separator between hunks).
    Hunk,
    /// Unchanged: (old line no, new line no, text).
    Ctx(usize, usize, String),
    Del(usize, String),
    Add(usize, String),
    /// Anything unparsed worth keeping ("Binary files … differ").
    Plain(String),
}

fn parse_events(diff: &str) -> Vec<Ev> {
    let mut evs = Vec::new();
    let mut old_no = 0usize;
    let mut new_no = 0usize;
    let mut seen_hunk = false;
    let mut in_hunk = false;
    for line in diff.lines() {
        if line.starts_with("diff --git") {
            in_hunk = false;
        }
        if (!in_hunk && (line.starts_with("--- ") || line.starts_with("+++ ")))
            || line.starts_with("diff --git")
            || line.starts_with("index ")
            || line.starts_with("old mode")
            || line.starts_with("new mode")
            || line.starts_with("similarity")
            || line.starts_with("rename ")
            || line.starts_with("copy ")
            || line.starts_with("new file mode")
            || line.starts_with("deleted file mode")
            || line.starts_with('\\')
        {
            continue;
        }
        if line.starts_with("@@") {
            in_hunk = true;
            for tok in line.split_whitespace() {
                let (sign, rest) = match tok.split_at_checked(1) {
                    Some(pair) => pair,
                    None => continue,
                };
                let start = rest.split(',').next().unwrap_or("");
                if let Ok(n) = start.parse::<usize>() {
                    match sign {
                        "-" => old_no = n,
                        "+" => new_no = n,
                        _ => {}
                    }
                }
            }
            if seen_hunk {
                evs.push(Ev::Hunk);
            }
            seen_hunk = true;
            continue;
        }
        if let Some(t) = line.strip_prefix('-') {
            evs.push(Ev::Del(old_no, t.to_string()));
            old_no += 1;
        } else if let Some(t) = line.strip_prefix('+') {
            evs.push(Ev::Add(new_no, t.to_string()));
            new_no += 1;
        } else if let Some(t) = line.strip_prefix(' ') {
            evs.push(Ev::Ctx(old_no, new_no, t.to_string()));
            old_no += 1;
            new_no += 1;
        } else if line.is_empty() {
            // Some tools trim the leading space off blank context lines.
            evs.push(Ev::Ctx(old_no, new_no, String::new()));
            old_no += 1;
            new_no += 1;
        } else {
            evs.push(Ev::Plain(line.to_string()));
        }
    }
    evs
}

/// Char range (start, end) of the differing middle of a changed line,
/// keyed by event index — deletions paired 1:1 with the additions that
/// immediately follow them, VS Code style.
fn word_ranges(evs: &[Ev]) -> HashMap<usize, (usize, usize)> {
    let mut ranges = HashMap::new();
    let mut i = 0;
    while i < evs.len() {
        let del_start = i;
        while matches!(evs.get(i), Some(Ev::Del(..))) {
            i += 1;
        }
        let add_start = i;
        while matches!(evs.get(i), Some(Ev::Add(..))) {
            i += 1;
        }
        if del_start == add_start || add_start == i {
            if i == del_start {
                i += 1;
            }
            continue;
        }
        for k in 0..(add_start - del_start).min(i - add_start) {
            let (Some(Ev::Del(_, old)), Some(Ev::Add(_, new))) =
                (evs.get(del_start + k), evs.get(add_start + k))
            else {
                continue;
            };
            let old_chars: Vec<char> = old.chars().collect();
            let new_chars: Vec<char> = new.chars().collect();
            let mut prefix = 0;
            while prefix < old_chars.len()
                && prefix < new_chars.len()
                && old_chars[prefix] == new_chars[prefix]
            {
                prefix += 1;
            }
            let mut suffix = 0;
            while suffix < old_chars.len().saturating_sub(prefix)
                && suffix < new_chars.len().saturating_sub(prefix)
                && old_chars[old_chars.len() - 1 - suffix]
                    == new_chars[new_chars.len() - 1 - suffix]
            {
                suffix += 1;
            }
            // Only worth tinting when the lines genuinely share material.
            if prefix + suffix == 0 {
                continue;
            }
            ranges.insert(del_start + k, (prefix, old_chars.len() - suffix));
            ranges.insert(add_start + k, (prefix, new_chars.len() - suffix));
        }
    }
    ranges
}

/// Re-slice spans so `range` (in chars) carries `bg` — the word-level tint.
fn overlay_bg(spans: Vec<Span<'static>>, range: (usize, usize), bg: Color) -> Vec<Span<'static>> {
    let (start, end) = range;
    if start >= end {
        return spans;
    }
    let mut out = Vec::new();
    let mut pos = 0usize;
    for span in spans {
        let chars: Vec<char> = span.content.chars().collect();
        let len = chars.len();
        let (a, b) = (start.clamp(pos, pos + len), end.clamp(pos, pos + len));
        if a == b || a >= pos + len {
            out.push(span);
        } else {
            let style = span.style;
            let before: String = chars[..a - pos].iter().collect();
            let middle: String = chars[a - pos..b - pos].iter().collect();
            let after: String = chars[b - pos..].iter().collect();
            if !before.is_empty() {
                out.push(Span::styled(before, style));
            }
            out.push(Span::styled(middle, style.bg(bg)));
            if !after.is_empty() {
                out.push(Span::styled(after, style));
            }
        }
        pos += len;
    }
    out
}

/// One row of the side-by-side layout. `src` is the unified line the row
/// came from (unified output is one line per parsed event), so scroll
/// anchoring and hunk navigation keep working across a layout toggle.
#[derive(Debug, Clone, PartialEq)]
pub enum SplitRow {
    /// Spans both columns: a hunk separator or an unparsed line.
    Full { src: usize, line: Line<'static> },
    /// Old side left, new side right; `None` is a blank filler cell.
    Pair {
        src: usize,
        left: Option<Line<'static>>,
        right: Option<Line<'static>>,
    },
}

/// Both layouts of one diff, highlighted once.
pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    pub split: Vec<SplitRow>,
}

/// Render a unified diff for `rel` (its extension picks the grammar) into
/// display lines — `old new ±` gutters, tinted rows, highlighted code — and
/// into side-by-side rows (each side with its own `no ±` gutter, deletions
/// paired 1:1 with the additions that follow them, like `word_ranges`).
pub fn render(rel: &str, diff: &str) -> Rendered {
    let evs = parse_events(diff);
    let ranges = word_ranges(&evs);

    let max_no = evs
        .iter()
        .map(|e| match e {
            Ev::Ctx(o, n, _) => (*o).max(*n),
            Ev::Del(o, _) => *o,
            Ev::Add(n, _) => *n,
            _ => 0,
        })
        .max()
        .unwrap_or(0);
    let w = max_no.to_string().len().max(2);
    let fmt = |v: Option<usize>| match v {
        Some(v) => format!("{v:>w$}"),
        None => " ".repeat(w),
    };

    // Two stateful highlighters approximate the old and new file contexts,
    // so multi-line constructs mostly survive (the delta/bat trick).
    let mut old_hl = LineHighlighter::new(rel);
    let mut new_hl = LineHighlighter::new(rel);
    let colors = palette();
    let code: Vec<Vec<Span<'static>>> = evs
        .iter()
        .enumerate()
        .map(|(idx, ev)| match ev {
            Ev::Ctx(_, _, t) => {
                old_hl.line(t);
                new_hl.line(t)
            }
            Ev::Del(_, t) => match ranges.get(&idx) {
                Some(&range) => overlay_bg(old_hl.line(t), range, colors.diff_del_word_bg),
                None => old_hl.line(t),
            },
            Ev::Add(_, t) => match ranges.get(&idx) {
                Some(&range) => overlay_bg(new_hl.line(t), range, colors.diff_add_word_bg),
                None => new_hl.line(t),
            },
            _ => Vec::new(),
        })
        .collect();

    let mark = |m: &str, fg: Color| Span::styled(format!("{m} "), Style::default().fg(fg));
    let tint = |line: Line<'static>, bg: Option<Color>| match bg {
        Some(bg) => line.style(Style::default().bg(bg)),
        None => line,
    };
    // (old no, new no, mark, mark color, row tint) per event.
    let shape = |ev: &Ev| match ev {
        Ev::Ctx(o, n, _) => (Some(*o), Some(*n), " ", Color::Reset, None),
        Ev::Del(o, _) => (
            Some(*o),
            None,
            "-",
            colors.diff_del_mark,
            Some(colors.diff_del_bg),
        ),
        Ev::Add(n, _) => (
            None,
            Some(*n),
            "+",
            colors.diff_add_mark,
            Some(colors.diff_add_bg),
        ),
        _ => (None, None, " ", Color::Reset, None),
    };

    let lines: Vec<Line<'static>> = evs
        .iter()
        .zip(&code)
        .map(|(ev, spans)| match ev {
            Ev::Hunk => Line::from(Span::styled(
                format!("{}⋯", " ".repeat(w * 2 + 2)),
                Style::default().dim(),
            )),
            Ev::Plain(t) => Line::from(Span::styled(t.clone(), Style::default().dim())),
            _ => {
                let (o, n, m, fg, bg) = shape(ev);
                let mut all = vec![
                    Span::styled(format!("{} {} ", fmt(o), fmt(n)), Style::default().dim()),
                    mark(m, fg),
                ];
                all.extend(spans.iter().cloned());
                tint(Line::from(all), bg)
            }
        })
        .collect();

    // One side of a split row: `no ± code`, carrying the event's tint.
    let side = |j: usize, old: bool| {
        let (o, n, m, fg, bg) = shape(&evs[j]);
        let mut all = vec![
            Span::styled(
                format!("{} ", fmt(if old { o } else { n })),
                Style::default().dim(),
            ),
            mark(m, fg),
        ];
        all.extend(code[j].iter().cloned());
        tint(Line::from(all), bg)
    };
    let is_del = |j: usize| matches!(evs.get(j), Some(Ev::Del(..)));
    let is_add = |j: usize| matches!(evs.get(j), Some(Ev::Add(..)));
    let mut split = Vec::new();
    let mut i = 0;
    while i < evs.len() {
        match &evs[i] {
            Ev::Hunk | Ev::Plain(_) => {
                split.push(SplitRow::Full {
                    src: i,
                    line: lines[i].clone(),
                });
                i += 1;
            }
            Ev::Ctx(..) => {
                split.push(SplitRow::Pair {
                    src: i,
                    left: Some(side(i, true)),
                    right: Some(side(i, false)),
                });
                i += 1;
            }
            Ev::Del(..) | Ev::Add(..) => {
                let dels: Vec<usize> = (i..).take_while(|&j| is_del(j)).collect();
                let adds: Vec<usize> = (i + dels.len()..).take_while(|&j| is_add(j)).collect();
                for k in 0..dels.len().max(adds.len()) {
                    let (d, a) = (dels.get(k).copied(), adds.get(k).copied());
                    split.push(SplitRow::Pair {
                        src: d.or(a).unwrap_or(i),
                        left: d.map(|j| side(j, true)),
                        right: a.map(|j| side(j, false)),
                    });
                }
                i += dels.len() + adds.len();
            }
        }
    }
    Rendered { lines, split }
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: no `\` line continuations — they would eat the leading space
    // that marks context lines.
    const DIFF: &str = concat!(
        "diff --git a/src/app.ts b/src/app.ts\n",
        "index 111..222 100644\n",
        "--- a/src/app.ts\n",
        "+++ b/src/app.ts\n",
        "@@ -1,4 +1,5 @@\n",
        " import { search } from \"./search\";\n",
        "-console.log(\"scm-playground up\");\n",
        "+console.log(\"scm oh yeah\");\n",
        "+// TODO: debounce input\n",
        " search(\"hello\");\n",
    );

    #[test]
    fn diff_parses_gutters_tints_and_word_ranges() {
        let colors = palette();
        let lines = render("app.ts", DIFF).lines;
        assert_eq!(lines.len(), 5);
        // Context row: both numbers, no tint.
        assert!(lines[0].to_string().starts_with(" 1  1"));
        assert_eq!(lines[0].style.bg, None);
        // Deletion: old number only, red row tint.
        assert!(lines[1].to_string().contains('-'));
        assert_eq!(lines[1].style.bg, Some(colors.diff_del_bg));
        // Addition: new number only, green row tint.
        assert_eq!(lines[2].style.bg, Some(colors.diff_add_bg));
        // The paired del/add carry a darker word-level tint on the middle.
        let word_tinted = lines[1]
            .spans
            .iter()
            .any(|s| s.style.bg == Some(colors.diff_del_word_bg));
        assert!(word_tinted, "expected word-level tint on the deletion");
        // The unpaired trailing addition has no word tint.
        let plain_add = lines[3]
            .spans
            .iter()
            .all(|s| s.style.bg != Some(colors.diff_add_word_bg));
        assert!(plain_add);
    }

    #[test]
    fn deleted_line_starting_with_dashes_does_not_desync_gutters() {
        // A deleted SQL/Lua-style "-- comment" line looks exactly like a
        // "--- a/file" header if the parser isn't hunk-aware.
        let diff = concat!(
            "diff --git a/q.sql b/q.sql\n",
            "index 111..222 100644\n",
            "--- a/q.sql\n",
            "+++ b/q.sql\n",
            "@@ -1,3 +1,2 @@\n",
            " SELECT 1;\n",
            "--- trailing comment\n",
            " SELECT 2;\n",
        );
        let lines = render("q.sql", diff).lines;
        assert_eq!(lines.len(), 3, "the deleted comment line must render");
        assert!(lines[0].to_string().starts_with(" 1  1"));
        assert!(lines[1].to_string().contains("-- trailing comment"));
        // Old-side numbering must not skip: line 3 is old line 3, not 2.
        assert!(lines[2].to_string().starts_with(" 3  2"));
    }

    #[test]
    fn hunk_boundaries_and_binary_lines_survive() {
        let two_hunks = "@@ -1,1 +1,1 @@\n ctx\n@@ -9,1 +9,1 @@\n ctx2\n";
        let lines = render("x.rs", two_hunks).lines;
        assert_eq!(lines.len(), 3);
        assert!(lines[1].to_string().contains('⋯'));
        let bin = render("x.bin", "Binary files a/x.bin and b/x.bin differ\n").lines;
        assert_eq!(bin.len(), 1);
    }

    #[test]
    fn split_pairs_dels_with_adds_and_fills_the_rest() {
        let colors = palette();
        let split = render("app.ts", DIFF).split;
        let text = |l: &Option<Line>| l.as_ref().map(|l| l.to_string());
        let pair = |r: &SplitRow| match r {
            SplitRow::Pair { src, left, right } => (*src, text(left), text(right)),
            SplitRow::Full { .. } => panic!("unexpected full row"),
        };
        // ctx, (del|add), (filler|add), ctx — one row per pair.
        assert_eq!(split.len(), 4);
        // Context on both sides, each with its own number.
        let (src, l, r) = pair(&split[0]);
        assert_eq!(src, 0);
        assert!(l.unwrap().starts_with(" 1   import"));
        assert!(r.unwrap().starts_with(" 1   import"));
        // The deletion sits beside the addition that replaced it, tinted and
        // word-tinted; src is the deletion's unified line.
        let (src, l, r) = pair(&split[1]);
        assert_eq!(src, 1);
        assert!(l.unwrap().contains("scm-playground up"));
        assert!(r.unwrap().contains("scm oh yeah"));
        let SplitRow::Pair {
            left: Some(left),
            right: Some(right),
            ..
        } = &split[1]
        else {
            unreachable!()
        };
        assert_eq!(left.style.bg, Some(colors.diff_del_bg));
        assert_eq!(right.style.bg, Some(colors.diff_add_bg));
        assert!(
            left.spans
                .iter()
                .any(|s| s.style.bg == Some(colors.diff_del_word_bg))
        );
        // The unpaired extra addition gets a blank filler on the left.
        let (src, l, r) = pair(&split[2]);
        assert_eq!(src, 3);
        assert_eq!(l, None);
        assert!(r.unwrap().contains("TODO"));
        // Trailing context: old 3 / new 4.
        let (_, l, r) = pair(&split[3]);
        assert!(l.unwrap().starts_with(" 3 "));
        assert!(r.unwrap().starts_with(" 4 "));
    }

    #[test]
    fn split_lone_deletions_and_hunk_separators() {
        let diff = "@@ -1,2 +1,0 @@\n-a\n-b\n@@ -9,1 +7,1 @@\n ctx\n";
        let split = render("x.rs", diff).split;
        assert_eq!(split.len(), 4);
        for row in &split[..2] {
            assert!(matches!(
                row,
                SplitRow::Pair {
                    left: Some(_),
                    right: None,
                    ..
                }
            ));
        }
        assert!(
            matches!(&split[2], SplitRow::Full { src: 2, line } if line.to_string().contains('⋯'))
        );
        assert!(matches!(
            &split[3],
            SplitRow::Pair {
                src: 3,
                left: Some(_),
                right: Some(_)
            }
        ));
    }
}
