//! Syntax highlighting for the file preview: syntect with bat's extended
//! grammar set via `two-face` (syntect's own defaults lack TypeScript, TOML,
//! Dockerfile, …), on the pure-Rust `regex-fancy` engine — no oniguruma C
//! build on Windows. Foreground colors only: the terminal keeps its own
//! background, and unknown file types fall back to plain lines.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{
    FontStyle, HighlightIterator, HighlightState, Highlighter, Theme, ThemeSet,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

/// Lines longer than this skip highlighting entirely. syntect's `regex-fancy`
/// backtracking engine is roughly quadratic in line length on pathological
/// input (minified JS/CSS, generated data) — a single multi-hundred-KB line
/// can take tens of seconds and freeze the render thread.
const MAX_HIGHLIGHT_LINE_LEN: usize = 2000;

/// Grammar + theme assets, loaded once (the bundled dumps take a few ms).
/// BOTH themes are built up front: switching the color theme must not stall a
/// render, and the dumps are small.
fn assets() -> &'static (SyntaxSet, Theme, Theme) {
    static ASSETS: OnceLock<(SyntaxSet, Theme, Theme)> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let syntaxes = two_face::syntax::extra_newlines();
        let mut themes = ThemeSet::load_defaults();
        let mut pick = |name: &str| {
            themes
                .themes
                .remove(name)
                .or_else(|| themes.themes.pop_first().map(|(_, t)| t))
                .unwrap_or_default()
        };
        let dark = pick("base16-ocean.dark");
        let light = pick("InspiredGitHub");
        (syntaxes, dark, light)
    })
}

/// The grammar set plus the theme that suits the active color theme. syntect
/// colors foregrounds only, so a dark grammar theme on a light terminal is
/// exactly the washed-out case the light palette exists to fix.
fn syntaxes_and_theme() -> (&'static SyntaxSet, &'static Theme) {
    let (syntaxes, dark, light) = assets();
    (syntaxes, if crate::ui::is_light() { light } else { dark })
}

/// Highlight `text` for a file called `name`, up to `max` lines. `None` when
/// no grammar matches (caller falls back to plain lines).
pub fn highlight(name: &str, text: &str, max: usize) -> Option<Vec<Line<'static>>> {
    let (syntaxes, theme) = syntaxes_and_theme();
    let ext = name.rsplit('.').next().unwrap_or("");
    let syntax = syntaxes
        .find_syntax_by_extension(ext)
        .or_else(|| syntaxes.find_syntax_by_extension(name))
        .or_else(|| {
            text.lines()
                .next()
                .and_then(|l| syntaxes.find_syntax_by_first_line(l))
        })?;

    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();
    for raw in LinesWithEndings::from(text).take(max) {
        if raw.len() > MAX_HIGHLIGHT_LINE_LEN {
            lines.push(Line::raw(raw.trim_end_matches(['\n', '\r']).to_string()));
            continue;
        }
        let Ok(regions) = highlighter.highlight_line(raw, syntaxes) else {
            lines.push(Line::raw(raw.trim_end_matches(['\n', '\r']).to_string()));
            continue;
        };
        let spans: Vec<Span<'static>> = regions
            .into_iter()
            .filter_map(|(style, chunk)| {
                let chunk = chunk.trim_end_matches(['\n', '\r']);
                if chunk.is_empty() {
                    return None;
                }
                let fg = style.foreground;
                let mut out = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
                if style.font_style.contains(FontStyle::BOLD) {
                    out = out.add_modifier(Modifier::BOLD);
                }
                if style.font_style.contains(FontStyle::ITALIC) {
                    out = out.add_modifier(Modifier::ITALIC);
                }
                Some(Span::styled(chunk.to_string(), out))
            })
            .collect();
        lines.push(Line::from(spans));
    }
    Some(lines)
}

/// Stateful per-line highlighter (for diff rendering, where old/new file
/// contexts advance independently). Plain spans when no grammar matches.
pub struct LineHighlighter {
    inner: Option<HighlightLines<'static>>,
}

