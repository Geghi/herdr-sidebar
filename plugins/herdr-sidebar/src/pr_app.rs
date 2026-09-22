//! The Pull Requests view: open pull requests in collapsible drawers, the
//! files a pull request touches expanded INLINE under it, and the overview /
//! per-file diff in the preview pane beside the sidebar.
//!
//! `gh` is a network CLI with no timeout of its own, so every fetch runs on a
//! worker thread and lands here through a channel polled from `tick()`. The
//! pane therefore never blocks on the network: a slow request only ever shows
//! as a list that is still empty.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph};

use crate::scm_app::{ChangeTreeRow, TreeNode, changes_tree_rows, folder_item};
use herdr_sidebar::actions::copy_to_clipboard;
use herdr_sidebar::git::FileEntry;
use herdr_sidebar::icons::{IconTheme, icon};
use herdr_sidebar::pr::{
    self, MergeMethod, PrFile, PrFilter, PullRequest, ReviewState, Thread, Verdict,
};
use herdr_sidebar::state::{self as sidebar, Exit, View};
use herdr_sidebar::ui::{
    activity_button_style, activity_icons, draw_activity_caps, draw_scrollbar, hits,
    hits_activity_button, hover_style, palette, selection_style, set_color_theme,
    wrap_footer_message, wrap_hints,
};

const MY_VIEW: View = View::PullRequests;
/// How many pull requests a drawer asks for.
const PAGE: usize = 30;
/// A background refresh this long after the last one — `gh` hits the network,
/// so the view does not poll it on every tick.
const REFRESH_EVERY: Duration = Duration::from_secs(120);
/// The heartbeat interval (the launcher kills token-less panes).
const BEAT_EVERY: Duration = Duration::from_secs(5);

/// List rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Row {
    /// A drawer header, by index into [`PrFilter::ALL`].
    Header(usize),
    /// A pull request: (drawer index, index into that drawer's page).
    Pr(usize, usize),
    /// A changed file of the expanded pull request.
    File(usize),
    /// A folder row of that file list in tree view (index into
    /// [`App::tree_nodes`]).
    FileFolder(usize),
}

/// One drawer: whether it is open, its page, and the error that replaced it.
#[derive(Default)]
struct DrawerPanel {
    expanded: bool,
    prs: Vec<PullRequest>,
    error: Option<String>,
}

/// A page arriving from the worker.
struct Page {
    filter: usize,
    result: Result<Vec<PullRequest>, String>,
}

/// A pull request's files arriving from the worker.
struct Files {
    number: u64,
    result: Result<Vec<PrFile>, String>,
}

/// What a pull-request context-menu entry does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MenuAction {
    OpenOverview,
    OpenInBrowser,
    Checkout,
    Merge(MergeMethod),
    Ready,
    Review(Verdict),
    Comment,
    ResolveThreads,
    CopyUrl,
    CopyBranch,
}

impl MenuAction {
    /// Whether running it changes the repository or the pull request, and so
    /// goes through the y/N prompt first.
    fn needs_confirm(self) -> bool {
        matches!(self, Self::Checkout | Self::Merge(_))
    }

    /// Whether it asks for a body first. An approval does not need one;
    /// a comment or a change request does.
    fn needs_body(self) -> bool {
        matches!(
            self,
            Self::Comment | Self::Review(Verdict::RequestChanges | Verdict::Comment)
        )
    }

    /// The prompt shown while it runs.
    fn progress(self, number: u64) -> String {
        match self {
            Self::Checkout => format!("checking out PR #{number}…"),
            Self::Merge(method) => format!("{} PR #{number}…", method.label(),),
            Self::Ready => format!("marking PR #{number} ready…"),
            Self::Review(Verdict::Approve) => format!("approving PR #{number}…"),
            Self::Review(Verdict::RequestChanges) => format!("requesting changes on PR #{number}…"),
            Self::Review(Verdict::Comment) => format!("reviewing PR #{number}…"),
            Self::Comment => format!("commenting on PR #{number}…"),
            Self::ResolveThreads => format!("loading conversations of PR #{number}…"),
            _ => format!("PR #{number}…"),
        }
    }

    /// The confirmation question for a destructive action.
    fn confirm_prompt(self, number: u64) -> String {
        match self {
            Self::Checkout => format!("Check out the branch of PR #{number}? (y/N)"),
            Self::Merge(method) => {
                format!("{} on PR #{number}? (y/N)", method.label())
            }
            _ => format!("Run this on PR #{number}? (y/N)"),
        }
    }
}

