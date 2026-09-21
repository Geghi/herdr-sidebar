//! GitHub pull-request plumbing: the `gh` CLI plus pure parsers. `gh` talks to
//! the network and has no timeout of its own, so every command here is a plain
//! synchronous call that the caller MUST run on a worker thread (see
//! `pr_app`), never on the pane's event loop.

use std::path::Path;
use std::process::Command;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::git::FileEntry;
use crate::ui::palette;

/// The `gh pr list --json` field list the rows are built from.
const LIST_FIELDS: &str = "number,title,author,headRefName,baseRefName,isDraft,updatedAt,url,additions,deletions,changedFiles,reviewDecision";

/// The `gh pr view --json` field list the overview is built from.
const OVERVIEW_FIELDS: &str = "number,title,author,state,isDraft,baseRefName,headRefName,body,url,additions,deletions,changedFiles,reviewDecision,comments,reviews,statusCheckRollup,mergeable,mergeStateStatus";

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

/// The overview pane's lines: a structured app-native summary — the header
/// with a state badge, the metadata card, the checks as grouped rows, the
/// description as clean prose, and the conversation as headed blocks. Never
/// raw markdown: no `#`, `**` or `[..](..)` syntax reaches the pane.
pub fn render_overview(detail: &Value, width: usize) -> Vec<Line<'static>> {
    let text = |key: &str| detail.get(key).and_then(Value::as_str).unwrap_or("");
    let author = detail
        .get("author")
        .and_then(|author| author.get("login"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let number = detail.get("number").and_then(Value::as_u64).unwrap_or(0);

    let mut out: Vec<Line<'static>> = Vec::new();
    // Title: the number and the state badge carry the color, the title the
    // weight.
    let (state, state_color) = state_badge(detail);
    out.push(Line::from(vec![
        Span::styled(
            format!("#{number}  "),
            Style::default().fg(palette().header_accent),
        ),
        Span::styled(
            text("title").to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("   {state}"),
            Style::default()
                .fg(state_color)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    // The metadata card: who, where, how big, and the review decision.
    out.push(Line::from(vec![Span::styled(
        format!("@{author}"),
        Style::default().fg(palette().accent),
    )]));
    out.push(Line::from(vec![Span::styled(
        format!(
            "{} → {}  ·  +{} −{}  ·  {} files",
            text("headRefName"),
            text("baseRefName"),
            detail.get("additions").and_then(Value::as_u64).unwrap_or(0),
            detail.get("deletions").and_then(Value::as_u64).unwrap_or(0),
            detail
                .get("changedFiles")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ),
        Style::default().dim(),
    )]));
    if let Some((glyph, label, color)) =
        review_line(detail.get("reviewDecision").and_then(Value::as_str))
    {
        out.push(Line::from(vec![
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(label, Style::default().fg(color)),
        ]));
    }
    out.push(Line::from(Span::styled(
        text("url").to_string(),
        Style::default().dim(),
    )));
    if let Some(checks) = checks_summary(detail.get("statusCheckRollup")) {
        out.push(Line::default());
        out.extend(checks);
    }

    let body = text("body").trim();
    if !body.is_empty() {
        out.push(Line::default());
        out.push(section_heading("Description"));
        out.push(Line::default());
        out.extend(crate::markdown::render(body, width));
    }
    if let Some(blocks) = conversation(detail.get("comments"), detail.get("reviews"), width) {
        out.push(Line::default());
        out.push(section_heading("Conversation"));
        out.push(Line::default());
        out.extend(blocks);
    }
    out
}

/// A section heading: bold in the header accent, like the pane titles.
fn section_heading(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        title.to_string(),
        Style::default()
            .fg(palette().header_accent)
            .add_modifier(Modifier::BOLD),
    ))
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

/// A full-width dim rule.
fn rule(width: usize) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(width.clamp(8, 80)),
        Style::default().dim(),
    ))
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

/// The Checks section: one row per non-empty outcome group, the failing and
/// pending checks named under theirs (capped). Passing checks are counted,
/// not listed — a green wall of fifty names helps no one.
fn checks_summary(rollup: Option<&Value>) -> Option<Vec<Line<'static>>> {
    let checks = parse_checks(rollup);
    if checks.is_empty() {
        return None;
    }
    /// Named rows per group before the "+N more" tail.
    const NAMED: usize = 6;
    let mut out = vec![section_heading("Checks")];
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
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{glyph} {} {label}", group.len()),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ]));
        if status == CheckStatus::Passed {
            continue;
        }
        for check in group.iter().take(NAMED) {
            let name = if check.name.is_empty() {
                "(unnamed)".to_string()
            } else {
                check.name.clone()
            };
            out.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(name, Style::default().fg(color)),
            ]));
        }
        if group.len() > NAMED {
            out.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    format!("…and {} more", group.len() - NAMED),
                    Style::default().dim(),
                ),
            ]));
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