impl LineHighlighter {
    pub fn new(name: &str) -> Self {
        let (syntaxes, theme) = syntaxes_and_theme();
        let ext = name.rsplit('.').next().unwrap_or("");
        let syntax = syntaxes
            .find_syntax_by_extension(ext)
            .or_else(|| syntaxes.find_syntax_by_extension(name));
        Self {
            inner: syntax.map(|s| HighlightLines::new(s, theme)),
        }
    }

    /// Highlight one line (no trailing newline in, none out).
    pub fn line(&mut self, text: &str) -> Vec<Span<'static>> {
        let Some(hl) = self.inner.as_mut() else {
            return vec![Span::raw(text.to_string())];
        };
        if text.len() > MAX_HIGHLIGHT_LINE_LEN {
            return vec![Span::raw(text.to_string())];
        }
        let (syntaxes, _) = syntaxes_and_theme();
        let with_nl = format!("{text}\n");
        match hl.highlight_line(&with_nl, syntaxes) {
            Ok(regions) => regions
                .into_iter()
                .filter_map(|(style, chunk)| {
                    let chunk = chunk.trim_end_matches(['\n', '\r']);
                    if chunk.is_empty() {
                        return None;
                    }
                    let fg = style.foreground;
                    let mut out = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
                    if style.font_style.contains(FontStyle::BOLD) {
                        out = out.add_modifier(Modifier::BOLD);
                    }
                    if style.font_style.contains(FontStyle::ITALIC) {
                        out = out.add_modifier(Modifier::ITALIC);
                    }
                    Some(Span::styled(chunk.to_string(), out))
                })
                .collect(),
            Err(_) => vec![Span::raw(text.to_string())],
        }
    }
}

fn to_style(style: syntect::highlighting::Style) -> Style {
    let fg = style.foreground;
    let mut out = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    out
}

/// Per-line style runs `(char_end, style)` for the editor, highlighted lazily.
/// syntect is stateful line to line, so an edit invalidates from the first
/// changed line DOWNWARD only; [`Incremental::advance`] then re-highlights at
/// most up to the line the caller needs, within a time budget per frame.
pub struct Incremental {
    syntax: Option<&'static SyntaxReference>,
    light: bool,
    /// The text each cached line was highlighted from (change detection).
    texts: Vec<String>,
    runs: Vec<Vec<(usize, Style)>>,
    /// Parser + highlighter state AFTER each cached line.
    states: Vec<(ParseState, HighlightState)>,
}

impl Incremental {
    pub fn new(name: &str) -> Self {
        let (syntaxes, _) = syntaxes_and_theme();
        let ext = name.rsplit('.').next().unwrap_or("");
        let syntax = syntaxes
            .find_syntax_by_extension(ext)
            .or_else(|| syntaxes.find_syntax_by_extension(name));
        Self {
            syntax,
            light: crate::ui::is_light(),
            texts: Vec::new(),
            runs: Vec::new(),
            states: Vec::new(),
        }
    }

    /// How many leading lines are cached and still valid.
    pub fn valid(&self) -> usize {
        self.runs.len()
    }

    /// Drop every cached line from the first one that no longer matches.
    pub fn invalidate(&mut self, lines: &[String]) {
        if self.light != crate::ui::is_light() {
            self.light = crate::ui::is_light();
            self.texts.clear();
        }
        let first = self
            .texts
            .iter()
            .zip(lines)
            .position(|(a, b)| a != b)
            .unwrap_or(self.texts.len().min(lines.len()));
        self.texts.truncate(first);
        self.runs.truncate(first);
        self.states.truncate(first);
    }