/// The context menu for one pull request, which depends on its state: a draft
/// is offered "ready for review" instead of the merge options.
fn menu_entries(pr: &PullRequest) -> Vec<MenuEntry> {
    let mut entries = vec![
        MenuEntry::Action(MenuAction::OpenOverview, "Open Pull Request"),
        MenuEntry::Action(MenuAction::OpenInBrowser, "Open in Browser"),
        MenuEntry::Separator,
        MenuEntry::Action(MenuAction::Checkout, "Checkout Branch"),
    ];
    if pr.draft {
        entries.push(MenuEntry::Action(
            MenuAction::Ready,
            "Mark Ready for Review",
        ));
    } else {
        for method in MergeMethod::ALL {
            entries.push(MenuEntry::Action(MenuAction::Merge(method), method.label()));
        }
    }
    entries.extend([
        MenuEntry::Separator,
        MenuEntry::Action(MenuAction::Review(Verdict::Approve), "Approve"),
        MenuEntry::Action(
            MenuAction::Review(Verdict::RequestChanges),
            "Request Changes…",
        ),
        MenuEntry::Action(MenuAction::Comment, "Comment…"),
        MenuEntry::Separator,
        MenuEntry::Action(MenuAction::ResolveThreads, "Resolve Conversations…"),
        MenuEntry::Separator,
        MenuEntry::Action(MenuAction::CopyUrl, "Copy URL"),
        MenuEntry::Action(MenuAction::CopyBranch, "Copy Branch Name"),
    ]);
    entries
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MenuEntry {
    Action(MenuAction, &'static str),
    Separator,
}

/// The menu entry under a click, if the click lands on one. Entries render
/// one row each under the popup's top border, so the row maps straight back
/// to the entry index.
fn menu_hit(entries: &[MenuEntry], rect: Rect, x: u16, y: u16) -> Option<usize> {
    if x < rect.x || x >= rect.x.saturating_add(rect.width) {
        return None;
    }
    let row = y.saturating_sub(rect.y.saturating_add(1)) as usize;
    (row < entries.len()).then_some(row)
}

/// A modal layered over the list.
enum Overlay {
    Menu {
        number: u64,
        entries: Vec<MenuEntry>,
        selected: usize,
        rect: Rect,
    },
    /// The y/N prompt in front of a mutating action.
    Confirm { number: u64, action: MenuAction },
    /// A one-line body for a comment or a review.
    Input {
        number: u64,
        action: MenuAction,
        text: Vec<char>,
        cursor: usize,
    },
    /// The pull request's unresolved conversations.
    Threads {
        number: u64,
        threads: Vec<Thread>,
        selected: usize,
    },
}

/// A background operation with one result.
enum Job {
    /// A `gh` command that mutates something.
    Command(Result<String, String>),
    /// A pull request's review conversations.
    Threads(Result<Vec<Thread>, String>),
}

/// What a finished job was for.
#[derive(Clone, PartialEq, Eq)]
enum JobKind {
    Action,
    Threads(u64),
    Resolve(String),
}

/// Identity/label control of our own pane over the socket API.
struct PaneCtl {
    pane_id: String,
}

impl PaneCtl {
    fn from_env() -> Option<Self> {
        let pane_id = std::env::var("HERDR_PANE_ID")
            .ok()
            .filter(|id| !id.is_empty())?;
        Some(Self { pane_id })
    }

    fn set_label(&self, label: &str) {
        let _ = herdr_sidebar::ipc::call_text(
            "pane.rename",
            serde_json::json!({ "pane_id": self.pane_id, "label": label }),
        );
    }

    fn report_tokens(&self, merged: bool) {
        herdr_sidebar::ipc::report_identity(&self.pane_id, MY_VIEW, merged);
    }
}

pub struct App {
    cwd: PathBuf,
    /// Kept alive for the merged sidebar's cwd following; this view reads the
    /// folder it was opened on and does not move.
    _cwd_follower: Rc<RefCell<herdr_sidebar::launch::CwdFollower>>,
    theme: IconTheme,
    drawers: [DrawerPanel; 3],
    /// The pull request whose files are expanded inline, by number.
    expanded: Option<u64>,
    files: Vec<PrFile>,
    /// The file list as a tree (the same "View as tree" setting the Source
    /// Control view uses): folder rows and the collapse set that drives them.
    tree_nodes: Vec<TreeNode>,
    tree_collapsed: BTreeSet<String>,
    /// Indent depth per row, parallel to `rows` (tree view).
    row_depth: Vec<usize>,
    /// The pull request whose files are being fetched.
    files_loading: Option<u64>,
    rows: Vec<Row>,
    selected: Option<usize>,
    scroll: usize,
    snap: bool,
    hovered: Option<usize>,
    flash: Option<(String, bool)>,
    /// One list fetch in flight: a second refresh would only queue latency.
    fetching: Option<Receiver<Page>>,
    files_rx: Option<Receiver<Files>>,
    /// One mutating command or conversation fetch at a time: the overlays keep
    /// the user from starting a second one.
    job: Option<(JobKind, Receiver<Job>)>,
    last_refresh: Instant,
    overlay: Option<Overlay>,
    last_width: u16,
    last_height: u16,
    last_mouse: Option<Instant>,
    mouse_pos: Option<(u16, u16)>,
    last_beat: Instant,
    pane_ctl: Option<PaneCtl>,
    sidebar_state: sidebar::State,
    /// The list's geometry from the last draw.
    body: Rect,
    /// The activity bar's click zones from the last draw.
    activity_row: u16,
    activity_zones: [(u16, u16); 4],
}

impl App {
    pub fn new(
        cwd: PathBuf,
        cwd_follower: Rc<RefCell<herdr_sidebar::launch::CwdFollower>>,
    ) -> Self {
        let sidebar_state = sidebar::load_state();
        set_color_theme(sidebar_state.color_theme);
        let mut drawers: [DrawerPanel; 3] = Default::default();
        for drawer in &mut drawers {
            drawer.expanded = true;
        }
        let mut app = Self {
            cwd,
            _cwd_follower: cwd_follower,
            theme: IconTheme::resolve(
                std::env::var("HERDR_SIDEBAR_ICONS")
                    .or_else(|_| std::env::var("HERDR_AA_GIT_ICONS"))
                    .or_else(|_| std::env::var("HERDR_AA_FILETREE_ICONS"))
                    .ok()
                    .as_deref(),
                sidebar_state.icons,
            ),
            drawers,
            expanded: None,
            files: Vec::new(),
            tree_nodes: Vec::new(),
            tree_collapsed: BTreeSet::new(),
            row_depth: Vec::new(),
            files_loading: None,
            rows: Vec::new(),
            selected: None,
            scroll: 0,
            snap: false,
            hovered: None,
            flash: None,
            fetching: None,
            files_rx: None,
            job: None,
            last_refresh: Instant::now() - REFRESH_EVERY,
            overlay: None,
            last_width: 0,
            last_height: 0,
            last_mouse: None,
            mouse_pos: None,
            last_beat: Instant::now(),
            pane_ctl: PaneCtl::from_env(),
            sidebar_state,
            body: Rect::default(),
            activity_row: 0,
            activity_zones: [(0, 0); 4],
        };
        app.apply_identity();
        app.refresh();
        app
    }

    pub fn root_path(&self) -> &Path {
        &self.cwd
    }

    fn merged(&self) -> bool {
        self.sidebar_state.merged
    }

    /// Push our label + metadata tokens to herdr.
    fn apply_identity(&self) {
        let Some(ctl) = &self.pane_ctl else { return };
        let label = if self.merged() {
            sidebar::SIDEBAR_LABEL
        } else {
            MY_VIEW.label()
        };
        ctl.set_label(label);
        ctl.report_tokens(self.merged());
    }

    pub fn clear_identity(&self) {
        if let Some(ctl) = &self.pane_ctl {
            herdr_sidebar::ipc::clear_identity(&ctl.pane_id);
        }
    }

    /// Hide the sidebar: snooze this tab and close our own pane.
    fn hide(&mut self) {
        self.close(true);
    }

    /// Close our own pane. Ctrl+Q from a toggle launcher closes without
    /// snoozing (the toggle's own decision already knows whether to re-dock);
    /// `q`/`b` behave like the other views' hide instead.
    fn close(&mut self, snooze: bool) {
        let Some(ctl) = &self.pane_ctl else { return };
        if snooze
            && let Ok(json) = herdr_sidebar::ipc::call_text("pane.list", serde_json::json!({}))
        {
            let tab = herdr_sidebar::launch::tab_of(&json, &ctl.pane_id);
            herdr_sidebar::snooze::set(&herdr_sidebar::snooze::dir(), &tab);
        }
        let _ = herdr_sidebar::ipc::call_text(
            "pane.close",
            serde_json::json!({ "pane_id": ctl.pane_id }),
        );
    }

    /// The identity heartbeat: a token-less pane is killed as a corpse.
    pub fn heartbeat(&mut self) {
        if self.last_beat.elapsed() < BEAT_EVERY {
            return;
        }
        self.last_beat = Instant::now();
        if let Some(ctl) = &self.pane_ctl {
            ctl.report_tokens(self.merged());
        }
    }

    pub fn on_resize(&mut self, width: u16) {
        self.last_width = width;
    }

    /// Kick a background fetch of every drawer.
    fn refresh(&mut self) {
        if self.fetching.is_some() {
            return;
        }
        self.last_refresh = Instant::now();
        let root = self.cwd.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for (index, filter) in PrFilter::ALL.iter().enumerate() {
                let result = pr::list(&root, *filter, PAGE);
                if sender
                    .send(Page {
                        filter: index,
                        result,
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        self.fetching = Some(receiver);
    }

    /// Whether the file list renders as a tree (shared with the Source
    /// Control view's "View as tree" setting).
    fn tree(&self) -> bool {
        self.sidebar_state.scm_tree
    }

    /// `t`: flip the tree/flat rendering of a pull request's files.
    fn toggle_tree_view(&mut self) {
        self.sidebar_state = sidebar::update_state(|state| state.scm_tree = !state.scm_tree);
        self.rebuild();
    }

    /// Fold/unfold a file-tree folder, keeping the selection on its row.
    /// Collapse keys are scoped to the expanded pull request (`N:path`), so
    /// two requests touching the same directory do not fold each other.
    fn toggle_tree_folder(&mut self, node: usize) {
        let Some(folder) = self.tree_nodes.get(node) else {
            return;
        };
        let path = folder.path.clone();
        let number = self.expanded.unwrap_or(0);
        let key = format!("{number}:{path}");
        if !self.tree_collapsed.remove(&key) {
            self.tree_collapsed.insert(key);
        }
        self.rebuild();
        // The fold hides whatever file was selected under it, so the plain
        // stable-id re-find would clamp to a neighbour — re-find the folder
        // itself instead, like the SCM tree.
        if let Some(index) = self.find_row_by_stable_id(&format!("folder:{number}:{path}")) {
            self.select(index);
        }
    }

    /// Start (or stop) the inline file list of one pull request.
    fn toggle_files(&mut self, number: u64) {
        if self.expanded == Some(number) {
            self.expanded = None;
            self.files.clear();
            self.files_loading = None;
            self.files_rx = None;
            self.rebuild();
            return;
        }
        self.expanded = Some(number);
        self.files.clear();
        self.files_loading = Some(number);
        self.flash = Some((format!("loading files of PR #{number}…"), false));
        let root = self.cwd.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = pr::files(&root, number);
            let _ = sender.send(Files { number, result });
        });
        self.files_rx = Some(receiver);
        self.rebuild();
        self.snap = true;
    }

    /// Collect whatever the workers finished.
    fn poll(&mut self) {
        if let Some((kind, receiver)) = self.job.take() {
            match receiver.try_recv() {
                Ok(job) => self.apply_job(kind, job),
                Err(TryRecvError::Empty) => self.job = Some((kind, receiver)),
                Err(TryRecvError::Disconnected) => {
                    self.flash = Some(("gh exited without a result".into(), true));
                }
            }
        }
        if let Some(receiver) = self.fetching.take() {
            let mut pages = Vec::new();
            let mut done = false;
            loop {
                match receiver.try_recv() {
                    Ok(page) => pages.push(page),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        done = true;
                        break;
                    }
                }
            }
            for page in pages {
                let panel = &mut self.drawers[page.filter];
                match page.result {
                    Ok(prs) => {
                        panel.prs = prs;
                        panel.error = None;
                    }
                    Err(e) => panel.error = Some(e),
                }
            }
            if !done {
                self.fetching = Some(receiver);
            }
            self.rebuild();
        }
        if let Some(receiver) = self.files_rx.take() {
            match receiver.try_recv() {
                Ok(files) => {
                    self.files_rx = None;
                    self.files_loading = None;
                    self.flash = None;
                    if self.expanded == Some(files.number) {
                        match files.result {
                            Ok(list) => self.files = list,
                            Err(e) => self.flash = Some((e, true)),
                        }
                    }
                    self.rebuild();
                }
                Err(TryRecvError::Empty) => self.files_rx = Some(receiver),
                Err(TryRecvError::Disconnected) => {
                    self.files_loading = None;
                }
            }
        }
    }

    /// Poll the workers, refresh on a slow cadence, and re-read the shared
    /// display settings — another pane's `t` (or the ⚙ row) flips the tree
    /// view for every pane, separated ones included.
    pub fn tick(&mut self) {
        self.poll();
        let shared = sidebar::load_state();
        if shared.scm_tree != self.sidebar_state.scm_tree {
            self.sidebar_state.scm_tree = shared.scm_tree;
            self.rebuild();
        }
        if shared.color_theme != self.sidebar_state.color_theme {
            self.sidebar_state.color_theme = shared.color_theme;
            set_color_theme(shared.color_theme);
        }
        if shared.show_hotkeys != self.sidebar_state.show_hotkeys {
            self.sidebar_state.show_hotkeys = shared.show_hotkeys;
        }
        if self.fetching.is_none() && self.last_refresh.elapsed() >= REFRESH_EVERY {
            self.refresh();
        }
    }

    /// Rebuild the row list from the drawers. The selection follows the
    /// row's stable id — folding a folder, toggling the tree, or finishing
    /// a network refresh keeps it on the same logical row, not the same
    /// index.
    fn rebuild(&mut self) {
        let keep = self.selected.and_then(|i| self.row_stable_id(i));
        self.tree_nodes.clear();
        let tree = self.tree();
        let (rows, depths) = build_rows(
            &self.drawers,
            self.expanded,
            &self.files,
            tree,
            &self.tree_collapsed,
            &mut self.tree_nodes,
        );
        self.rows = rows;
        self.row_depth = depths;
        if self.rows.is_empty() {
            self.selected = None;
            self.scroll = 0;
            self.hovered = None;
            return;
        }
        let found = keep
            .as_deref()
            .and_then(|id| self.find_row_by_stable_id(id));
        if found != self.selected {
            self.snap = true;
        }
        self.selected = found
            .or_else(|| self.selected.map(|s| s.min(self.rows.len() - 1)))
            .or(Some(0));
    }

    /// A row's stable id — drawer, pull request number, file path, or
    /// tree-folder path — so the selection survives a rebuild.
    fn row_stable_id(&self, index: usize) -> Option<String> {
        match self.rows.get(index)? {
            Row::Header(drawer) => Some(format!("header:{drawer}")),
            Row::Pr(drawer, i) => self.drawers[*drawer]
                .prs
                .get(*i)
                .map(|pr| format!("pr:{}", pr.number)),
            Row::File(i) => self
                .files
                .get(*i)
                .map(|file| format!("file:{}", file.entry.path)),
            Row::FileFolder(node) => self
                .tree_nodes
                .get(*node)
                .map(|folder| format!("folder:{}:{}", self.expanded.unwrap_or(0), folder.path)),
        }
    }

    /// Inverse of [`row_stable_id`]: the row whose id matches, if any.
    fn find_row_by_stable_id(&self, id: &str) -> Option<usize> {
        (0..self.rows.len()).find(|&i| self.row_stable_id(i).as_deref() == Some(id))
    }

    /// The nearest row above `index` with a shallower depth — a file's (or a
    /// folded folder's) parent row, like the Source Control tree.
    fn parent_row(&self, index: usize) -> Option<usize> {
        let depth = *self.row_depth.get(index)?;
        (0..index)
            .rev()
            .find(|&i| self.row_depth.get(i).is_some_and(|row| *row < depth))
    }

    fn select(&mut self, index: usize) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = Some(index.min(self.rows.len() - 1));
        self.snap = true;
    }

    fn move_by(&mut self, delta: isize) {
        let Some(current) = self.selected else { return };
        let next = (current as isize + delta).clamp(0, self.rows.len() as isize - 1);
        self.selected = Some(next as usize);
        self.snap = true;
    }

    /// The pull request a row points at, if any.
    fn row_pr(&self, index: usize) -> Option<&PullRequest> {
        match self.rows.get(index)? {
            Row::Pr(drawer, i) => self.drawers[*drawer].prs.get(*i),
            _ => None,
        }
    }

    fn selected_pr_number(&self) -> Option<u64> {
        self.row_pr(self.selected?).map(|pr| pr.number)
    }

    fn activate(&mut self) {
        let Some(&row) = self.selected.and_then(|index| self.rows.get(index)) else {
            return;
        };
        match row {
            Row::Header(drawer) => {
                self.drawers[drawer].expanded = !self.drawers[drawer].expanded;
                self.rebuild();
            }
            Row::Pr(..) => {
                if let Some(number) = self.selected_pr_number() {
                    self.open_overview(number);
                }
            }
            Row::File(index) => self.open_file_diff(index),
            Row::FileFolder(node) => self.toggle_tree_folder(node),
        }
    }

    /// Right/`l`: open the selected row — a pull request's files, or a drawer.
    fn expand(&mut self) {
        let Some(&row) = self.selected.and_then(|index| self.rows.get(index)) else {
            return;
        };
        match row {
            Row::Header(drawer) => {
                if !self.drawers[drawer].expanded {
                    self.drawers[drawer].expanded = true;
                    self.rebuild();
                }
            }
            Row::Pr(..) => {
                if let Some(number) = self.selected_pr_number() {
                    self.toggle_files(number);
                }
            }
            Row::FileFolder(node) => {
                if self
                    .tree_nodes
                    .get(node)
                    .is_some_and(|folder| !folder.expanded)
                {
                    self.toggle_tree_folder(node);
                }
            }
            Row::File(_) => {}
        }
    }

    /// Left/`h`: close the selected row. In tree mode a file — or an
    /// already-folded folder — steps out to its parent row, like the
    /// Source Control tree.
    fn collapse(&mut self) {
        let Some(&row) = self.selected.and_then(|index| self.rows.get(index)) else {
            return;
        };
        match row {
            Row::Header(drawer) => {
                if self.drawers[drawer].expanded {
                    self.drawers[drawer].expanded = false;
                    self.rebuild();
                }
            }
            Row::Pr(drawer, i) => {
                if let Some(number) = self.drawers[drawer].prs.get(i).map(|pr| pr.number)
                    && Some(number) == self.expanded
                {
                    self.toggle_files(number);
                }
            }
            Row::FileFolder(node) => {
                if self
                    .tree_nodes
                    .get(node)
                    .is_some_and(|folder| folder.expanded)
                {
                    self.toggle_tree_folder(node);
                } else if self.tree()
                    && let Some(index) = self.selected
                    && let Some(parent) = self.parent_row(index)
                {
                    self.select(parent);
                }
            }
            Row::File(_) => {
                if self.tree()
                    && let Some(index) = self.selected
                    && let Some(parent) = self.parent_row(index)
                {
                    self.select(parent);
                }
            }
        }
    }

    /// Show a pull request's overview in the preview pane.
    fn open_overview(&mut self, number: u64) {
        let Some(pane_id) = self.pane_ctl.as_ref().map(|ctl| ctl.pane_id.clone()) else {
            self.flash = Some(("preview needs a herdr pane".into(), true));
            return;
        };
        let payload = herdr_sidebar::viewer::pr_request(&self.cwd, number);
        let doc_key = herdr_sidebar::viewer::doc_key_for_pr(&self.cwd, number);
        match herdr_sidebar::viewer::open_in_pane(&pane_id, &self.cwd, &doc_key, &payload) {
            Ok(_) => {}
            Err(e) => self.flash = Some((e, true)),
        }
    }

    /// Show one file's diff in the pull request, in the preview pane.
    fn open_file_diff(&mut self, index: usize) {
        let Some(pane_id) = self.pane_ctl.as_ref().map(|ctl| ctl.pane_id.clone()) else {
            self.flash = Some(("preview needs a herdr pane".into(), true));
            return;
        };
        let (Some(number), Some(file)) = (self.expanded, self.files.get(index)) else {
            return;
        };
        let kind = format!("pr:{number}");
        let payload = herdr_sidebar::viewer::diff_request(&self.cwd, &file.entry.path, &kind);
        let doc_key = herdr_sidebar::viewer::doc_key_for_diff(&self.cwd, &file.entry.path, &kind);
        match herdr_sidebar::viewer::open_in_pane(&pane_id, &self.cwd, &doc_key, &payload) {
            Ok(_) => {}
            Err(e) => self.flash = Some((e, true)),
        }
    }

    /// A left click on an overlay. A menu click chooses the CLICKED entry —
    /// a click used to confirm the merely SELECTED one, so clicking "Squash
    /// and Merge" ran "Open Pull Request" instead. A click outside the menu
    /// dismisses it. Other overlays keep confirming the current selection.
    fn overlay_click(&mut self, x: u16, y: u16) {
        enum Outcome {
            Run(u64, MenuAction),
            Dismiss,
            ConfirmCurrent,
        }
        let outcome = match &self.overlay {
            Some(Overlay::Menu {
                number,
                entries,
                rect,
                ..
            }) => match menu_hit(entries, *rect, x, y) {
                Some(index) => match entries[index] {
                    MenuEntry::Action(action, _) => Outcome::Run(*number, action),
                    MenuEntry::Separator => return,
                },
                None => Outcome::Dismiss,
            },
            Some(_) => Outcome::ConfirmCurrent,
            None => return,
        };
        match outcome {
            Outcome::Run(number, action) => {
                self.overlay = None;
                self.run_menu_action(number, action);
            }
            Outcome::Dismiss => self.overlay = None,
            Outcome::ConfirmCurrent => {
                self.overlay_key(KeyEvent::new(
                    KeyCode::Enter,
                    crossterm::event::KeyModifiers::NONE,
                ));
            }
        }
    }

    /// `m`: the context menu of the selected pull request.
    fn open_menu(&mut self) {
        let Some(index) = self.selected else { return };
        let Some(&Row::Pr(drawer, i)) = self.rows.get(index) else {
            return;
        };
        let Some(pr) = self.drawers[drawer].prs.get(i) else {
            return;
        };
        let number = pr.number;
        let entries = menu_entries(pr);
        self.overlay = Some(Overlay::Menu {
            number,
            entries,
            selected: 0,
            rect: Rect::default(),
        });
    }

    fn run_menu_action(&mut self, number: u64, action: MenuAction) {
        match action {
            MenuAction::OpenOverview => self.open_overview(number),
            MenuAction::CopyUrl | MenuAction::CopyBranch => {
                let Some(pr) = self.find_pr(number) else {
                    return;
                };
                let (label, text) = if action == MenuAction::CopyUrl {
                    ("url", pr.url.clone())
                } else {
                    ("branch", pr.head.clone())
                };
                self.flash = Some(match copy_to_clipboard(&text) {
                    Ok(()) => (format!("copied {label}: {text}"), false),
                    Err(e) => (format!("copy failed: {e}"), true),
                });
            }
            MenuAction::ResolveThreads => {
                self.flash = Some((action.progress(number), false));
                let root = self.cwd.clone();
                let work = move || Job::Threads(pr::threads(&root, number));
                self.spawn_job(JobKind::Threads(number), work);
            }
            _ if action.needs_confirm() => {
                self.overlay = Some(Overlay::Confirm { number, action });
            }
            _ if action.needs_body() => {
                self.overlay = Some(Overlay::Input {
                    number,
                    action,
                    text: Vec::new(),
                    cursor: 0,
                });
            }
            _ => self.spawn_action(number, action, String::new()),
        }
    }

    /// Run a mutating `gh` command on a worker thread.
    fn spawn_action(&mut self, number: u64, action: MenuAction, body: String) {
        self.flash = Some((action.progress(number), false));
        let root = self.cwd.clone();
        let work = move || {
            let result = match action {
                MenuAction::Checkout => pr::checkout(&root, number),
                MenuAction::Merge(method) => pr::merge(&root, number, method),
                MenuAction::Ready => pr::ready(&root, number),
                MenuAction::Review(verdict) => pr::review(&root, number, verdict, &body),
                MenuAction::Comment => pr::comment(&root, number, &body),
                MenuAction::OpenInBrowser => pr::open_in_browser(&root, number),
                _ => Ok(String::new()),
            };
            Job::Command(result)
        };
        self.spawn_job(JobKind::Action, work);
    }

    /// Run `work` on a worker thread; `poll` routes the result back by kind.
    fn spawn_job<F>(&mut self, kind: JobKind, work: F)
    where
        F: FnOnce() -> Job + Send + 'static,
    {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(work());
        });
        self.job = Some((kind, receiver));
    }

    /// Apply a finished background job.
    fn apply_job(&mut self, kind: JobKind, job: Job) {
        match (kind, job) {
            (JobKind::Action, Job::Command(result)) => {
                self.flash = Some(match result {
                    Ok(text) => (summary_of(&text), false),
                    Err(e) => (e, true),
                });
                self.refresh();
            }
            (JobKind::Threads(number), Job::Threads(result)) => match result {
                Ok(threads) => {
                    if threads.is_empty() {
                        self.flash = Some(("no unresolved conversations".into(), false));
                    } else {
                        self.flash = None;
                        self.overlay = Some(Overlay::Threads {
                            number,
                            threads,
                            selected: 0,
                        });
                    }
                }
                Err(e) => self.flash = Some((e, true)),
            },
            (JobKind::Resolve(id), Job::Command(result)) => {
                match result {
                    Ok(_) => {
                        if let Some(Overlay::Threads {
                            threads, selected, ..
                        }) = &mut self.overlay
                        {
                            threads.retain(|thread| thread.id != id);
                            *selected = (*selected).min(threads.len().saturating_sub(1));
                        }
                        self.flash = Some(("conversation resolved".into(), false));
                    }
                    Err(e) => self.flash = Some((e, true)),
                }
                self.refresh();
            }
            _ => {}
        }
    }

    fn find_pr(&self, number: u64) -> Option<&PullRequest> {
        self.drawers
            .iter()
            .flat_map(|drawer| drawer.prs.iter())
            .find(|pr| pr.number == number)
    }

    fn switch_to(&mut self, view: View) -> Option<Exit> {
        if !self.merged() || view == MY_VIEW {
            return None;
        }
        self.sidebar_state = sidebar::update_state(|state| {
            state.active = view;
            state.search_active = false;
        });
        Some(Exit::Switch(view))
    }

    fn open_search(&mut self) -> Option<Exit> {
        if !self.merged() {
            return None;
        }
        self.sidebar_state = sidebar::update_state(|state| {
            state.active = View::Explorer;
            state.search_active = true;
        });
        Some(Exit::Search { focus_query: false })
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<Exit> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        // Ctrl+Q from a toggle launcher: close our own pane gracefully, in
        // front of any overlay — like the other views.
        if key.code == KeyCode::Char('q')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            self.close(false);
            return None;
        }
        // Ctrl+P / F12 is the host's Quick Open gesture, like in the other
        // apps — but only the unified sidebar has an Explorer to open it in.
        if ((key.code == KeyCode::Char('p')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT))
            || key.code == KeyCode::F(12))
            && self.merged()
        {
            self.sidebar_state = sidebar::update_state(|state| {
                state.active = View::Explorer;
                state.search_active = false;
            });
            return Some(Exit::QuickOpen);
        }
        // View-switch transports reach past any open overlay or focused
        // input: F9–F11 are the host's synthetic keys for views 1–3, and
        // Ctrl+1/2/3/4 mirror the activity-bar chords (Ctrl+4 reaches this
        // view, a no-op while it is already showing — including in a pinned
        // standalone pane).
        let injected_view = match key.code {
            KeyCode::F(9) => Some('1'),
            KeyCode::F(10) => Some('2'),
            KeyCode::F(11) => Some('3'),
            _ => None,
        };
        let keyboard_view = match key.code {
            KeyCode::Char(c @ ('1' | '2' | '3' | '4'))
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                Some(c)
            }
            _ => None,
        };
        if let Some(c) = injected_view.or(keyboard_view) {
            self.overlay = None;
            return match c {
                '1' => self.switch_to(View::Explorer),
                '2' => self.open_search(),
                '4' => self.switch_to(View::PullRequests),
                _ => self.switch_to(View::SourceControl),
            };
        }
        if self.overlay.is_some() {
            return self.overlay_key(key);
        }
        self.flash = None;
        match key.code {
            KeyCode::Char('q') => return Some(Exit::Quit),
            KeyCode::Char('b') => self.hide(),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::PageDown => self.move_by(9),
            KeyCode::PageUp => self.move_by(-9),
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => {
                let last = self.rows.len().saturating_sub(1);
                self.select(last);
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.activate(),
            KeyCode::Char('m') => self.open_menu(),
            KeyCode::Char('t') => self.toggle_tree_view(),
            KeyCode::Right | KeyCode::Char('l') => self.expand(),
            KeyCode::Left | KeyCode::Char('h') => self.collapse(),
            KeyCode::Char('1') => return self.switch_to(View::Explorer),
            KeyCode::Char('2') => return self.open_search(),
            KeyCode::Char('3') => return self.switch_to(View::SourceControl),
            _ => {}
        }
        None
    }

    fn overlay_key(&mut self, key: KeyEvent) -> Option<Exit> {
        // Read the key against the open overlay first, then act: every branch
        // below needs `&mut self` for the action itself.
        let mut chosen: Option<(u64, MenuAction)> = None;
        let mut agreed: Option<bool> = None;
        let mut submitted = false;
        let mut resolve: Option<String> = None;
        let mut close = false;
        match &mut self.overlay {
            Some(Overlay::Menu {
                number,
                entries,
                selected,
                ..
            }) => match key.code {
                KeyCode::Esc => close = true,
                KeyCode::Char('j') | KeyCode::Down => step_menu(entries, selected, 1),
                KeyCode::Char('k') | KeyCode::Up => step_menu(entries, selected, -1),
                KeyCode::Enter | KeyCode::Char(' ') => {
                    if let Some(MenuEntry::Action(action, _)) = entries.get(*selected) {
                        chosen = Some((*number, *action));
                    }
                }
                _ => {}
            },
            Some(Overlay::Confirm { .. }) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => agreed = Some(true),
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => agreed = Some(false),
                _ => {}
            },
            Some(Overlay::Input { text, cursor, .. }) => match key.code {
                KeyCode::Esc => close = true,
                KeyCode::Enter => submitted = true,
                KeyCode::Backspace => {
                    if *cursor > 0 {
                        *cursor -= 1;
                        text.remove(*cursor);
                    }
                }
                KeyCode::Left => *cursor = cursor.saturating_sub(1),
                KeyCode::Right => *cursor = (*cursor + 1).min(text.len()),
                KeyCode::Char(character) if !character.is_control() => {
                    text.insert(*cursor, character);
                    *cursor += 1;
                }
                _ => {}
            },
            Some(Overlay::Threads {
                threads, selected, ..
            }) => match key.code {
                KeyCode::Esc => close = true,
                KeyCode::Char('j') | KeyCode::Down => {
                    *selected = (*selected + 1).min(threads.len().saturating_sub(1));
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    *selected = selected.saturating_sub(1);
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    resolve = threads.get(*selected).map(|thread| thread.id.clone());
                }
                _ => {}
            },
            None => {}
        }
        if close {
            self.overlay = None;
        }
        if let Some((number, action)) = chosen {
            self.overlay = None;
            self.run_menu_action(number, action);
        }
        if let Some(agree) = agreed
            && let Some(Overlay::Confirm { number, action }) = self.overlay.take()
        {
            if agree {
                self.spawn_action(number, action, String::new());
            } else {
                self.flash = Some(("cancelled".into(), false));
            }
        }
        if submitted
            && let Some(Overlay::Input {
                number,
                action,
                text,
                ..
            }) = self.overlay.take()
        {
            let body: String = text.iter().collect();
            self.spawn_action(number, action, body);
        }
        if let Some(id) = resolve {
            self.flash = Some(("resolving conversation…".into(), false));
            let root = self.cwd.clone();
            let thread = id.clone();
            let work = move || Job::Command(pr::resolve_thread(&root, &thread));
            self.spawn_job(JobKind::Resolve(id), work);
        }
        None
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) -> Option<Exit> {
        self.last_mouse = Some(Instant::now());
        self.mouse_pos = Some((mouse.column, mouse.row));
        if self.overlay.is_some() {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                self.overlay_click(mouse.column, mouse.row);
            }
            return None;
        }
        let (x, y) = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollDown => self.move_by(3),
            MouseEventKind::ScrollUp => self.move_by(-3),
            MouseEventKind::Down(MouseButton::Left) => {
                if self.merged() {
                    if hits_activity_button(self.activity_zones[0], self.activity_row, x, y) {
                        return self.switch_to(View::Explorer);
                    }
                    if hits_activity_button(self.activity_zones[1], self.activity_row, x, y) {
                        return self.open_search();
                    }
                    if hits_activity_button(self.activity_zones[2], self.activity_row, x, y) {
                        return self.switch_to(View::SourceControl);
                    }
                    if hits_activity_button(self.activity_zones[3], self.activity_row, x, y) {
                        return None;
                    }
                }
                let index = self.row_at(y)?;
                self.select(index);
                // The chevron column opens the file list; the rest of the row
                // opens the request itself, the way VS Code splits the two.
                if x <= self.body.x.saturating_add(1)
                    && matches!(self.rows.get(index), Some(Row::Pr(..)))
                {
                    self.expand();
                } else {
                    self.activate();
                }
            }
            _ => {}
        }
        None
    }

    /// The row under a screen `y`, using the same window the draw used.
    fn row_at(&self, y: u16) -> Option<usize> {
        let area = self.body;
        if !hits(area, area.x, y) || area.height == 0 {
            return None;
        }
        let offset = y.saturating_sub(area.y) as usize;
        let index = self.scroll + offset;
        (index < self.rows.len()).then_some(index)
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.last_width = area.width;
        self.last_height = area.height;
        let activity_height = u16::from(self.merged()) * 3;
        let footer = self.footer_lines(area.width);
        let footer_height = (footer.len() as u16).max(1);
        let sections: [Rect; 4] = Layout::vertical([
            Constraint::Length(activity_height),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(footer_height),
        ])
        .areas(area);
        if self.merged() {
            self.draw_activity_bar(frame, sections[0]);
        }
        self.draw_header(frame, sections[1]);
        self.body = sections[2];
        let visible = self.prepare_list(sections[2]);
        self.draw_list(frame, sections[2], visible);
        frame.render_widget(Paragraph::new(footer), sections[3]);
        self.draw_overlay(frame, area);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let count: usize = self.drawers.iter().map(|drawer| drawer.prs.len()).sum();
        let left = Line::from(vec![
            Span::raw(" "),
            Span::styled("Pull Requests", Style::default().bold()),
        ]);
        let right = Span::styled(
            format!("{count} open  "),
            Style::default().fg(palette().untracked),
        );
        let [left_area, right_area]: [Rect; 2] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(12)]).areas(area);
        frame.render_widget(Paragraph::new(left), left_area);
        frame.render_widget(
            Paragraph::new(right).alignment(Alignment::Right),
            right_area,
        );
    }

    /// The rows the viewport shows, and the scroll it needs.
    fn prepare_list(&mut self, area: Rect) -> usize {
        let height = area.height as usize;
        if height == 0 {
            return 0;
        }
        let len = self.rows.len();
        if let Some(selected) = self.selected
            && self.snap
        {
            if selected < self.scroll {
                self.scroll = selected;
            } else if selected >= self.scroll + height {
                self.scroll = selected + 1 - height;
            }
            self.snap = false;
        }
        self.scroll = self.scroll.min(len.saturating_sub(1));
        height.min(len.saturating_sub(self.scroll))
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect, visible: usize) {
        if self.rows.is_empty() {
            let text = if self.fetching.is_some() {
                "(loading pull requests…)"
            } else {
                "(no pull requests)"
            };
            frame.render_widget(Paragraph::new(text).dim().italic(), area);
            return;
        }
        let width = area.width as usize;
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(visible)
            .map(|(index, row)| {
                let hovered = self.hovered == Some(index);
                let item = match *row {
                    Row::Header(drawer) => {
                        let panel = &self.drawers[drawer];
                        header_item(
                            PrFilter::ALL[drawer],
                            panel.expanded,
                            panel.prs.len(),
                            width,
                        )
                    }
                    Row::Pr(drawer, i) => match self.drawers[drawer].prs.get(i) {
                        Some(pr) => pr_item(pr, self.expanded == Some(pr.number), width),
                        None => ListItem::new(Line::default()),
                    },
                    Row::File(i) => match self.files.get(i) {
                        Some(file) => {
                            let tree = self
                                .tree()
                                .then(|| self.row_depth.get(index).copied().unwrap_or(0));
                            file_item(file, tree, width, self.theme)
                        }
                        None => ListItem::new(Line::default()),
                    },
                    Row::FileFolder(node) => match self.tree_nodes.get(node) {
                        Some(folder) => folder_item(folder, width, self.theme),
                        None => ListItem::new(Line::default()),
                    },
                };
                if self.selected == Some(index) {
                    item.style(selection_style(true))
                } else if hovered {
                    item.style(hover_style())
                } else {
                    item
                }
            })
            .collect();
        frame.render_widget(List::new(items), area);
        draw_scrollbar(frame, area, self.rows.len(), visible, self.scroll);
    }

    fn draw_activity_bar(&mut self, frame: &mut Frame, area: Rect) {
        let outer_top = area.y;
        let outer_bottom = area.y + 2;
        let area = Rect::new(area.x, area.y + 1, area.width, 1);
        let [exp_icon, search_icon, git_icon, pr_icon] = activity_icons(self.theme);
        let slack = if self.theme == IconTheme::Material {
            " "
        } else {
            ""
        };
        let mut spans = [
            Span::raw(" "),
            Span::raw(format!(" {exp_icon}{slack} ")),
            Span::raw(" "),
            Span::raw(format!(" {search_icon}{slack} ")),
            Span::raw(" "),
            Span::raw(format!(" {git_icon}{slack} ")),
            Span::raw(" "),
            Span::raw(format!(" {pr_icon}{slack} ")),
        ];
        let mut x = area.x;
        let mut bounds = Vec::new();
        for span in &spans {
            let w = span.width() as u16;
            bounds.push((x, x + w));
            x += w;
        }
        self.activity_row = area.y;
        self.activity_zones = [bounds[1], bounds[3], bounds[5], bounds[7]];
        let hovered = |bounds| {
            self.mouse_pos
                .is_some_and(|(x, y)| hits_activity_button(bounds, area.y, x, y))
        };
        let explorer_hovered = hovered(bounds[1]);
        let search_hovered = hovered(bounds[3]);
        let git_hovered = hovered(bounds[5]);
        let pr_hovered = hovered(bounds[7]);
        spans[1].style = activity_button_style(false, explorer_hovered);
        spans[3].style = activity_button_style(false, search_hovered);
        spans[5].style = activity_button_style(false, git_hovered);
        spans[7].style = activity_button_style(true, pr_hovered);
        draw_activity_caps(
            frame,
            bounds[7],
            outer_top,
            outer_bottom,
            palette().selection_bg,
        );
        for (is_hovered, button_bounds) in [
            (explorer_hovered, bounds[1]),
            (search_hovered, bounds[3]),
            (git_hovered, bounds[5]),
        ] {
            if is_hovered {
                draw_activity_caps(
                    frame,
                    button_bounds,
                    outer_top,
                    outer_bottom,
                    palette().hover_bg,
                );
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans.to_vec())), area);
    }

    fn footer_lines(&self, width: u16) -> Vec<Line<'static>> {
        let modal: Option<String> = match &self.overlay {
            Some(Overlay::Confirm { number, action }) => Some(action.confirm_prompt(*number)),
            Some(Overlay::Input { action, .. }) => Some(
                match action {
                    MenuAction::Comment => "Comment — ⏎ to send, esc to cancel",
                    _ => "Review body — ⏎ to send, esc to cancel",
                }
                .to_string(),
            ),
            Some(Overlay::Threads { .. }) => {
                Some("⏎ resolves the conversation, esc closes".to_string())
            }
            _ => None,
        };
        if let Some(text) = modal {
            return wrap_footer_message(&text, width, 4)
                .into_iter()
                .map(Line::from)
                .collect();
        }
        if let Some((text, is_error)) = &self.flash {
            let color = if *is_error {
                palette().deleted
            } else {
                palette().untracked
            };
            return wrap_footer_message(text, width, 4)
                .into_iter()
                .map(|line| Line::from(Span::styled(line, Style::default().fg(color))))
                .collect();
        }
        if !self.sidebar_state.show_hotkeys {
            return Vec::new();
        }
        wrap_hints(
            &[
                ("⏎", "overview"),
                ("l", "files"),
                ("t", "tree"),
                ("m", "menu"),
                ("r", "refresh"),
            ],
            width,
            0,
        )
    }

    fn draw_overlay(&mut self, frame: &mut Frame, area: Rect) {
        match &mut self.overlay {
            Some(Overlay::Menu {
                entries,
                selected,
                rect,
                ..
            }) => {
                let width = entries
                    .iter()
                    .map(|entry| match entry {
                        MenuEntry::Action(_, label) => label.len() + 4,
                        MenuEntry::Separator => 0,
                    })
                    .max()
                    .unwrap_or(12)
                    .clamp(12, usize::from(area.width.saturating_sub(2)));
                let height = (entries.len() as u16 + 2).min(area.height);
                let popup_rect = Rect {
                    x: area.x + (area.width.saturating_sub(width as u16)) / 2,
                    y: area.y + (area.height.saturating_sub(height)) / 2,
                    width: width as u16,
                    height,
                };
                *rect = popup_rect;
                frame.render_widget(Clear, popup_rect);
                let items: Vec<ListItem> = entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| match entry {
                        MenuEntry::Separator => ListItem::new(Line::from(Span::raw(""))),
                        MenuEntry::Action(_, label) => {
                            let line = Line::from(Span::raw(format!(" {label}")));
                            if index == *selected {
                                ListItem::new(line).style(selection_style(true))
                            } else {
                                ListItem::new(line)
                            }
                        }
                    })
                    .collect();
                frame.render_widget(
                    List::new(items).block(Block::bordered().border_style(Style::default().dim())),
                    popup_rect,
                );
            }
            Some(Overlay::Confirm { number, action }) => {
                popup(
                    frame,
                    area,
                    &[Line::from(action.confirm_prompt(*number))],
                    Some("Confirm"),
                );
            }
            Some(Overlay::Input {
                action,
                text,
                cursor,
                ..
            }) => {
                let label = if matches!(action, MenuAction::Comment) {
                    "Comment"
                } else {
                    "Review body"
                };
                let body: String = text.iter().collect();
                let mut line = format!(" {label}: {body}");
                let at = 1 + label.chars().count() + 2 + *cursor;
                line.insert(at.min(line.len()), '│');
                popup(frame, area, &[Line::from(line)], Some(label));
            }
            Some(Overlay::Threads {
                number,
                threads,
                selected,
            }) => {
                let items: Vec<Line> = threads
                    .iter()
                    .enumerate()
                    .map(|(index, thread)| {
                        let line = Line::from(vec![
                            Span::styled(
                                format!(" {}:{} ", thread.path, thread.line),
                                Style::default().fg(palette().modified),
                            ),
                            Span::styled(thread.snippet.clone(), Style::default()),
                        ]);
                        if index == *selected {
                            line.style(selection_style(true))
                        } else {
                            line
                        }
                    })
                    .collect();
                popup(
                    frame,
                    area,
                    &items,
                    Some(&format!("Resolve conversation · PR #{number}")),
                );
            }
            None => {}
        }
    }
}