/// Comments and reviews, oldest first, as headed blocks: an
/// `@author · state · time` line, the body indented under it, a dim rule
/// between entries. Review states carry their badge.
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
    let mut out = Vec::new();
    for (i, remark) in entries.iter().enumerate() {
        if i > 0 {
            out.push(rule(width));
        }
        let day = remark.stamp.split('T').next().unwrap_or(&remark.stamp);
        let mut head = vec![
            Span::styled(
                format!("@{}", remark.author),
                Style::default().fg(palette().accent),
            ),
            Span::styled(" · ", Style::default().dim()),
        ];
        match remark.badge {
            Some((glyph, label, color)) => {
                head.push(Span::styled(
                    format!("{glyph} {label}"),
                    Style::default().fg(color),
                ));
            }
            None => head.push(Span::styled("commented", Style::default().dim())),
        }
        head.push(Span::styled(
            format!(" · {day}"),
            Style::default().dim(),
        ));
        out.push(Line::from(head));
        if !remark.body.is_empty() {
            out.push(Line::default());
            for line in crate::markdown::render(&remark.body, width) {
                if line.spans.is_empty() {
                    out.push(Line::default());
                } else {
                    let mut spans = vec![Span::raw("  ")];
                    spans.extend(line.spans);
                    out.push(Line::from(spans));
                }
            }
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
        assert!(all.contains("#74  feat: previews"), "{all}");
        assert!(all.contains("Open"), "the state badge: {all}");
        assert!(all.contains("@Facu"), "{all}");
        assert!(all.contains("feat/x → main"), "{all}");
        assert!(all.contains("+10 −2"), "{all}");
        assert!(all.contains("3 files"), "{all}");
        assert!(all.contains("✓ approved"), "{all}");
        assert!(all.contains("Checks"), "{all}");
        assert!(all.contains("✗ 1 failed"), "{all}");
        assert!(all.contains("build"), "failing checks are named: {all}");
        assert!(all.contains("● 1 pending"), "{all}");
        assert!(all.contains("lint"), "{all}");
        assert!(all.contains("✓ 2 passed"), "{all}");
        assert!(
            all.find("✗ 1 failed").unwrap() < all.find("● 1 pending").unwrap(),
            "failing checks come first: {all}"
        );
        assert!(all.contains("Description"), "{all}");
        assert!(all.contains("Adds full-resolution previews."), "{all}");
        assert!(all.contains("docs"), "{all}");
        assert!(
            !all.contains("[docs](https://example.com/x)"),
            "link syntax never reaches the pane: {all}"
        );
        assert!(!all.contains("**"), "markdown markers are gone: {all}");
        assert!(all.contains("Conversation"), "{all}");
        assert!(all.contains("@ann · ✗ requested changes · 2026-09-19"), "{all}");
        assert!(all.contains("@bob · commented · 2026-09-20"), "{all}");
        assert!(all.contains("@cid · ✓ approved · 2026-09-21"), "{all}");
        assert!(all.contains('─'), "entries are rule-separated: {all}");
        let changes = all.find("requested changes").unwrap();
        let nit = all.find("nit: rename this").unwrap();
        let approved = all.find("· ✓ approved · 2026-09-21").unwrap();
        assert!(changes < nit, "oldest first");
        assert!(nit < approved);
        // The title is the one bold span, the number carries the accent.
        let title = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content == "#74  ")
            .unwrap();
        assert_eq!(title.style.fg, Some(palette().header_accent));
    }

    #[test]
    fn overview_without_body_or_conversation_stays_minimal() {
        let detail: Value = serde_json::from_str(
            r#"{"number":1,"title":"t","author":{"login":"me"},"state":"OPEN",
                "headRefName":"a","baseRefName":"b","body":"   ","url":"u"}"#,
        )
        .unwrap();
        let all = joined(&render_overview(&detail, 60));
        assert!(!all.contains("Conversation"), "{all}");
        assert!(
            !all.contains("Checks"),
            "no rollup adds no checks section: {all}"
        );
        assert!(!all.contains("approved"), "{all}");
        assert!(!all.contains("Description"), "{all}");
        assert!(all.contains("#1  t"), "{all}");
        assert!(all.contains("Open"), "the state badge is always there: {all}");
    }

    #[test]
    fn state_badges_cover_draft_merged_and_closed() {
        let badge = |json: &str| {
            let detail: Value = serde_json::from_str(json).unwrap();
            let head = joined(&render_overview(&detail, 60));
            head.lines().next().unwrap_or_default().to_string()
        };
        let draft = badge(r#"{"number":1,"title":"t","isDraft":true,"state":"OPEN"}"#);
        assert!(draft.contains("Draft"), "{draft}");
        let merged = badge(r#"{"number":1,"title":"t","state":"MERGED"}"#);
        assert!(merged.contains("Merged"), "{merged}");
        let closed = badge(r#"{"number":1,"title":"t","state":"CLOSED"}"#);
        assert!(closed.contains("Closed"), "{closed}");
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
        let all = joined(&checks_summary(Some(&rollup)).unwrap());
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
        assert!(checks_summary(None).is_none());
        assert!(checks_summary(Some(&Value::Array(vec![]))).is_none());
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
        assert!(all.contains("  ☐ todo"), "bodies are indented: {all}");
        assert!(!all.contains("- [ ]"), "task syntax is gone: {all}");
        assert!(!all.contains('`'), "code ticks are gone: {all}");
        assert!(all.contains('─'), "entries are rule-separated: {all}");
        assert!(conversation(None, None, 60).is_none());
    }
}
