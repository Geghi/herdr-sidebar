//! GitHub pull-request plumbing: the `gh` CLI plus pure parsers. `gh` talks to
//! the network and has no timeout of its own, so every command here is a plain
//! synchronous call that the caller MUST run on a worker thread (see
//! `pr_app`), never on the pane's event loop.

use std::path::Path;
use std::process::Command;
use std::time::SystemTime;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::git::FileEntry;
use crate::ui::palette;

/// The `gh pr list --json` field list the rows are built from.
const LIST_FIELDS: &str = "number,title,author,headRefName,baseRefName,isDraft,updatedAt,url,additions,deletions,changedFiles,reviewDecision";

/// The `gh pr view --json` field list the overview is built from.
const OVERVIEW_FIELDS: &str = "number,title,author,state,isDraft,baseRefName,headRefName,body,url,additions,deletions,changedFiles,reviewDecision,comments,reviews,statusCheckRollup,mergeable,mergeStateStatus,labels,reviewRequests,updatedAt";

/// Which drawer asks `gh` for which list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrFilter {
    /// Opened by the authenticated account.
    Authored,
    /// Waiting on the authenticated account as a reviewer.
    ReviewRequested,
    /// Every open pull request.
    Open,
}

impl PrFilter {
    /// The drawer title — also the persisted drawer key.
    pub fn title(self) -> &'static str {
        match self {
            Self::Authored => "My Pull Requests",
            Self::ReviewRequested => "Review Requested",
            Self::Open => "Open Pull Requests",
        }
    }

    pub const ALL: [PrFilter; 3] = [Self::Authored, Self::ReviewRequested, Self::Open];
}

/// GitHub's review decision, collapsed to what a row shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewState {
    /// Nothing decided (or nothing required) yet.
    Pending,
    Approved,
    ChangesRequested,
}

impl ReviewState {
    /// The one-character marker a pull-request row carries.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Pending => "○",
            Self::Approved => "✓",
            Self::ChangesRequested => "✗",
        }
    }
}

/// One open pull request, as the list rows need it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub author: String,
    /// The head branch (where the changes come from).
    pub head: String,
    /// The base branch (where they would land).
    pub base: String,
    pub draft: bool,
    pub updated: String,
    pub url: String,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    pub review: ReviewState,
}

impl PullRequest {
    /// The row's dim second line: who opened it, from where, how big.
    pub fn detail(&self) -> String {
        format!(
            "#{} {} {} → {} +{} −{}",
            self.number, self.author, self.head, self.base, self.additions, self.deletions
        )
    }

    /// `gh pr checkout` needs a ref, and the number is the shortest one that
    /// always resolves.
    pub fn checkout_ref(&self) -> String {
        self.number.to_string()
    }
}

/// One file of a pull request: the panel's file entry plus its churn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrFile {
    pub entry: FileEntry,
    pub additions: u64,
    pub deletions: u64,
}

/// Whether `gh` is usable in `root`, with the message to show when it is not.
pub fn availability(root: &Path) -> Result<(), String> {
    run(root, &["auth".to_string(), "status".to_string()]).map(|_| ())
}

/// One page of open pull requests for `filter`, newest activity first.
pub fn list(root: &Path, filter: PrFilter, limit: usize) -> Result<Vec<PullRequest>, String> {
    let mut args: Vec<String> = vec![
        "pr".into(),
        "list".into(),
        "--state".into(),
        "open".into(),
        "--limit".into(),
        limit.to_string(),
        "--json".into(),
        LIST_FIELDS.into(),
    ];
    match filter {
        PrFilter::Authored => {
            args.push("--author".into());
            args.push("@me".into());
        }
        PrFilter::ReviewRequested => {
            args.push("--search".into());
            args.push("review-requested:@me".into());
        }
        PrFilter::Open => {}
    }
    Ok(parse_pr_list(&run(root, &args)?))
}

/// Parse the `gh pr list --json` array.
pub fn parse_pr_list(raw: &str) -> Vec<PullRequest> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    items.iter().filter_map(parse_pr).collect()
}