/// A centered bordered popup sized to `lines`.
fn popup(frame: &mut Frame, area: Rect, lines: &[Line<'static>], title: Option<&str>) {
    let content = lines.iter().map(Line::width).max().unwrap_or(12);
    let width = (content as u16 + 4).clamp(16, area.width.saturating_sub(2).max(16));
    let height = (lines.len() as u16 + 2).min(area.height.max(3));
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    let mut block = Block::bordered().border_style(Style::default().dim());
    if let Some(title) = title {
        block = block.title(format!(" {title} "));
    }
    frame.render_widget(Paragraph::new(lines.to_vec()).block(block), rect);
}

/// Next selectable menu index in `direction`, staying put at the ends.
fn step_menu(entries: &[MenuEntry], selected: &mut usize, direction: isize) {
    let mut index = *selected as isize;
    loop {
        index += direction;
        if index < 0 || index >= entries.len() as isize {
            return;
        }
        if matches!(entries[index as usize], MenuEntry::Action(..)) {
            *selected = index as usize;
            return;
        }
    }
}

/// A one-line summary of a command's stdout (`gh` prints URLs and notices).
fn summary_of(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| herdr_sidebar::ui::truncate_to(line.to_string(), 60))
        .unwrap_or_else(|| "done".to_string())
}