    /// Highlight forward until line `upto` (exclusive) is cached or `budget`
    /// runs out — whatever is left renders plain and catches up next frame.
    pub fn advance(&mut self, lines: &[String], upto: usize, budget: std::time::Duration) {
        let Some(syntax) = self.syntax else {
            return;
        };
        let (syntaxes, theme) = syntaxes_and_theme();
        let highlighter = Highlighter::new(theme);
        let started = std::time::Instant::now();
        let upto = upto.min(lines.len());
        while self.runs.len() < upto && started.elapsed() < budget {
            let i = self.runs.len();
            let (mut parse, mut high) = self.states.last().cloned().unwrap_or_else(|| {
                (
                    ParseState::new(syntax),
                    HighlightState::new(&highlighter, ScopeStack::new()),
                )
            });
            let text = &lines[i];
            let mut runs = Vec::new();
            let with_nl = format!("{text}\n");
            if text.len() <= MAX_HIGHLIGHT_LINE_LEN
                && let Ok(ops) = parse.parse_line(&with_nl, syntaxes)
            {
                let mut end = 0;
                for (style, chunk) in
                    HighlightIterator::new(&mut high, &ops, &with_nl, &highlighter)
                {
                    end += chunk.trim_end_matches('\n').chars().count();
                    runs.push((end, to_style(style)));
                }
            }
            self.texts.push(text.clone());
            self.runs.push(runs);
            self.states.push((parse, high));
        }
    }

    /// Style of char `col` on `line`, if that line is highlighted.
    pub fn style_at(&self, line: usize, col: usize) -> Style {
        self.runs
            .get(line)
            .and_then(|runs| runs.iter().find(|(end, _)| col < *end))
            .map_or_else(Style::default, |(_, style)| *style)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_extensions_highlight_with_colors() {
        let lines = highlight("main.rs", "fn main() {}\n", 10).expect("rust grammar");
        assert_eq!(lines.len(), 1);
        // The `fn` keyword must carry a non-default foreground color.
        let colored = lines[0]
            .spans
            .iter()
            .any(|s| s.content.contains("fn") && s.style.fg.is_some());
        assert!(colored, "expected a colored keyword span");
        assert_eq!(lines[0].to_string(), "fn main() {}");
    }

    #[test]
    fn extended_grammars_cover_typescript_and_toml() {
        assert!(
            highlight(
                "app.ts",
                "const x: string = \"hi\";
",
                10
            )
            .is_some()
        );
        assert!(
            highlight(
                "Cargo.toml",
                "[package]
name = \"x\"
",
                10
            )
            .is_some()
        );
    }

    #[test]
    fn unknown_extensions_fall_back_to_none() {
        assert!(highlight("data.qqzz", "gibberish content\n", 10).is_none());
    }

    #[test]
    fn pathologically_long_lines_skip_highlighting_instead_of_hanging() {
        let long_line = format!(
            "const x = \"{}\";\n",
            "a".repeat(MAX_HIGHLIGHT_LINE_LEN + 1)
        );
        let lines = highlight("bundle.min.js", &long_line, 10).expect("js grammar matches");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_string(), long_line.trim_end_matches('\n'));

        let mut hl = LineHighlighter::new("bundle.min.js");
        let spans = hl.line(long_line.trim_end_matches('\n'));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, long_line.trim_end_matches('\n'));
    }

    #[test]
    fn incremental_invalidates_from_the_edited_line_down_only() {
        let mut lines: Vec<String> = ["fn a() {}", "/* open", "still comment */", "fn b() {}"]
            .map(String::from)
            .to_vec();
        let mut hl = Incremental::new("x.rs");
        let budget = std::time::Duration::from_secs(5);
        hl.advance(&lines, 2, budget);
        assert_eq!(hl.valid(), 2, "stops at the requested line");
        hl.advance(&lines, 99, budget);
        assert_eq!(hl.valid(), 4);
        let comment = hl.style_at(2, 0);
        lines[1] = "// closed".into();
        hl.invalidate(&lines);
        assert_eq!(hl.valid(), 1, "lines above the edit stay cached");
        hl.advance(&lines, 99, budget);
        assert_ne!(hl.style_at(2, 0), comment, "parse state flowed down");
        assert_eq!(hl.style_at(0, 0), hl.style_at(3, 0));
    }
}