fn parse_pr(item: &Value) -> Option<PullRequest> {
    let text = |key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    Some(PullRequest {
        number: item.get("number")?.as_u64()?,
        title: text("title"),
        author: item
            .get("author")
            .and_then(|author| author.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        head: text("headRefName"),
        base: text("baseRefName"),
        draft: item
            .get("isDraft")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        updated: text("updatedAt"),
        url: text("url"),
        additions: item.get("additions").and_then(Value::as_u64).unwrap_or(0),
        deletions: item.get("deletions").and_then(Value::as_u64).unwrap_or(0),
        changed_files: item
            .get("changedFiles")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        review: parse_review(item.get("reviewDecision").and_then(Value::as_str)),
    })
}

fn parse_review(decision: Option<&str>) -> ReviewState {
    match decision.unwrap_or("") {
        "APPROVED" => ReviewState::Approved,
        "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
        _ => ReviewState::Pending,
    }
}

/// The files a pull request touches. The REST API is used over `gh pr view
/// --json files` because it carries the status letter and, for a rename, the
/// path it came from.
pub fn files(root: &Path, number: u64) -> Result<Vec<PrFile>, String> {
    let jq =
        r#".[] | [.status, .filename, (.previous_filename // ""), .additions, .deletions] | @tsv"#;
    let out = run(
        root,
        &[
            "api".into(),
            format!("repos/{{owner}}/{{repo}}/pulls/{number}/files"),
            "--paginate".into(),
            "--jq".into(),
            jq.into(),
        ],
    )?;
    Ok(parse_pr_files(&out))
}

/// `status<TAB>path<TAB>previous<TAB>additions<TAB>deletions` rows.
pub fn parse_pr_files(raw: &str) -> Vec<PrFile> {
    raw.lines()
        .filter_map(|line| {
            let line = line.trim_end();
            if line.is_empty() {
                return None;
            }
            let mut fields = line.split('\t');
            let letter = match fields.next()? {
                "added" => 'A',
                "removed" => 'D',
                "renamed" => 'R',
                "copied" => 'C',
                _ => 'M',
            };
            let path = fields.next()?.to_string();
            let previous = fields.next().unwrap_or("").to_string();
            let churn = |field: Option<&str>| field.and_then(|n| n.parse().ok()).unwrap_or(0);
            Some(PrFile {
                entry: FileEntry {
                    path,
                    orig: matches!(letter, 'R' | 'C')
                        .then_some(previous)
                        .filter(|path| !path.is_empty()),
                    letter,
                },
                additions: churn(fields.next()),
                deletions: churn(fields.next()),
            })
        })
        .collect()
}

/// The whole patch of a pull request, as `gh pr diff` prints it.
pub fn diff(root: &Path, number: u64) -> Result<String, String> {
    run(root, &["pr".into(), "diff".into(), number.to_string()])
}

/// The section of a unified patch that belongs to `path`. The panel fetches a
/// pull request's patch ONCE and slices it per file, so opening a file costs
/// no request.
pub fn patch_for_file(patch: &str, path: &str) -> Option<String> {
    let mut out = String::new();
    let mut inside = false;
    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            inside = patch_header_matches(line, path);
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Whether a `diff --git a/<path> b/<path>` header names `path`. git quotes
/// either side when the path needs it (`"a/with space"`), so both the quoted
/// and the bare form are unwrapped before the `a/`/`b/` prefix is dropped.
fn patch_header_matches(header: &str, path: &str) -> bool {
    let rest = header.trim_start_matches("diff --git ");
    let (left, right) = match rest.split_once("\" \"") {
        Some(pair) => pair,
        None => match rest.split_once(" b/") {
            Some(pair) => pair,
            None => return false,
        },
    };
    let left = unquote_path(left);
    let left = left.strip_prefix("a/").unwrap_or(&left);
    let right = unquote_path(right);
    let right = right.strip_prefix("b/").unwrap_or(&right);
    left == path || right == path
}

/// git quotes paths that need it (`"a/with space"`); compare the inner text.
fn unquote_path(raw: &str) -> String {
    raw.trim()
        .trim_matches('"')
        .replace("\\\"", "\"")
        .replace("\\\\", "\\")
}

/// The pull-request overview as markdown for the preview pane: what it is, who
/// wrote it, the conversation so far.
pub fn detail(root: &Path, number: u64) -> Result<Value, String> {
    let out = run(
        root,
        &[
            "pr".into(),
            "view".into(),
            number.to_string(),
            "--json".into(),
            OVERVIEW_FIELDS.into(),
        ],
    )?;
    serde_json::from_str(&out).map_err(|e| format!("gh json: {e}"))
}

/// The overview pane's lines in the GitHub web look: a hero card (band, bold
/// title, pills, a two-column meta grid), a keycap action bar, then UPPERCASE
/// section heads over a rule. Description rows carry NO rail — eighty rows
/// of `▍` read as a slab, not a card — while conversation bodies keep a thin
/// `▎` in the entry's dot color so the feed still reads as entries. Never raw
/// markdown: no `#`, `**` or `[..](..)` syntax reaches the pane.
///
/// Nothing here is boxed, so every line re-wraps naturally when the pane
/// width moves; only the meta grid's column split is width-derived, and it is
/// recomputed on every call.
pub fn render_overview(detail: &Value, width: usize) -> Vec<Line<'static>> {
    render_overview_at(detail, width, SystemTime::now())
}

/// Widths under this collapse the meta grid to one column.
const TWO_COLUMN_MIN: usize = 60;

fn render_overview_at(detail: &Value, width: usize, now: SystemTime) -> Vec<Line<'static>> {
    let text = |key: &str| detail.get(key).and_then(Value::as_str).unwrap_or("");
    let author = detail
        .get("author")
        .and_then(|author| author.get("login"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let number = detail.get("number").and_then(Value::as_u64).unwrap_or(0);
    let additions = detail
        .get("additions")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let deletions = detail
        .get("deletions")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let changed = detail
        .get("changedFiles")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let dim = Style::default().dim();

    let mut out: Vec<Line<'static>> = Vec::new();
    // The hero card, topped by a full-bleed band in the state color — GitHub's
    // PR card edge. The `#n` chip, the state pill and the left rail echo that
    // color, so the request's verdict owns the top of the page.
    let (state, state_color) = state_badge(detail);
    out.push(Line::from(vec![Span::styled(
        " ".repeat(width.max(1)),
        Style::default().bg(state_color),
    )]));
    out.push(rail_line(
        state_color,
        vec![
            pill(&format!("#{number}"), state_color),
            Span::styled(
                text("title").to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ],
    ));
    // The pill row: state, review decision, mergeability, checks in one line.
    let mut pills = vec![pill(state, state_color)];
    let verdicts = [
        review_line(detail.get("reviewDecision").and_then(Value::as_str))
            .map(|(glyph, label, color)| (glyph, label.to_string(), color)),
        mergeability_line(detail).map(|(glyph, label, color)| (glyph, label.to_string(), color)),
        checks_line(detail.get("statusCheckRollup")),
    ];
    for (glyph, label, color) in verdicts.into_iter().flatten() {
        pills.push(Span::raw("  "));
        pills.push(Span::styled(
            format!("{glyph} {label}"),
            Style::default().fg(color),
        ));
    }
    out.push(rail_line(state_color, pills));
    out.push(rail_line(state_color, vec![]));

    // The meta grid: `Label  value` cells, two columns when the pane is wide
    // enough, one below. Rows without content (no reviewers, no labels) drop
    // out rather than reading `—`.
    let mut changes = vec![
        Span::styled(format!("+{additions}"), Style::default().fg(palette().untracked)),
        Span::styled(format!(" −{deletions}"), Style::default().fg(palette().deleted)),
    ];
    if let Some(bar) = stat_bar(additions, deletions, width) {
        changes.push(Span::raw("  "));
        changes.extend(bar);
    }
    changes.push(Span::styled(format!("  {changed} files"), dim));
    let mut left: Vec<(&str, Vec<Span<'static>>)> = vec![(
        "Author",
        vec![Span::styled(format!("@{author}"), Style::default().fg(palette().accent))],
    )];
    if let Some(reviewers) = reviewers(detail.get("reviews"), detail.get("reviewRequests")) {
        left.push(("Reviewers", reviewers));
    }
    if let Some(labels) = labels(detail.get("labels")) {
        left.push(("Labels", labels));
    }
    let mut right: Vec<(&str, Vec<Span<'static>>)> = vec![
        (
            "Branch",
            vec![
                Span::raw(text("headRefName").to_string()),
                Span::styled(" → ", dim),
                Span::raw(text("baseRefName").to_string()),
            ],
        ),
        ("Changes", changes),
    ];
    if !text("updatedAt").is_empty() {
        right.push((
            "Updated",
            vec![Span::raw(relative_time(text("updatedAt"), now))],
        ));
    }
    out.extend(meta_grid(left, right, width, state_color));

    // The action bar: only keys the viewer actually answers (it binds `m` for
    // the merge menu; approve/checkout/browser live in the sidebar's menu).
    if merge_state(detail) == Mergeability::Ready {
        out.push(Line::default());
        out.extend(crate::ui::wrap_hints(&[("m", "merge ▾")], width as u16, 0));
    }

    if let Some(checks) = checks_summary(detail.get("statusCheckRollup"), width) {
        out.extend(checks);
    }

    let body = text("body").trim();
    if !body.is_empty() {
        out.extend(section_head("Description", width));
        out.extend(
            crate::markdown::render(body, width.saturating_sub(2))
                .into_iter()
                .map(|line| indent_row("  ", line)),
        );
    }
    if let Some(blocks) = conversation(detail.get("comments"), detail.get("reviews"), width) {
        out.extend(blocks);
    }
    out
}

/// The `Label  value` rows of the hero's meta grid on the card surface: side
/// by side at `width >= TWO_COLUMN_MIN`, the left column padded to its widest
/// cell; stacked left-then-right below that.
fn meta_grid(
    left: Vec<(&str, Vec<Span<'static>>)>,
    right: Vec<(&str, Vec<Span<'static>>)>,
    width: usize,
    rail: Color,
) -> Vec<Line<'static>> {
    const LABEL: usize = 11;
    let cell = |label: &str, value: &[Span<'static>]| {
        let mut spans = vec![Span::styled(format!("{label:<LABEL$}"), Style::default().dim())];
        spans.extend(value.iter().cloned());
        spans
    };
    let plain = |spans: &[Span<'static>]| spans.iter().map(Span::width).sum::<usize>();
    let left_w = left
        .iter()
        .map(|(_, value)| LABEL + plain(value))
        .max()
        .unwrap_or(0);
    let right_w = right
        .iter()
        .map(|(_, value)| LABEL + plain(value))
        .max()
        .unwrap_or(0);
    let two_columns = width >= TWO_COLUMN_MIN && 2 + left_w + 4 + right_w <= width;
    if !two_columns {
        return left
            .iter()
            .chain(right.iter())
            .map(|(label, value)| rail_line(rail, cell(label, value)))
            .collect();
    }
    let rows = left.len().max(right.len());
    (0..rows)
        .map(|i| {
            let mut spans = Vec::new();
            let used = match left.get(i) {
                Some((label, value)) => {
                    spans.extend(cell(label, value));
                    LABEL + plain(value)
                }
                None => 0,
            };
            if let Some((label, value)) = right.get(i) {
                spans.push(Span::raw(" ".repeat(left_w - used + 4)));
                spans.extend(cell(label, value));
            }
            rail_line(rail, spans)
        })
        .collect()
}

/// Every reviewer's LATEST verdict (a later review supersedes an earlier one
/// by the same account; bare COMMENTED reviews do not count as a verdict),
/// then the accounts still asked for one. `None` when nobody is involved.
fn reviewers(reviews: Option<&Value>, requests: Option<&Value>) -> Option<Vec<Span<'static>>> {
    let mut latest: Vec<(String, &'static str, Color)> = Vec::new();
    for item in reviews.and_then(Value::as_array).into_iter().flatten() {
        let login = item
            .get("author")
            .and_then(|author| author.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let Some((_, label, color)) =
            review_badge(item.get("state").and_then(Value::as_str).unwrap_or(""))
        else {
            continue;
        };
        match latest.iter_mut().find(|(who, _, _)| *who == login) {
            Some(entry) => *entry = (login, label, color),
            None => latest.push((login, label, color)),
        }
    }
    for item in requests.and_then(Value::as_array).into_iter().flatten() {
        let login = item
            .get("login")
            .or_else(|| item.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !login.is_empty() && !latest.iter().any(|(who, _, _)| *who == login) {
            latest.push((login, "pending", palette().modified));
        }
    }
    if latest.is_empty() {
        return None;
    }
    let mut spans = Vec::new();
    for (i, (login, label, color)) in latest.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(format!("@{login}"), Style::default().fg(palette().accent)));
        spans.push(Span::styled(format!(" ({label})"), Style::default().fg(color)));
    }
    Some(spans)
}

/// GitHub labels as small pills in their own hex color. The text color
/// follows the fill's luminance, so the pill reads on any theme without
/// touching the palette: the fill IS the label's color.
fn labels(labels: Option<&Value>) -> Option<Vec<Span<'static>>> {
    let mut spans = Vec::new();
    for item in labels.and_then(Value::as_array).into_iter().flatten() {
        let name = item.get("name").and_then(Value::as_str).unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let bg = item
            .get("color")
            .and_then(Value::as_str)
            .and_then(hex_color)
            .unwrap_or(palette().keycap_bg);
        let fg = match bg {
            Color::Rgb(r, g, b)
                if 0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)
                    < 128.0 =>
            {
                Color::Rgb(240, 240, 240)
            }
            Color::Rgb(..) => Color::Black,
            _ => palette().keycap_fg,
        };
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            format!(" {name} "),
            Style::default().fg(fg).bg(bg),
        ));
    }
    (!spans.is_empty()).then_some(spans)
}