/// The row list: every drawer header, its pull requests when open, and the
/// expanded one's files — a tree when `tree`, a flat list otherwise. Returns
/// the rows with their indent depths, which must stay parallel to each other.
/// Files and folders nest one level under their pull request, like the
/// Source Control tree nests under its section.
fn build_rows(
    drawers: &[DrawerPanel; 3],
    expanded: Option<u64>,
    files: &[PrFile],
    tree: bool,
    collapsed: &BTreeSet<String>,
    nodes: &mut Vec<TreeNode>,
) -> (Vec<Row>, Vec<usize>) {
    let mut rows = Vec::new();
    let mut depths = Vec::new();
    for (drawer, panel) in drawers.iter().enumerate() {
        rows.push(Row::Header(drawer));
        depths.push(0);
        if !panel.expanded {
            continue;
        }
        for (index, pr) in panel.prs.iter().enumerate() {
            rows.push(Row::Pr(drawer, index));
            depths.push(0);
            if expanded != Some(pr.number) {
                continue;
            }
            if tree {
                let entries: Vec<FileEntry> =
                    files.iter().map(|file| file.entry.clone()).collect();
                // Collapse keys are `N:path`; strip this request's prefix so
                // the shared SCM tree builder sees plain repo paths.
                let prefix = format!("{}:", pr.number);
                let scoped: BTreeSet<String> = collapsed
                    .iter()
                    .filter_map(|key| key.strip_prefix(&prefix).map(str::to_string))
                    .collect();
                for row in changes_tree_rows(&entries, &scoped) {
                    match row {
                        ChangeTreeRow::Folder {
                            path,
                            label,
                            depth,
                            expanded,
                        } => {
                            rows.push(Row::FileFolder(nodes.len()));
                            depths.push(depth + 1);
                            nodes.push(TreeNode {
                                path,
                                label,
                                depth: depth + 1,
                                expanded,
                            });
                        }
                        ChangeTreeRow::File { index, depth } => {
                            rows.push(Row::File(index));
                            depths.push(depth + 1);
                        }
                    }
                }
            } else {
                for index in 0..files.len() {
                    rows.push(Row::File(index));
                    depths.push(1);
                }
            }
        }
    }
    (rows, depths)
}

