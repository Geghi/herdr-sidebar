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

/// The overview pane's lines: a styled header, the checks, then the
/// description and the conversation, each rendered as markdown.
pub fn render_overview(detail: &Value, width: usize) -> Vec<Line<'static>> {
    let text = |key: &str| detail.get(key).and_then(Value::as_str).unwrap_or("");
    let author = detail
        .get("author")
        .and_then(|author| author.get("login"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let number = detail.get("number").and_then(Value::as_u64).unwrap_or(0);
    let draft = detail
        .get("isDraft")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut out: Vec<Line<'static>> = Vec::new();
    // Title: the number reads as a badge, the title carries the weight.
    let mut title = vec![Span::styled(
        format!("#{number}  "),
        Style::default().fg(palette().header_accent),
    )];
    title.push(Span::styled(
        text("title").to_string(),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    if draft {
        title.push(Span::styled(
            "  draft",
            Style::default().fg(palette().modified),
        ));
    }
    out.push(Line::from(title));
    // Branches, size, author.
    out.push(Line::from(vec![
        Span::styled(format!("@{author}"), Style::default().fg(palette().accent)),
        Span::styled(
            format!(
                "  {} → {}  ·  {}  ·  +{} −{}  ·  {} files",
                text("headRefName"),
                text("baseRefName"),
                text("state").to_lowercase(),
                detail.get("additions").and_then(Value::as_u64).unwrap_or(0),
                detail.get("deletions").and_then(Value::as_u64).unwrap_or(0),
                detail
                    .get("changedFiles")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            ),
            Style::default().dim(),
        ),
    ]));
    if let Some((glyph, label, color)) =
        review_line(detail.get("reviewDecision").and_then(Value::as_str))
    {
        out.push(Line::from(vec![
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(label, Style::default().fg(color)),
        ]));
    }
    if let Some(checks) = checks_summary(detail.get("statusCheckRollup")) {
        out.push(checks);
    }
    out.push(Line::from(Span::styled(
        text("url").to_string(),
        Style::default().dim(),
    )));
    out.push(rule(width));

    let body = text("body").trim();
    if !body.is_empty() {
        out.extend(crate::markdown::render(body, width));
        out.push(rule(width));
    }
    if let Some(conversation) = conversation(detail.get("comments"), detail.get("reviews"), width) {
        out.push(Line::from(Span::styled(
            "Conversation",
            Style::default()
                .fg(palette().header_accent)
                .add_modifier(Modifier::BOLD),
        )));
        out.push(Line::default());
        out.extend(conversation);
    }
    out
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
        ReviewState::Pending => None,
    }
}

/// `✓ 3 passed · ✗ 1 failed · ○ 2 pending` as a styled line.
fn checks_summary(rollup: Option<&Value>) -> Option<Line<'static>> {
    let items = rollup?.as_array()?;
    if items.is_empty() {
        return None;
    }
    let count = |want: &[&str]| {
        items
            .iter()
            .filter(|item| {
                let state = item
                    .get("conclusion")
                    .or_else(|| item.get("state"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_uppercase();
                want.iter().any(|w| state.contains(w))
            })
            .count()
    };
    let passed = count(&["SUCCESS", "NEUTRAL"]);
    let failed = count(&["FAILURE", "ERROR", "CANCELLED", "TIMED_OUT"]);
    let pending = items.len() - passed - failed;
    let mut spans = vec![Span::styled("checks  ", Style::default().dim())];
    let mut push = |glyph: &str, count: usize, label: &str, color: ratatui::style::Color| {
        if count > 0 {
            spans.push(Span::styled(
                format!("{glyph} {count} {label}"),
                Style::default().fg(color),
            ));
            spans.push(Span::styled("   ", Style::default().dim()));
        }
    };
    push("✓", passed, "passed", palette().untracked);
    push("✗", failed, "failed", palette().deleted);
    push("○", pending, "pending", palette().modified);
    while matches!(spans.last(), Some(span) if span.content == "   ") {
        spans.pop();
    }
    Some(Line::from(spans))
}

/// Comments and reviews, oldest first, as markdown under a styled byline.
fn conversation(
    comments: Option<&Value>,
    reviews: Option<&Value>,
    width: usize,
) -> Option<Vec<Line<'static>>> {
    let mut entries: Vec<(String, String, String)> = Vec::new();
    for item in comments.and_then(Value::as_array).into_iter().flatten() {
        let author = item
            .get("author")
            .and_then(|author| author.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let body = item
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if !body.is_empty() {
            entries.push((
                item.get("createdAt")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                format!("@{author} commented"),
                body.to_string(),
            ));
        }
    }
    for item in reviews.and_then(Value::as_array).into_iter().flatten() {
        let author = item
            .get("author")
            .and_then(|author| author.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let state = item.get("state").and_then(Value::as_str).unwrap_or("");
        let body = item
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let head = match state {
            "APPROVED" => format!("@{author} approved"),
            "CHANGES_REQUESTED" => format!("@{author} requested changes"),
            "DISMISSED" => format!("@{author}'s review was dismissed"),
            _ => format!("@{author} reviewed"),
        };
        if !body.is_empty() || state != "COMMENTED" {
            entries.push((
                item.get("submittedAt")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                head,
                body.to_string(),
            ));
        }
    }
    if entries.is_empty() {
        return None;
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    for (stamp, head, body) in entries {
        let day = stamp.split('T').next().unwrap_or(&stamp);
        out.push(Line::from(vec![
            Span::styled(head, Style::default().fg(palette().accent)),
            Span::styled(format!("  ·  {day}"), Style::default().dim()),
        ]));
        if !body.is_empty() {
            out.extend(crate::markdown::render(&body, width));
        }
        out.push(Line::default());
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
                "body":"Adds full-resolution previews.","url":"https://github.com/o/r/pull/74",
                "additions":10,"deletions":2,"changedFiles":3,"reviewDecision":"APPROVED",
                "statusCheckRollup":[{"conclusion":"SUCCESS"},{"conclusion":"SUCCESS"},
                                     {"conclusion":"FAILURE"},{"state":"PENDING"}],
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
        assert!(all.contains("@Facu"), "{all}");
        assert!(all.contains("feat/x → main"), "{all}");
        assert!(all.contains("+10 −2"), "{all}");
        assert!(all.contains("3 files"), "{all}");
        assert!(all.contains("✓ approved"), "{all}");
        assert!(all.contains("✓ 2 passed"), "{all}");
        assert!(all.contains("✗ 1 failed"), "{all}");
        assert!(all.contains("○ 1 pending"), "{all}");
        assert!(all.contains("Adds full-resolution previews."), "{all}");
        assert!(!all.contains("**"), "markdown markers are gone: {all}");
        assert!(all.contains("Conversation"), "{all}");
        let changes = all.find("requested changes").unwrap();
        let nit = all.find("nit: rename this").unwrap();
        let approved = all.find("approved  ·  2026-09-21").unwrap();
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
            !all.contains("checks"),
            "no rollup adds no checks line: {all}"
        );
        assert!(!all.contains("approved"), "{all}");
        assert!(all.contains("#1  t"), "{all}");
        assert!(all.contains("open"), "{all}");
    }
}