/// `rrggbb` (GitHub's label color, no `#`) as an RGB color.
fn hex_color(hex: &str) -> Option<Color> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some(Color::Rgb(channel(0)?, channel(2)?, channel(4)?))
}

/// An ISO-8601 UTC stamp (`2026-09-21T09:44:22Z`) as "N days ago" against
/// `now`. Unparseable input falls back to the raw text.
pub fn relative_time(stamp: &str, now: SystemTime) -> String {
    let Some(then) = parse_utc(stamp) else {
        return stamp.to_string();
    };
    let now = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = (now - then).max(0);
    let unit = |n: i64, name: &str| format!("{n} {name}{} ago", if n == 1 { "" } else { "s" });
    match secs {
        s if s < 60 => "just now".to_string(),
        s if s < 3600 => unit(s / 60, "minute"),
        s if s < 86_400 => unit(s / 3600, "hour"),
        s if s < 30 * 86_400 => unit(s / 86_400, "day"),
        s if s < 365 * 86_400 => unit(s / (30 * 86_400), "month"),
        s => unit(s / (365 * 86_400), "year"),
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` to unix seconds — enough for GitHub's stamps
/// without a date crate.
fn parse_utc(stamp: &str) -> Option<i64> {
    let (date, time) = stamp.split_once('T')?;
    let mut ymd = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (ymd.next()??, ymd.next()??, ymd.next()??);
    let mut hms = time
        .trim_end_matches('Z')
        .split(':')
        .map(|p| p.parse::<i64>().ok());
    let (h, min, s) = (hms.next()??, hms.next()??, hms.next()??);
    // Howard Hinnant's days_from_civil.
    let (y, m) = if m <= 2 { (y - 1, m + 12) } else { (y, m) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m - 3) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + min * 60 + s)
}

/// The rollup as one hero verdict: failing first, then pending, else passed.
fn checks_line(rollup: Option<&Value>) -> Option<(&'static str, String, Color)> {
    let checks = parse_checks(rollup);
    if checks.is_empty() {
        return None;
    }
    let count = |status| checks.iter().filter(|c| c.status == status).count();
    let (failed, pending) = (count(CheckStatus::Failed), count(CheckStatus::Pending));
    let plural = |n: usize| if n == 1 { "check" } else { "checks" };
    Some(if failed > 0 {
        (
            "✗",
            format!("{failed} of {} {} failed", checks.len(), plural(checks.len())),
            palette().deleted,
        )
    } else if pending > 0 {
        (
            "●",
            format!("{pending} {} pending", plural(pending)),
            palette().modified,
        )
    } else {
        (
            "✓",
            format!("{} {} passed", checks.len(), plural(checks.len())),
            palette().untracked,
        )
    })
}

/// A row on a card surface: the 1-cell left "rail" in the card's accent
/// color — Linear-style — then a space, then the content. The viewer pads
/// rows whose line style carries a background out to the full pane width,
/// so every card reads as a full-bleed band, never a box.
fn rail_line(rail: Color, spans: Vec<Span<'static>>) -> Line<'static> {
    let mut all = vec![
        Span::styled("▍", Style::default().fg(rail)),
        Span::raw(" "),
    ];
    all.extend(spans);
    let mut out = Line::from(all);
    out.style = Style::default().bg(palette().card_bg);
    out
}

/// A body row under a section head: `prefix` (an indent, or a rail glyph)
/// before the content, no card surface. A blank markdown row stays blank so
/// the wrap's hanging anchor has nothing to re-emit.
fn indent_row(prefix: &str, line: Line<'static>) -> Line<'static> {
    if line.spans.is_empty() {
        return line;
    }
    let mut all = vec![Span::raw(prefix.to_string())];
    all.extend(line.spans);
    let mut out = Line::from(all);
    out.style = line.style;
    out
}

/// A section head: a blank row, the UPPERCASE title in the accent, a dim
/// full-width rule, a blank row. Three visual levels then read apart —
/// section head (caps + rule) > markdown heading (bold header accent, one
/// blank above) > body.
fn section_head(title: &str, width: usize) -> Vec<Line<'static>> {
    vec![
        Line::default(),
        Line::from(Span::styled(
            format!("  {}", title.to_uppercase()),
            Style::default()
                .fg(palette().accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("  {}", "─".repeat(width.saturating_sub(2).max(2))),
            Style::default().dim(),
        )),
        Line::default(),
    ]
}

/// A filled state pill — ` Open `, ` Merged ` — the GitHub look: the fill,
/// not the text, carries the meaning. Dark text on the fill stays readable
/// on light and dark terminals alike (a white label would wash out on a
/// light profile, where ANSI white renders as pale grey).
fn pill(label: &str, bg: Color) -> Span<'static> {
    Span::styled(
        format!(" {label} "),
        Style::default()
            .fg(Color::Black)
            .bg(bg)
            .add_modifier(Modifier::BOLD),
    )
}

/// The diff stat bar as a two-run gradient: green for the additions, red for
/// the deletions, each run fading toward the join (brightest at the outer
/// edge) like the classic GitHub bar — enough cells to read at a glance
/// without crowding a narrow pane.
fn stat_bar(additions: u64, deletions: u64, width: usize) -> Option<Vec<Span<'static>>> {
    let cells = ((width as u64) / 8).clamp(4, 12);
    let total = additions + deletions;
    if total == 0 {
        return None;
    }
    let mut green = (additions * cells + total / 2) / total;
    if additions > 0 && green == 0 {
        green = 1;
    }
    if deletions > 0 && green >= cells {
        green = cells.saturating_sub(1);
    }
    let green = green.min(cells);
    let red = cells - green;
    let mut spans = Vec::new();
    for i in 0..green {
        let frac = if green > 1 {
            i as f32 / (green - 1) as f32
        } else {
            0.0
        };
        spans.push(Span::styled(
            "█",
            Style::default().fg(ramp(palette().untracked, frac)),
        ));
    }
    for i in 0..red {
        let frac = if red > 1 {
            (red - 1 - i) as f32 / (red - 1) as f32
        } else {
            0.0
        };
        spans.push(Span::styled(
            "█",
            Style::default().fg(ramp(palette().deleted, frac)),
        ));
    }
    Some(spans)
}

/// One shade of a color falling toward dark — the diff bar's join fades
/// without mixing its two hues. Named colors (the terminal theme) carry no
/// RGB to ramp from, so they stay flat and solid.
fn ramp(color: Color, frac: f32) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color;
    };
    let f = frac.clamp(0.0, 1.0) * 0.72;
    let to = |channel: u8| (f32::from(channel) * (1.0 - f)).round() as u8;
    Color::Rgb(to(r), to(g), to(b))
}

/// Whether the overview's merge button is live, and why not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mergeability {
    /// An open request `gh pr merge` may attempt — remaining failures (failing
    /// checks, behind the base) surface as the command's own error.
    Ready,
    /// Conflicting files: no automatic merge exists.
    Conflicts,
    /// Already merged or closed: the button becomes a state pill.
    Merged,
    Closed,
}

/// GitHub's merge verdict for the overview: `mergeStateStatus` DIRTY means
/// conflicting files, anything else on an open request stays an attempt.
pub fn merge_state(detail: &Value) -> Mergeability {
    match detail.get("state").and_then(Value::as_str).unwrap_or("") {
        "MERGED" => Mergeability::Merged,
        "CLOSED" => Mergeability::Closed,
        _ => match detail
            .get("mergeStateStatus")
            .and_then(Value::as_str)
            .unwrap_or("")
        {
            "DIRTY" => Mergeability::Conflicts,
            _ => Mergeability::Ready,
        },
    }
}

/// The pull request's state as a badge label + color. Drafts read Draft
/// even though GitHub reports them OPEN.
fn state_badge(detail: &Value) -> (&'static str, ratatui::style::Color) {
    if detail
        .get("isDraft")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return ("Draft", palette().modified);
    }
    match detail.get("state").and_then(Value::as_str).unwrap_or("") {
        "MERGED" => ("Merged", palette().accent),
        "CLOSED" => ("Closed", palette().deleted),
        _ => ("Open", palette().untracked),
    }
}

/// The review decision as (glyph, label, color), or `None` when undecided.
fn review_line(
    decision: Option<&str>,
) -> Option<(&'static str, &'static str, ratatui::style::Color)> {
    match parse_review(decision) {
        ReviewState::Approved => Some(("✓", "approved", palette().untracked)),
        ReviewState::ChangesRequested => Some(("✗", "changes requested", palette().deleted)),
        ReviewState::Pending if decision == Some("REVIEW_REQUIRED") => {
            Some(("●", "review required", palette().modified))
        }
        ReviewState::Pending => None,
    }
}

/// GitHub's merge verdict for an open request: whether the button will
/// attempt or the files conflict. Merged/closed show nothing extra — the
/// state pill already owns them.
fn mergeability_line(
    detail: &Value,
) -> Option<(&'static str, &'static str, ratatui::style::Color)> {
    match merge_state(detail) {
        Mergeability::Ready => Some(("✓", "mergeable", palette().untracked)),
        Mergeability::Conflicts => Some(("✕", "has conflicts", palette().deleted)),
        Mergeability::Merged | Mergeability::Closed => None,
    }
}

/// One check's outcome.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CheckStatus {
    Passed,
    Failed,
    Pending,
}

/// A named check rollup entry.
struct Check {
    /// Failing and pending checks list theirs; passing ones only count.
    /// `gh` names CheckRuns `name` and StatusContexts `context`.
    name: String,
    status: CheckStatus,
}

/// The rollup as checks, failing first — each group keeps its arrival order.
fn parse_checks(rollup: Option<&Value>) -> Vec<Check> {
    let mut out = Vec::new();
    for item in rollup.and_then(Value::as_array).into_iter().flatten() {
        let state = item
            .get("conclusion")
            .or_else(|| item.get("state"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_uppercase();
        let status = if ["SUCCESS", "NEUTRAL"].iter().any(|w| state.contains(w)) {
            CheckStatus::Passed
        } else if ["FAILURE", "ERROR", "CANCELLED", "TIMED_OUT"]
            .iter()
            .any(|w| state.contains(w))
        {
            CheckStatus::Failed
        } else {
            CheckStatus::Pending
        };
        let name = item
            .get("name")
            .or_else(|| item.get("context"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        out.push(Check { name, status });
    }
    out.sort_by_key(|check| match check.status {
        CheckStatus::Failed => 0,
        CheckStatus::Pending => 1,
        CheckStatus::Passed => 2,
    });
    out
}

/// The Checks section: one row per non-empty outcome group, the failing
/// and pending checks named under theirs (capped). Passing checks are
/// counted, not listed — a green wall of fifty names helps no one.
fn checks_summary(rollup: Option<&Value>, width: usize) -> Option<Vec<Line<'static>>> {
    let checks = parse_checks(rollup);
    if checks.is_empty() {
        return None;
    }
    /// Named rows per group before the "+N more" tail.
    const NAMED: usize = 6;
    if checks.iter().all(|check| check.status == CheckStatus::Passed) {
        return None;
    }
    let mut out = section_head("Checks", width);
    for (status, glyph, label, color) in [
        (
            CheckStatus::Failed,
            "✗",
            "failed",
            palette().deleted,
        ),
        (
            CheckStatus::Pending,
            "●",
            "pending",
            palette().modified,
        ),
        (
            CheckStatus::Passed,
            "✓",
            "passed",
            palette().untracked,
        ),
    ] {
        let group: Vec<&Check> = checks
            .iter()
            .filter(|check| check.status == status)
            .collect();
        if group.is_empty() {
            continue;
        }
        out.push(indent_row("  ", Line::from(vec![Span::styled(
            format!("{glyph} {} {label}", group.len()),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )])));
        if status == CheckStatus::Passed {
            continue;
        }
        for check in group.iter().take(NAMED) {
            let name = if check.name.is_empty() {
                "(unnamed)".to_string()
            } else {
                check.name.clone()
            };
            out.push(indent_row("    ", Line::from(Span::styled(name, Style::default().fg(color)))));
        }
        if group.len() > NAMED {
            out.push(indent_row(
                "    ",
                Line::from(Span::styled(
                    format!("…and {} more", group.len() - NAMED),
                    Style::default().dim(),
                )),
            ));
        }
    }
    Some(out)
}

/// One conversation entry: a comment or a review.
struct Remark {
    /// ISO stamp, for oldest-first ordering.
    stamp: String,
    author: String,
    /// The review's badge; `None` for plain comments.
    badge: Option<(&'static str, &'static str, ratatui::style::Color)>,
    body: String,
}

/// A review's badge: approved, changes requested, dismissed. Plain
/// comments and bare COMMENTED reviews carry none.
fn review_badge(state: &str) -> Option<(&'static str, &'static str, ratatui::style::Color)> {
    match state {
        "APPROVED" => Some(("✓", "approved", palette().untracked)),
        "CHANGES_REQUESTED" => Some(("✗", "requested changes", palette().deleted)),
        "DISMISSED" => Some(("○", "dismissed", palette().ignored)),
        _ => None,
    }
}

/// Comments and reviews, oldest first, as a feed: a `●` dot per entry in
/// the review's color, the `@author · state · time` line beside it, the
/// body under it behind a thin `▎` rail in the same color. A blank row
/// separates entries so the section reads as entries, not one slab.
fn conversation(
    comments: Option<&Value>,
    reviews: Option<&Value>,
    width: usize,
) -> Option<Vec<Line<'static>>> {
    let login = |item: &Value| {
        item.get("author")
            .and_then(|author| author.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let mut entries: Vec<Remark> = Vec::new();
    for item in comments.and_then(Value::as_array).into_iter().flatten() {
        let body = item
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if !body.is_empty() {
            entries.push(Remark {
                stamp: item
                    .get("createdAt")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                author: login(item),
                badge: None,
                body: body.to_string(),
            });
        }
    }
    for item in reviews.and_then(Value::as_array).into_iter().flatten() {
        let state = item.get("state").and_then(Value::as_str).unwrap_or("");
        let body = item
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if !body.is_empty() || state != "COMMENTED" {
            entries.push(Remark {
                stamp: item
                    .get("submittedAt")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                author: login(item),
                badge: review_badge(state),
                body: body.to_string(),
            });
        }
    }
    if entries.is_empty() {
        return None;
    }
    entries.sort_by(|a, b| a.stamp.cmp(&b.stamp));
    let mut out = section_head(&format!("Conversation  ({})", entries.len()), width);
    for (i, remark) in entries.iter().enumerate() {
        if i > 0 {
            out.push(Line::default());
        }
        let day = remark.stamp.split('T').next().unwrap_or(&remark.stamp);
        let dot_color = remark.badge.map_or(palette().ignored, |(_, _, color)| color);
        let mut head = vec![
            Span::styled("● ", Style::default().fg(dot_color)),
            Span::styled(
                format!("@{}", remark.author),
                Style::default().fg(palette().accent),
            ),
        ];
        match remark.badge {
            Some((glyph, label, color)) => {
                head.push(Span::styled(" · ", Style::default().dim()));
                head.push(Span::styled(
                    format!("{glyph} {label}"),
                    Style::default().fg(color),
                ));
            }
            None => head.push(Span::styled(" · commented", Style::default().dim())),
        }
        head.push(Span::styled(format!(" · {day}"), Style::default().dim()));
        out.push(indent_row("  ", Line::from(head)));
        let rail = Span::styled("  ▎ ", Style::default().fg(dot_color));
        for line in crate::markdown::render(&remark.body, width.saturating_sub(4)) {
            let mut row = vec![rail.clone()];
            row.extend(line.spans);
            let mut row = Line::from(row);
            row.style = line.style;
            out.push(row);
        }
    }
    Some(out)
}

/// What `gh pr merge` does with the branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeMethod {
    Commit,
    Squash,
    Rebase,
}

impl MergeMethod {
    pub const ALL: [MergeMethod; 3] = [Self::Commit, Self::Squash, Self::Rebase];

    /// The `gh pr merge` flag.
    pub fn flag(self) -> &'static str {
        match self {
            Self::Commit => "--merge",
            Self::Squash => "--squash",
            Self::Rebase => "--rebase",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Commit => "Merge Commit",
            Self::Squash => "Squash and Merge",
            Self::Rebase => "Rebase and Merge",
        }
    }

    /// The one-word name the overview's method chip shows.
    pub fn short(self) -> &'static str {
        match self {
            Self::Commit => "Merge",
            Self::Squash => "Squash",
            Self::Rebase => "Rebase",
        }
    }
}

/// The verdict `gh pr review` sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    RequestChanges,
    Comment,
}

impl Verdict {
    /// The `gh pr review` flag.
    pub fn flag(self) -> &'static str {
        match self {
            Self::Approve => "--approve",
            Self::RequestChanges => "--request-changes",
            Self::Comment => "--comment",
        }
    }
}

/// The arguments `gh pr merge` runs with.
pub fn merge_args(number: u64, method: MergeMethod) -> Vec<String> {
    vec![
        "pr".into(),
        "merge".into(),
        number.to_string(),
        method.flag().into(),
    ]
}

/// The arguments `gh pr review` runs with.
pub fn review_args(number: u64, verdict: Verdict, body: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "pr".into(),
        "review".into(),
        number.to_string(),
        verdict.flag().into(),
    ];
    if !body.trim().is_empty() {
        args.push("--body".into());
        args.push(body.trim().to_string());
    }
    args
}

/// Check the pull request's branch out into the working tree.
pub fn checkout(root: &Path, number: u64) -> Result<String, String> {
    run(root, &["pr".into(), "checkout".into(), number.to_string()])
}

/// Merge the pull request with `method`.
pub fn merge(root: &Path, number: u64, method: MergeMethod) -> Result<String, String> {
    run(root, &merge_args(number, method))
}

/// Comment on the pull request.
pub fn comment(root: &Path, number: u64, body: &str) -> Result<String, String> {
    run(
        root,
        &[
            "pr".into(),
            "comment".into(),
            number.to_string(),
            "--body".into(),
            body.to_string(),
        ],
    )
}

/// Send a review with `verdict`.
pub fn review(root: &Path, number: u64, verdict: Verdict, body: &str) -> Result<String, String> {
    run(root, &review_args(number, verdict, body))
}

/// Mark a draft pull request ready for review.
pub fn ready(root: &Path, number: u64) -> Result<String, String> {
    run(root, &["pr".into(), "ready".into(), number.to_string()])
}

/// Open the pull request in the browser.
pub fn open_in_browser(root: &Path, number: u64) -> Result<String, String> {
    run(
        root,
        &[
            "pr".into(),
            "view".into(),
            number.to_string(),
            "--web".into(),
        ],
    )
}

/// One review conversation of a pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    /// The GraphQL node id `resolveReviewThread` needs.
    pub id: String,
    pub path: String,
    pub line: u64,
    pub author: String,
    /// The first comment's text, one line.
    pub snippet: String,
}

/// The pull request's unresolved review conversations.
pub fn threads(root: &Path, number: u64) -> Result<Vec<Thread>, String> {
    let (owner, repo) = repo_slug(root)?;
    let query = "query($owner:String!,$repo:String!,$number:Int!){repository(owner:$owner,name:$repo){pullRequest(number:$number){reviewThreads(first:100){nodes{id isResolved path line comments(first:1){nodes{body author{login}}}}}}}}";
    let out = run(
        root,
        &[
            "api".into(),
            "graphql".into(),
            "-f".into(),
            format!("query={query}"),
            "-f".into(),
            format!("owner={owner}"),
            "-f".into(),
            format!("repo={repo}"),
            "-F".into(),
            format!("number={number}"),
        ],
    )?;
    let value: Value = serde_json::from_str(&out).map_err(|e| format!("gh json: {e}"))?;
    Ok(parse_threads(&value))
}

/// The unresolved threads in a `reviewThreads` GraphQL response.
pub fn parse_threads(value: &Value) -> Vec<Thread> {
    let nodes = value
        .pointer("/data/repository/pullRequest/reviewThreads/nodes")
        .and_then(Value::as_array);
    let mut out = Vec::new();
    for node in nodes.into_iter().flatten() {
        if node
            .get("isResolved")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let Some(id) = node.get("id").and_then(Value::as_str) else {
            continue;
        };
        let comment = node
            .pointer("/comments/nodes/0")
            .and_then(|comment| comment.as_object());
        let snippet = comment
            .and_then(|comment| comment.get("body"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        out.push(Thread {
            id: id.to_string(),
            path: node
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            line: node.get("line").and_then(Value::as_u64).unwrap_or(0),
            author: comment
                .and_then(|comment| comment.get("author"))
                .and_then(|author| author.get("login"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            snippet,
        });
    }
    out
}

/// Mark a review conversation resolved.
pub fn resolve_thread(root: &Path, thread_id: &str) -> Result<String, String> {
    let query = "mutation($id:ID!){resolveReviewThread(input:{threadId:$id}){thread{isResolved}}}";
    run(
        root,
        &[
            "api".into(),
            "graphql".into(),
            "-f".into(),
            format!("query={query}"),
            "-f".into(),
            format!("id={thread_id}"),
        ],
    )
}

/// The repository's `owner/name`, as `gh` resolves it for the remote.
fn repo_slug(root: &Path) -> Result<(String, String), String> {
    let out = run(
        root,
        &[
            "repo".into(),
            "view".into(),
            "--json".into(),
            "owner,name".into(),
        ],
    )?;
    let value: Value = serde_json::from_str(&out).map_err(|e| format!("gh json: {e}"))?;
    let owner = value
        .pointer("/owner/login")
        .and_then(Value::as_str)
        .ok_or("gh repo view: no owner")?
        .to_string();
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or("gh repo view: no name")?
        .to_string();
    Ok((owner, name))
}

/// Run `gh` in `root`, returning stdout, with stderr's first line as the error.
fn run(root: &Path, args: &[String]) -> Result<String, String> {
    let out = Command::new("gh")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("gh: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let message = stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("gh failed");
        return Err(message.trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"[
      {"number":74,"title":"feat: previews","author":{"login":"FacuVCanale"},
       "headRefName":"feat/image-preview","baseRefName":"main","isDraft":false,
       "updatedAt":"2026-09-21T09:44:22Z","url":"https://github.com/o/r/pull/74",
       "additions":2008,"deletions":132,"changedFiles":6,"reviewDecision":"APPROVED"},
      {"number":73,"title":"feat: page the preview","author":{"login":"wasuregusa18"},
       "headRefName":"feat/space-page-down","baseRefName":"main","isDraft":true,
       "updatedAt":"2026-09-20T09:26:14Z","url":"https://github.com/o/r/pull/73",
       "additions":12,"deletions":6,"changedFiles":2,"reviewDecision":"CHANGES_REQUESTED"},
      {"number":72,"title":"fix: glow width","author":{"login":"danielpradilla"},
       "headRefName":"fix/glow","baseRefName":"main","isDraft":false,
       "updatedAt":"2026-09-20T09:16:56Z","url":"https://github.com/o/r/pull/72",
       "additions":53,"deletions":9,"changedFiles":1}
    ]"#;

    #[test]
    fn pr_list_rows_keep_number_author_branches_and_review() {
        let prs = parse_pr_list(LIST);
        assert_eq!(prs.len(), 3);
        assert_eq!(prs[0].number, 74);
        assert_eq!(prs[0].author, "FacuVCanale");
        assert_eq!(prs[0].head, "feat/image-preview");
        assert_eq!(prs[0].base, "main");
        assert_eq!(prs[0].review, ReviewState::Approved);
        assert!(!prs[0].draft);
        assert!(prs[1].draft);
        assert_eq!(prs[1].review, ReviewState::ChangesRequested);
        assert_eq!(
            prs[2].review,
            ReviewState::Pending,
            "a missing reviewDecision is pending, not a parse failure"
        );
        assert_eq!(
            prs[0].detail(),
            "#74 FacuVCanale feat/image-preview → main +2008 −132"
        );
        assert_eq!(prs[2].changed_files, 1);
    }

    #[test]
    fn merge_and_review_args_carry_the_flag_and_body() {
        assert_eq!(
            merge_args(7, MergeMethod::Squash),
            ["pr", "merge", "7", "--squash"]
        );
        assert_eq!(
            review_args(7, Verdict::Approve, ""),
            ["pr", "review", "7", "--approve"],
            "an empty body adds no --body"
        );
        assert_eq!(
            review_args(7, Verdict::RequestChanges, "  please split  "),
            [
                "pr",
                "review",
                "7",
                "--request-changes",
                "--body",
                "please split"
            ]
        );
        assert_eq!(MergeMethod::Rebase.flag(), "--rebase");
        assert_eq!(MergeMethod::Commit.label(), "Merge Commit");
        assert_eq!(Verdict::Comment.flag(), "--comment");
    }

    #[test]
    fn unresolved_threads_parse_from_the_graphql_response() {
        let value: Value = serde_json::from_str(
            r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[
              {"id":"T1","isResolved":false,"path":"src/app.rs","line":12,
               "comments":{"nodes":[{"body":"rename this\nsecond line","author":{"login":"bob"}}]}},
              {"id":"T2","isResolved":true,"path":"a","line":1,
               "comments":{"nodes":[{"body":"done","author":{"login":"ann"}}]}},
              {"id":"T3","isResolved":false,"path":"lib.rs","line":0,
               "comments":{"nodes":[{"body":"nit","author":{"login":"cid"}}]}}
            ]}}}}}"#,
        )
        .unwrap();
        let threads = parse_threads(&value);
        assert_eq!(threads.len(), 2, "resolved conversations drop out");
        assert_eq!(threads[0].id, "T1");
        assert_eq!(threads[0].path, "src/app.rs");
        assert_eq!(threads[0].line, 12);
        assert_eq!(threads[0].author, "bob");
        assert_eq!(threads[0].snippet, "rename this", "one line only");
        assert_eq!(threads[1].snippet, "nit");
        assert!(parse_threads(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn a_broken_list_is_empty_not_a_panic() {
        assert!(parse_pr_list("").is_empty());
        assert!(parse_pr_list("gh: not authenticated").is_empty());
        assert!(parse_pr_list("[{\"title\":\"no number\"}]").is_empty());
    }

    #[test]
    fn review_glyphs_mark_the_three_states() {
        assert_eq!(ReviewState::Pending.glyph(), "○");
        assert_eq!(ReviewState::Approved.glyph(), "✓");
        assert_eq!(ReviewState::ChangesRequested.glyph(), "✗");
    }

    #[test]
    fn pr_files_map_status_letters_and_renames() {
        let rows = parse_pr_files(
            "added\tsrc/new.rs\t\t12\t0\nmodified\tCLAUDE.md\t\t54\t0\nremoved\told.txt\t\t0\t9\n\
             renamed\tsrc/new_name.rs\tsrc/old_name.rs\t3\t1\ncopied\tb.rs\ta.rs\t1\t1\n\
             changed\tweird.bin\t\t0\t0\n",
        );
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0].entry.letter, 'A');
        assert_eq!(rows[0].additions, 12);
        assert_eq!(rows[1].entry.letter, 'M');
        assert_eq!(rows[2].entry.letter, 'D');
        assert_eq!(rows[2].deletions, 9);
        assert_eq!(rows[3].entry.letter, 'R');
        assert_eq!(rows[3].entry.path, "src/new_name.rs");
        assert_eq!(rows[3].entry.orig.as_deref(), Some("src/old_name.rs"));
        assert_eq!(rows[4].entry.letter, 'C');
        assert_eq!(
            rows[5].entry.letter, 'M',
            "an unknown status reads as modified"
        );
        assert_eq!(rows[5].entry.orig, None);
    }

    const PATCH: &str = "\
diff --git a/CLAUDE.md b/CLAUDE.md
index 111..222 100644
--- a/CLAUDE.md
+++ b/CLAUDE.md
@@ -1 +1 @@
-old
+new
diff --git a/src/app.rs b/src/app.rs
index 333..444 100644
--- a/src/app.rs
+++ b/src/app.rs
@@ -2 +2 @@
-gone
+here
diff --git \"a/with space.rs\" \"b/with space.rs\"
index 555..666 100644
--- a/with space.rs
+++ b/with space.rs
@@ -1 +1 @@
-a
+b
";

    #[test]
    fn a_patch_slices_into_one_file_sections() {
        let section = patch_for_file(PATCH, "src/app.rs").unwrap();
        assert!(section.starts_with("diff --git a/src/app.rs"));
        assert!(section.contains("+here"));
        assert!(
            !section.contains("CLAUDE.md"),
            "no neighbouring file leaks in"
        );
        assert!(!section.contains("with space"));
        let first = patch_for_file(PATCH, "CLAUDE.md").unwrap();
        assert!(first.contains("+new"));
        assert!(!first.contains("src/app.rs"));
    }

    #[test]
    fn a_patch_slice_handles_quoted_paths_and_misses() {
        let section = patch_for_file(PATCH, "with space.rs").unwrap();
        assert!(section.contains("+b"), "git-quoted paths still match");
        assert!(patch_for_file(PATCH, "nope.rs").is_none());
        assert!(patch_for_file("", "any.rs").is_none());
    }

    fn joined(lines: &[Line<'static>]) -> String {
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

    #[test]
    fn overview_renders_the_pr_and_its_conversation() {
        let detail: Value = serde_json::from_str(
            r#"{"number":74,"title":"feat: previews","author":{"login":"Facu"},
                "state":"OPEN","isDraft":false,"headRefName":"feat/x","baseRefName":"main",
                "body":"Adds full-resolution previews. See [docs](https://example.com/x).",
                "url":"https://github.com/o/r/pull/74",
                "additions":10,"deletions":2,"changedFiles":3,"reviewDecision":"APPROVED",
                "statusCheckRollup":[{"conclusion":"SUCCESS"},{"conclusion":"SUCCESS"},
                                      {"conclusion":"FAILURE","name":"build"},
                                      {"state":"PENDING","context":"lint"}],
                "comments":[{"author":{"login":"bob"},"body":"nit: rename this",
                             "createdAt":"2026-09-20T10:00:00Z"}],
                "reviews":[{"author":{"login":"ann"},"state":"CHANGES_REQUESTED",
                            "body":"please split","submittedAt":"2026-09-19T08:00:00Z"},
                           {"author":{"login":"cid"},"state":"APPROVED","body":"",
                            "submittedAt":"2026-09-21T08:00:00Z"}]}"#,
        )
        .unwrap();
        let lines = render_overview(&detail, 60);
        let all = joined(&lines);
        assert!(all.contains("feat: previews"), "{all}");
        assert!(all.contains("#74"), "{all}");
        assert!(all.contains(" Open "), "the state pill: {all}");
        assert!(all.contains("@Facu"), "{all}");
        assert!(all.contains("feat/x → main"), "{all}");
        assert!(all.contains("+10"), "{all}");
        assert!(all.contains("−2"), "{all}");
        assert!(all.contains("3 files"), "{all}");
        assert!(all.contains("████"), "the diff stat bar: {all}");
        assert!(all.contains("✓ approved"), "{all}");
        assert!(all.contains("✗ 1 of 4 checks failed"), "the hero checks verdict: {all}");
        assert!(all.contains("CHECKS"), "{all}");
        assert!(all.contains("✗ 1 failed"), "{all}");
        assert!(all.contains("build"), "failing checks are named: {all}");
        assert!(all.contains("● 1 pending"), "{all}");
        assert!(all.contains("lint"), "{all}");
        assert!(all.contains("✓ 2 passed"), "{all}");
        assert!(
            all.find("✗ 1 failed").unwrap() < all.find("● 1 pending").unwrap(),
            "failing checks come first: {all}"
        );
        assert!(all.contains("DESCRIPTION"), "{all}");
        assert!(all.contains("Adds full-resolution previews."), "{all}");
        assert!(all.contains("docs"), "{all}");
        assert!(
            !all.contains("[docs](https://example.com/x)"),
            "link syntax never reaches the pane: {all}"
        );
        assert!(!all.contains("**"), "markdown markers are gone: {all}");
        assert!(all.contains("CONVERSATION  (3)"), "{all}");
        assert!(all.contains("@ann · ✗ requested changes · 2026-09-19"), "{all}");
        assert!(all.contains("@bob · commented · 2026-09-20"), "{all}");
        assert!(all.contains("@cid · ✓ approved · 2026-09-21"), "{all}");
        assert!(all.contains('●'), "the timeline dots: {all}");
        assert!(all.contains('▍'), "the hero rail: {all}");
        assert!(!all.contains('┌'), "nothing is boxed: {all}");
        assert!(all.contains("✓ mergeable"), "the merge verdict: {all}");
        assert!(all.contains("Reviewers"), "{all}");
        assert!(all.contains("@ann (requested changes)"), "{all}");
        assert!(all.contains("@cid (approved)"), "{all}");
        assert!(all.contains(" merge ▾"), "the action bar: {all}");
        assert!(!all.contains("https://github.com/o/r/pull/74"), "the URL row is gone: {all}");
        let changes = all.find("requested changes").unwrap();
        let nit = all.find("nit: rename this").unwrap();
        let approved = all.find("· ✓ approved · 2026-09-21").unwrap();
        assert!(changes < nit, "oldest first");
        assert!(nit < approved);
        // The state pill is a fill, not a tint: dark text on the state color.
        let open_pill = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content == " Open ")
            .unwrap();
        assert_eq!(open_pill.style.bg, Some(palette().untracked));
        assert_eq!(open_pill.style.fg, Some(Color::Black));
    }

    #[test]
    fn stat_bar_scales_green_against_red() {
        let cells = |width: usize| ((width as u64) / 8).clamp(4, 12);
        let expect = |adds: u64, dels: u64, width: usize| {
            let total = adds + dels;
            if total == 0 {
                return None;
            }
            let n = cells(width);
            let mut green = (adds * n + total / 2) / total;
            if adds > 0 && green == 0 {
                green = 1;
            }
            if dels > 0 && green >= n {
                green = n - 1;
            }
            Some(green.min(n))
        };
        let bar = stat_bar(10, 2, 80).unwrap();
        let text: String = bar.iter().map(|span| span.content.to_string()).collect();
        assert_eq!(text, "█".repeat(80 / 8), "{text}");
        assert_eq!(bar.len(), 10, "{text}");
        let green = expect(10, 2, 80).unwrap() as usize;
        assert_eq!(green, 8, "10 of 12 changes are additions");
        assert_eq!(bar[0].style.fg, Some(palette().untracked), "the outer edge is the full green");
        // The tail joins dark: not the full red, a shade of it.
        assert_ne!(bar[green].style.fg, Some(palette().deleted));
        assert_eq!(bar[bar.len() - 1].style.fg, Some(palette().deleted));
        assert!(stat_bar(0, 0, 80).is_none());
        let red = stat_bar(0, 3, 80).unwrap();
        assert_eq!(red.len(), 10, "all deletions, no green sliver");
        let green_only = stat_bar(3, 0, 80).unwrap();
        assert_eq!(green_only.len(), 10, "all additions, no red sliver");
        // A trace of additions still reads next to a sea of deletions.
        let trace = stat_bar(1, 99, 80).unwrap();
        assert_eq!(trace[0].style.fg, Some(palette().untracked));
    }

    #[test]
    fn overview_without_body_or_conversation_stays_minimal() {
        let detail: Value = serde_json::from_str(
            r#"{"number":1,"title":"t","author":{"login":"me"},"state":"OPEN",
                "headRefName":"a","baseRefName":"b","body":"   ","url":"u"}"#,
        )
        .unwrap();
        let all = joined(&render_overview(&detail, 60));
        assert!(!all.contains("CONVERSATION"), "{all}");
        assert!(
            !all.contains("CHECKS"),
            "no rollup adds no checks section: {all}"
        );
        assert!(!all.contains("approved"), "{all}");
        assert!(!all.contains("DESCRIPTION"), "{all}");
        assert!(!all.contains("Reviewers"), "{all}");
        assert!(!all.contains("Labels"), "{all}");
        assert!(all.contains("#1"), "{all}");
        assert!(all.contains(" Open "), "the state pill is always there: {all}");
        assert!(!all.contains('┌'), "nothing is boxed: {all}");
    }

    #[test]
    fn state_badges_cover_draft_merged_and_closed() {
        let pill = |json: &str| {
            let detail: Value = serde_json::from_str(json).unwrap();
            joined(&render_overview(&detail, 60))
        };
        let draft = pill(r#"{"number":1,"title":"t","isDraft":true,"state":"OPEN"}"#);
        assert!(draft.contains(" Draft "), "{draft}");
        let merged = pill(r#"{"number":1,"title":"t","state":"MERGED"}"#);
        assert!(merged.contains(" Merged "), "{merged}");
        let closed = pill(r#"{"number":1,"title":"t","state":"CLOSED"}"#);
        assert!(closed.contains(" Closed "), "{closed}");
    }

    #[test]
    fn review_line_names_all_three_decisions() {
        let (glyph, label, _) = review_line(Some("APPROVED")).unwrap();
        assert_eq!((glyph, label), ("✓", "approved"));
        let (glyph, label, _) = review_line(Some("CHANGES_REQUESTED")).unwrap();
        assert_eq!((glyph, label), ("✗", "changes requested"));
        let (glyph, label, _) = review_line(Some("REVIEW_REQUIRED")).unwrap();
        assert_eq!((glyph, label), ("●", "review required"));
        assert!(review_line(None).is_none());
        assert!(review_line(Some("COMMENTED")).is_none());
    }

    #[test]
    fn checks_summary_lists_failed_names_and_counts_the_rest() {
        let rollup: Value = serde_json::from_str(
            r#"[{"conclusion":"FAILURE","name":"build"},
                {"conclusion":"FAILURE","name":"deploy"},
                {"state":"PENDING","context":"lint"},
                {"conclusion":"SUCCESS"},{"conclusion":"NEUTRAL"},
                {"conclusion":"SKIPPED"}]"#,
        )
        .unwrap();
        let all = joined(&checks_summary(Some(&rollup), 60).unwrap());
        assert!(all.contains("✗ 2 failed"), "{all}");
        assert!(all.contains("build"), "{all}");
        assert!(all.contains("deploy"), "{all}");
        assert!(all.contains("● 2 pending"), "{all}");
        assert!(all.contains("lint"), "{all}");
        // SKIPPED is neither success nor failure: it lands in pending.
        assert!(all.contains("(unnamed)"), "{all}");
        assert!(all.contains("✓ 2 passed"), "{all}");
        let failed = all.find("✗ 2 failed").unwrap();
        let pending = all.find("● 2 pending").unwrap();
        let passed = all.find("✓ 2 passed").unwrap();
        assert!(failed < pending && pending < passed, "{all}");
        assert!(checks_summary(None, 60).is_none());
        assert!(checks_summary(Some(&Value::Array(vec![])), 60).is_none());
    }

    #[test]
    fn conversation_indents_bodies_and_badges_reviews() {
        let comments: Value = serde_json::from_str(
            r#"[{"author":{"login":"bob"},"body":"- [ ] todo\n`code`",
                "createdAt":"2026-09-20T10:00:00Z"}]"#,
        )
        .unwrap();
        let reviews: Value = serde_json::from_str(
            r#"[{"author":{"login":"ann"},"state":"DISMISSED","body":"",
                "submittedAt":"2026-09-19T08:00:00Z"}]"#,
        )
        .unwrap();
        let lines = conversation(Some(&comments), Some(&reviews), 60).unwrap();
        let all = joined(&lines);
        assert!(all.contains("@ann · ○ dismissed · 2026-09-19"), "{all}");
        assert!(all.contains("@bob · commented · 2026-09-20"), "{all}");
        assert!(all.contains("☐ todo"), "bodies are kept: {all}");
        assert!(all.contains('●'), "one dot per entry: {all}");
        assert!(all.contains("▎ ☐ todo"), "body rows sit behind the thin rail: {all}");
        assert!(!all.contains('▍'), "no card rail in the conversation: {all}");
        assert!(!all.contains("- [ ]"), "task syntax is gone: {all}");
        assert!(!all.contains('`'), "code ticks are gone: {all}");
        assert!(!all.contains('┌'), "entries are not boxed: {all}");
        assert!(conversation(None, None, 60).is_none());
    }

    #[test]
    fn overview_sections_are_ordered_and_body_rows_carry_no_rail() {
        let detail: Value = serde_json::from_str(
            r###"{"number":9,"title":"t","author":{"login":"me"},"state":"OPEN",
                "headRefName":"a","baseRefName":"b",
                "body":"## What\n\n\nfirst para\n## Why\nsecond para",
                "labels":[{"name":"bug","color":"d73a4a"},{"name":"ui","color":"fef2c0"}],
                "reviewRequests":[{"login":"dan"}],
                "updatedAt":"2026-09-20T00:00:00Z",
                "statusCheckRollup":[{"conclusion":"SUCCESS","name":"ci"}],
                "comments":[{"author":{"login":"bob"},"body":"hi\nthere",
                             "createdAt":"2026-09-20T10:00:00Z"}]}"###,
        )
        .unwrap();
        let lines = render_overview(&detail, 90);
        let all = joined(&lines);
        let pos = |needle: &str| all.find(needle).unwrap_or_else(|| panic!("{needle}: {all}"));
        assert!(pos("#9") < pos("Author") && pos("Author") < pos("DESCRIPTION"));
        assert!(pos("DESCRIPTION") < pos("What") && pos("What") < pos("CONVERSATION  (1)"));
        assert!(!all.contains("CHECKS"), "all-passing checks stay in the hero: {all}");
        assert!(all.contains("✓ 1 check passed"), "{all}");
        assert!(all.contains("@dan (pending)"), "{all}");
        assert!(all.contains(" bug "), "{all}");
        assert!(all.contains("Updated"), "{all}");
        // Rails: the hero rows only. Description rows have none; the comment
        // body rows carry the thin `▎`.
        let rows: Vec<String> = all.lines().map(str::to_string).collect();
        let desc = rows.iter().position(|r| r.contains("DESCRIPTION")).unwrap();
        let conv = rows.iter().position(|r| r.contains("CONVERSATION")).unwrap();
        assert!(rows[..desc].iter().any(|r| r.starts_with('▍')), "{all}");
        assert!(
            rows[desc..conv].iter().all(|r| !r.contains('▍') && !r.contains('▎')),
            "description rows carry no rail: {all}"
        );
        assert!(rows[conv..].iter().any(|r| r.starts_with("  ▎ hi")), "{all}");
        assert!(rows[conv..].iter().all(|r| !r.contains('▍')), "{all}");
        // A section head is blank / TITLE / rule / blank; a markdown heading
        // has exactly one blank above and none below.
        assert_eq!(rows[desc - 1], "");
        assert!(rows[desc + 1].starts_with("  ──"), "{}", rows[desc + 1]);
        assert_eq!(rows[desc + 2], "");
        assert_eq!(rows[desc + 3], "  What");
        assert_eq!(rows[desc + 4], "  first para");
        assert_eq!(rows[desc + 5], "");
        assert_eq!(rows[desc + 6], "  Why");
        assert_eq!(rows[desc + 7], "  second para");
        // Label pills pick their text color from the fill.
        let spans: Vec<&Span<'static>> = lines.iter().flat_map(|l| l.spans.iter()).collect();
        let bug = spans.iter().find(|s| s.content == " bug ").unwrap();
        assert_eq!(bug.style.bg, Some(Color::Rgb(0xd7, 0x3a, 0x4a)));
        assert_eq!(bug.style.fg, Some(Color::Rgb(240, 240, 240)));
        let ui = spans.iter().find(|s| s.content == " ui ").unwrap();
        assert_eq!(ui.style.fg, Some(Color::Black));
    }

    #[test]
    fn meta_grid_collapses_to_one_column_in_narrow_panes() {
        let detail: Value = serde_json::from_str(
            r#"{"number":9,"title":"t","author":{"login":"me"},"state":"OPEN",
                "headRefName":"feat/x","baseRefName":"main","updatedAt":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let wide = joined(&render_overview(&detail, 90));
        let author = wide.lines().find(|r| r.contains("Author")).unwrap();
        assert!(author.contains("Branch"), "two columns share a row: {author}");
        let narrow = joined(&render_overview(&detail, 50));
        let author = narrow.lines().find(|r| r.contains("Author")).unwrap();
        assert!(!author.contains("Branch"), "one column: {author}");
        assert!(narrow.lines().any(|r| r.starts_with("▍ Branch")), "{narrow}");
        assert!(narrow.lines().any(|r| r.starts_with("▍ Updated")), "{narrow}");
    }

    #[test]
    fn relative_time_counts_back_from_now() {
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(parse_utc("2026-09-22T12:00:00Z").unwrap() as u64);
        assert_eq!(relative_time("2026-09-22T11:59:30Z", now), "just now");
        assert_eq!(relative_time("2026-09-22T11:15:00Z", now), "45 minutes ago");
        assert_eq!(relative_time("2026-09-22T11:00:00Z", now), "1 hour ago");
        assert_eq!(relative_time("2026-09-20T12:00:00Z", now), "2 days ago");
        assert_eq!(relative_time("2026-07-01T12:00:00Z", now), "2 months ago");
        assert_eq!(relative_time("2024-09-22T12:00:00Z", now), "2 years ago");
        assert_eq!(relative_time("2027-01-01T00:00:00Z", now), "just now", "the future clamps");
        assert_eq!(relative_time("yesterday", now), "yesterday", "unparseable stays raw");
        assert_eq!(parse_utc("1970-01-02T00:00:00Z"), Some(86_400));
    }

    #[test]
    fn merge_state_reads_githubs_verdict() {
        let state = |json: &str| {
            let detail: Value = serde_json::from_str(json).unwrap();
            merge_state(&detail)
        };
        assert_eq!(state(r#"{"state":"OPEN"}"#), Mergeability::Ready);
        assert_eq!(
            state(r#"{"state":"OPEN","mergeStateStatus":"BLOCKED"}"#),
            Mergeability::Ready,
            "failing checks still offer the button — gh reports why"
        );
        assert_eq!(
            state(r#"{"state":"OPEN","mergeStateStatus":"DIRTY"}"#),
            Mergeability::Conflicts
        );
        assert_eq!(state(r#"{"state":"MERGED"}"#), Mergeability::Merged);
        assert_eq!(state(r#"{"state":"CLOSED"}"#), Mergeability::Closed);
    }
}