/// A drawer header row.
fn header_item(filter: PrFilter, expanded: bool, count: usize, width: usize) -> ListItem<'static> {
    ListItem::new(Line::from(header_spans(filter, expanded, count, width)))
}

/// A drawer header's spans: chevron, title and the page size.
fn header_spans(
    filter: PrFilter,
    expanded: bool,
    count: usize,
    width: usize,
) -> Vec<Span<'static>> {
    let chevron = if expanded { "▾" } else { "▸" };
    let label = format!(" {chevron} {} ", filter.title());
    let tail = format!("{count} ");
    let pad = width.saturating_sub(label.chars().count() + tail.chars().count());
    vec![
        Span::styled(label, Style::default().bold()),
        Span::raw(" ".repeat(pad)),
        Span::styled(tail, Style::default().dim()),
    ]
}

/// A pull-request row.
fn pr_item(pr: &PullRequest, expanded: bool, width: usize) -> ListItem<'static> {
    ListItem::new(Line::from(pr_spans(pr, expanded, width)))
}

/// A pull-request row's spans: the expand chevron, number, review marker,
/// draft note, title, and a dim `author head→base +A −D` tail — the branch
/// and size VS Code's GitHub view shows, so a bare title never stands alone.
fn pr_spans(pr: &PullRequest, expanded: bool, width: usize) -> Vec<Span<'static>> {
    let marker = pr.review.glyph();
    let color = match pr.review {
        ReviewState::Approved => palette().untracked,
        ReviewState::ChangesRequested => palette().deleted,
        ReviewState::Pending => palette().modified,
    };
    let chevron = if expanded { "▾" } else { "▸" };
    let prefix = format!("#{} ", pr.number);
    let draft = if pr.draft { "draft " } else { "" };
    let text = format!("{draft}{}", pr.title);
    let overhead = prefix.chars().count() + marker.chars().count() + 4;
    let room = width.saturating_sub(overhead).max(4);
    let lead = format!(" · {} {}→", pr.author, pr.head);
    let trail = format!(" +{} −{}", pr.additions, pr.deletions);
    let detail_len = lead.chars().count() + pr.base.chars().count() + trail.chars().count();
    // Keep the tail when the title can stay readable beside it; otherwise
    // fall back to a title-only row rather than a one-word stub. The
    // ARRIVAL branch (`pr.base`) leaves the dim tail so it reads as the
    // target of the request instead of blending with the metadatum.
    let (title, tail) = if text.chars().count() + detail_len <= room {
        (text, detail_len)
    } else if room.saturating_sub(detail_len) >= 8 {
        (
            herdr_sidebar::ui::truncate_to(text, room - detail_len),
            detail_len,
        )
    } else {
        (herdr_sidebar::ui::truncate_to(text, room), 0)
    };
    let mut spans = vec![
        Span::styled(chevron, Style::default().dim()),
        Span::raw(" "),
        Span::styled(prefix, Style::default().dim()),
        Span::styled(format!("{marker} "), Style::default().fg(color)),
        Span::raw(title),
    ];
    if tail > 0 {
        spans.push(Span::styled(lead, Style::default().dim()));
        spans.push(Span::styled(pr.base.clone(), Style::default().fg(palette().header_accent)));
        spans.push(Span::styled(trail, Style::default().dim()));
    }
    spans
}

/// A file row of the expanded pull request.
fn file_item(file: &PrFile, depth: Option<usize>, width: usize, theme: IconTheme) -> ListItem<'static> {
    ListItem::new(Line::from(file_spans(file, depth, width, theme)))
}

/// A file row's spans: the Source Control view's file-row anatomy (indent,
/// icon, the bare name in its status color, the right-aligned letter) plus
/// the `+A −D` churn. A tree row nests under its folders; a flat row
/// carries the dim directory instead.
fn file_spans(
    file: &PrFile,
    depth: Option<usize>,
    width: usize,
    theme: IconTheme,
) -> Vec<Span<'static>> {
    let (dir, name) = match file.entry.path.rsplit_once('/') {
        Some((dir, name)) => (Some(dir), name),
        None => (None, file.entry.path.as_str()),
    };
    let color = herdr_sidebar::ui::status_color(file.entry.letter);
    let file_icon = icon(theme, name, false, false);
    let icon_style = herdr_sidebar::ui::icon_style(file_icon.rgb);
    let indent = match depth {
        Some(depth) => 3 + depth * 2 + 2,
        None => 3,
    };
    let mut spans = vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(format!("{} ", file_icon.glyph), icon_style),
    ];
    let churn = format!("+{} −{}", file.additions, file.deletions);
    let tail = 2 + churn.chars().count();
    let prefix_width: usize = spans.iter().map(Span::width).sum();
    let room = width.saturating_sub(prefix_width + tail).max(6);
    let visible_name = herdr_sidebar::ui::truncate_to(name.to_string(), room);
    spans.push(Span::styled(visible_name, Style::default().fg(color)));
    if let Some(dir) = dir.filter(|_| depth.is_none()) {
        let used: usize = spans.iter().map(Span::width).sum();
        let avail = width.saturating_sub(used + tail);
        let text = herdr_sidebar::ui::truncate_to(format!(" {dir}"), avail);
        if !text.is_empty() {
            spans.push(Span::styled(text, Style::default().dim()));
        }
    }
    let left_width: usize = spans.iter().map(Span::width).sum();
    let pad = width.saturating_sub(left_width + tail);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(
        file.entry.letter.to_string(),
        Style::default().fg(color).bold(),
    ));
    spans.push(Span::styled(format!(" {churn}"), Style::default().dim()));
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use herdr_sidebar::git::FileEntry;

    fn pull(number: u64, review: ReviewState) -> PullRequest {
        PullRequest {
            number,
            title: format!("title {number}"),
            author: "me".into(),
            head: "feat/x".into(),
            base: "main".into(),
            draft: false,
            updated: String::new(),
            url: format!("https://github.com/o/r/pull/{number}"),
            additions: 1,
            deletions: 2,
            changed_files: 1,
            review,
        }
    }

    fn drawers(authored: Vec<PullRequest>, open: Vec<PullRequest>) -> [DrawerPanel; 3] {
        let mut panels: [DrawerPanel; 3] = Default::default();
        panels[0].expanded = true;
        panels[0].prs = authored;
        panels[2].expanded = true;
        panels[2].prs = open;
        panels
    }

    fn menu_labels(entries: &[MenuEntry]) -> Vec<&'static str> {
        entries
            .iter()
            .filter_map(|entry| match entry {
                MenuEntry::Action(_, label) => Some(*label),
                MenuEntry::Separator => None,
            })
            .collect()
    }

    #[test]
    fn the_menu_offers_the_merge_options_or_ready_for_a_draft() {
        let labels = menu_labels(&menu_entries(&pull(1, ReviewState::Pending)));
        for want in [
            "Open Pull Request",
            "Open in Browser",
            "Checkout Branch",
            "Merge Commit",
            "Squash and Merge",
            "Rebase and Merge",
            "Approve",
            "Request Changes…",
            "Comment…",
            "Resolve Conversations…",
            "Copy URL",
            "Copy Branch Name",
        ] {
            assert!(labels.contains(&want), "missing {want}: {labels:?}");
        }
        assert!(!labels.contains(&"Mark Ready for Review"));
        let mut draft = pull(2, ReviewState::Pending);
        draft.draft = true;
        let labels = menu_labels(&menu_entries(&draft));
        assert!(labels.contains(&"Mark Ready for Review"), "{labels:?}");
        assert!(!labels.contains(&"Merge Commit"), "a draft is not merged");
    }

    #[test]
    fn only_repository_changes_ask_for_confirmation() {
        assert!(MenuAction::Checkout.needs_confirm());
        assert!(MenuAction::Merge(MergeMethod::Squash).needs_confirm());
        assert!(!MenuAction::Comment.needs_confirm());
        assert!(MenuAction::Comment.needs_body());
        assert!(MenuAction::Review(Verdict::RequestChanges).needs_body());
        assert!(!MenuAction::Review(Verdict::Approve).needs_body());
        assert!(
            MenuAction::Merge(MergeMethod::Commit)
                .confirm_prompt(7)
                .contains("#7")
        );
        assert!(
            MenuAction::Merge(MergeMethod::Squash)
                .progress(7)
                .contains("Squash")
        );
    }

    #[test]
    fn summary_of_takes_the_first_meaningful_line() {
        assert_eq!(
            summary_of("\n\nhttps://github.com/o/r/pull/7#merged\n"),
            "https://github.com/o/r/pull/7#merged"
        );
        assert_eq!(summary_of(""), "done");
    }

    fn pr_file(path: &str, letter: char) -> PrFile {
        PrFile {
            entry: FileEntry {
                path: path.into(),
                orig: None,
                letter,
            },
            additions: 1,
            deletions: 0,
        }
    }

    fn flat(panels: &[DrawerPanel; 3], expanded: Option<u64>, files: &[PrFile]) -> Vec<Row> {
        let mut nodes = Vec::new();
        build_rows(panels, expanded, files, false, &BTreeSet::new(), &mut nodes).0
    }

    #[test]
    fn rows_nest_files_under_their_pull_request_only() {
        let panels = drawers(
            vec![pull(7, ReviewState::Pending)],
            vec![pull(9, ReviewState::Approved)],
        );
        let none: Vec<PrFile> = Vec::new();
        assert_eq!(
            flat(&panels, None, &none),
            [
                Row::Header(0),
                Row::Pr(0, 0),
                Row::Header(1),
                Row::Header(2),
                Row::Pr(2, 0)
            ],
            "a closed drawer contributes only its header"
        );
        let two = [pr_file("a.rs", 'M'), pr_file("b.rs", 'A')];
        assert_eq!(
            flat(&panels, Some(9), &two),
            [
                Row::Header(0),
                Row::Pr(0, 0),
                Row::Header(1),
                Row::Header(2),
                Row::Pr(2, 0),
                Row::File(0),
                Row::File(1),
            ],
            "the expanded pull request carries its files"
        );
        let three = [
            pr_file("a.rs", 'M'),
            pr_file("b.rs", 'A'),
            pr_file("c.rs", 'D'),
        ];
        assert_eq!(
            flat(&panels, Some(7), &three),
            [
                Row::Header(0),
                Row::Pr(0, 0),
                Row::File(0),
                Row::File(1),
                Row::File(2),
                Row::Header(1),
                Row::Header(2),
                Row::Pr(2, 0),
            ],
            "files sit under the pull request they belong to, never under another drawer's"
        );
    }

    #[test]
    fn the_tree_view_groups_files_under_their_folders() {
        let panels = drawers(vec![pull(7, ReviewState::Pending)], vec![]);
        let files = [
            pr_file("src/a.rs", 'M'),
            pr_file("src/deep/b.rs", 'A'),
            pr_file("CLAUDE.md", 'M'),
        ];
        let mut nodes = Vec::new();
        let (rows, depths) = build_rows(
            &panels,
            Some(7),
            &files,
            true,
            &BTreeSet::new(),
            &mut nodes,
        );
        assert_eq!(
            rows,
            [
                Row::Header(0),
                Row::Pr(0, 0),
                Row::FileFolder(0),
                Row::FileFolder(1),
                Row::File(1),
                Row::File(0),
                Row::File(2),
                Row::Header(1),
                Row::Header(2),
            ],
            "folders lead at each level and files nest under them, exactly like SCM"
        );
        assert_eq!(depths, [0, 0, 1, 2, 3, 2, 1, 0, 0], "indent per row");
        assert_eq!(nodes[0].path, "src");
        assert_eq!(nodes[1].path, "src/deep");
        assert!(nodes[0].expanded, "a folder holding files is open");
        assert_eq!(nodes[0].depth, 1, "folders nest under their pull request");
    }

    #[test]
    fn collapse_keys_are_scoped_to_their_pull_request() {
        let panels = drawers(vec![pull(7, ReviewState::Pending)], vec![]);
        let files = [pr_file("src/a.rs", 'M'), pr_file("src/b.rs", 'A')];
        // Collapsing "src" for PR #9 must not fold PR #7's folder.
        let collapsed: BTreeSet<String> = ["9:src".into()].iter().cloned().collect();
        let mut nodes = Vec::new();
        let (rows, _) = build_rows(&panels, Some(7), &files, true, &collapsed, &mut nodes);
        assert!(
            nodes[0].expanded,
            "another request's collapse key leaves this tree open: {rows:?}"
        );
        let collapsed: BTreeSet<String> = ["7:src".into()].iter().cloned().collect();
        let mut nodes = Vec::new();
        let (rows, _) = build_rows(&panels, Some(7), &files, true, &collapsed, &mut nodes);
        assert!(
            !nodes[0].expanded,
            "our own collapse key folds the folder: {rows:?}"
        );
        assert_eq!(
            rows,
            [Row::Header(0), Row::Pr(0, 0), Row::FileFolder(0), Row::Header(1), Row::Header(2)],
        );
    }

    #[test]
    fn selection_survives_folds_step_outs_and_the_tree_toggle() {
        fn test_app() -> App {
            let follower = Rc::new(RefCell::new(
                herdr_sidebar::launch::CwdFollower::default(),
            ));
            App::new(std::env::temp_dir(), follower)
        }
        let mut app = test_app();
        app.fetching = None;
        app.drawers = drawers(vec![pull(7, ReviewState::Pending)], vec![]);
        app.expanded = Some(7);
        app.files = vec![
            pr_file("src/a.rs", 'M'),
            pr_file("src/deep/b.rs", 'A'),
            pr_file("CLAUDE.md", 'M'),
        ];
        // Tree rows: [Header, Pr, Folder(src), Folder(src/deep),
        // File(deep/b.rs), File(src/a.rs), File(CLAUDE.md), …].
        app.sidebar_state.scm_tree = true;
        app.rebuild();
        app.select(4);
        assert_eq!(app.rows[app.selected.unwrap()], Row::File(1));
        // Folding the folder keeps the selection on its row.
        app.toggle_tree_folder(1);
        assert_eq!(
            app.rows[app.selected.unwrap()],
            Row::FileFolder(1),
            "the fold keeps its folder selected"
        );
        // Left on a folded folder steps out to the parent.
        app.collapse();
        assert_eq!(
            app.rows[app.selected.unwrap()],
            Row::FileFolder(0),
            "a folded folder steps out"
        );
        // Left on an open folder folds it instead of leaving.
        app.collapse();
        assert_eq!(app.rows[app.selected.unwrap()], Row::FileFolder(0));
        assert!(
            !app.tree_nodes[0].expanded,
            "Left folds an open folder first"
        );
        // Flat mode keeps the selection on the same file, not the same
        // index — unfold both folders again, sit on the deep file, toggle.
        app.toggle_tree_folder(0);
        app.toggle_tree_folder(1);
        app.select(4);
        assert_eq!(app.rows[app.selected.unwrap()], Row::File(1));
        app.sidebar_state.scm_tree = false;
        app.rebuild();
        assert_eq!(
            app.rows[app.selected.unwrap()],
            Row::File(1),
            "the tree toggle keeps the file, not the index"
        );
        // Left on a flat file row is a no-op.
        app.collapse();
        assert_eq!(app.rows[app.selected.unwrap()], Row::File(1));
    }

    #[test]
    fn a_collapsed_drawer_hides_its_pull_requests() {
        let mut panels = drawers(vec![pull(7, ReviewState::Pending)], vec![]);
        panels[0].expanded = false;
        let none: Vec<PrFile> = Vec::new();
        assert_eq!(
            flat(&panels, None, &none),
            [Row::Header(0), Row::Header(1), Row::Header(2)]
        );
    }

    fn joined(spans: &[Span<'static>]) -> String {
        spans
            .iter()
            .map(|span| span.content.to_string())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn pr_row_carries_number_review_marker_title_and_tail() {
        let wide = joined(&pr_spans(&pull(74, ReviewState::Approved), false, 80));
        assert!(wide.contains("#74"), "{wide}");
        assert!(wide.contains("✓"), "{wide}");
        assert!(wide.contains("title 74"), "{wide}");
        assert!(
            wide.contains("me feat/x→main +1 −2"),
            "author, branches and churn travel with the row: {wide}"
        );
        let spans = pr_spans(&pull(74, ReviewState::Approved), false, 40);
        let narrow = joined(&spans);
        assert!(narrow.contains("#74"), "{narrow}");
        assert!(narrow.contains("title 74"), "{narrow}");
        assert_eq!(
            spans[0].content, "▸",
            "a closed request leads with its chevron"
        );
        assert!(
            spans
                .iter()
                .any(|span| span.style.fg == Some(palette().untracked))
        );
        let open = joined(&pr_spans(&pull(74, ReviewState::Approved), true, 40));
        assert!(open.starts_with('▾'), "{open}");
        let changed = joined(&pr_spans(
            &pull(75, ReviewState::ChangesRequested),
            false,
            40,
        ));
        assert!(changed.contains("✗"), "{changed}");
    }

    #[test]
    fn pr_row_marks_drafts() {
        let mut pr = pull(12, ReviewState::Pending);
        pr.draft = true;
        assert!(joined(&pr_spans(&pr, false, 80)).contains("draft title 12"));
    }

    #[test]
    fn file_row_shows_status_and_churn() {
        let file = PrFile {
            entry: FileEntry {
                path: "src/deep/app.rs".into(),
                orig: None,
                letter: 'M',
            },
            additions: 12,
            deletions: 3,
        };
        let text = joined(&file_spans(&file, None, 40, IconTheme::Emoji));
        assert!(
            text.contains("app.rs"),
            "the flat row names the file: {text}"
        );
        assert!(
            text.contains("src/deep"),
            "the flat row keeps the dim directory: {text}"
        );
        assert!(text.contains('M'), "{text}");
        assert!(text.contains("+12 −3"), "{text}");
        let tree = joined(&file_spans(&file, Some(1), 40, IconTheme::Emoji));
        assert!(tree.contains("app.rs"), "{tree}");
        assert!(
            !tree.contains("src/deep"),
            "the tree row nests under its folders instead: {tree}"
        );
        assert!(
            file_spans(&file, None, 40, IconTheme::Emoji)
                .iter()
                .any(|span| span.style.fg == Some(herdr_sidebar::ui::status_color('M'))),
            "the name takes its status color"
        );
    }

    #[test]
    fn header_row_shows_its_count() {
        let text = joined(&header_spans(PrFilter::Authored, true, 3, 40));
        assert!(text.contains("My Pull Requests"), "{text}");
        assert!(text.contains('3'), "{text}");
        assert!(joined(&header_spans(PrFilter::Open, false, 0, 40)).contains('▸'));
    }

    #[test]
    fn menu_click_hits_the_clicked_entry() {
        let entries = menu_entries(&pull(7, ReviewState::Pending));
        let rect = Rect::new(10, 5, 30, 20);
        // Entry rows sit one row under the popup's top border.
        assert_eq!(menu_hit(&entries, rect, 12, 6), Some(0));
        // The first merge entry, not the selected first row.
        let merge_row = entries
            .iter()
            .position(|entry| matches!(entry, MenuEntry::Action(MenuAction::Merge(_), _)))
            .unwrap();
        assert_eq!(
            menu_hit(&entries, rect, 12, 5 + 1 + merge_row as u16),
            Some(merge_row)
        );
        assert_eq!(menu_hit(&entries, rect, 0, 6), None, "left of the menu");
        assert_eq!(
            menu_hit(&entries, rect, 12, 5 + 1 + entries.len() as u16),
            None,
            "below the entries"
        );
    }

    #[test]
    fn clicking_a_menu_entry_runs_it() {
        let follower = Rc::new(RefCell::new(
            herdr_sidebar::launch::CwdFollower::default(),
        ));
        let mut app = App::new(std::env::temp_dir(), follower);
        let entries = menu_entries(&pull(7, ReviewState::Pending));
        let merge_row = entries
            .iter()
            .position(|entry| matches!(entry, MenuEntry::Action(MenuAction::Merge(_), _)))
            .unwrap();
        app.overlay = Some(Overlay::Menu {
            number: 7,
            entries,
            selected: 0,
            rect: Rect::new(10, 5, 30, 20),
        });
        // Clicking "Merge Commit" asks to confirm THAT action — it used to
        // run whatever entry was selected (the first row).
        app.overlay_click(12, 5 + 1 + merge_row as u16);
        assert!(
            matches!(
                app.overlay,
                Some(Overlay::Confirm {
                    number: 7,
                    action: MenuAction::Merge(MergeMethod::Commit),
                })
            ),
            "clicking merge confirms the merge"
        );
        // A click outside dismisses the menu.
        app.overlay = Some(Overlay::Menu {
            number: 7,
            entries: menu_entries(&pull(7, ReviewState::Pending)),
            selected: 0,
            rect: Rect::new(10, 5, 30, 20),
        });
        app.overlay_click(0, 0);
        assert!(app.overlay.is_none(), "outside clicks dismiss");
    }
}
