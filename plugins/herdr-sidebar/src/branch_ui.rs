use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph};

use crate::git::{Branch, Git, Status};
use crate::icons::IconTheme;
use crate::ui::{
    branch_icon, hits, hover_style, keep_visible_scroll, palette, selection_style, truncate_to,
};

const SYNC_FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
const SYNC_FRAME_MILLIS: u128 = 120;

pub fn sync_glyph(syncing: bool) -> &'static str {
    if !syncing {
        return "⟳";
    }
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    SYNC_FRAMES[(elapsed / SYNC_FRAME_MILLIS) as usize % SYNC_FRAMES.len()]
}

pub enum PickerAction {
    None,
    Close,
    Checkout(Branch),
}

pub struct BranchPicker {
    pub git: Git,
    /// "Compare with" mode: the branch whose base is being picked. The
    /// choice then sets that base instead of checking anything out.
    pub compare_for: Option<String>,
    branches: Vec<Branch>,
    /// Filter typed into the input row; `matches` indexes `branches`.
    query: String,
    matches: Vec<usize>,
    /// Position inside `matches`, not `branches`.
    selected: usize,
    scroll: usize,
    rect: Rect,
}

impl BranchPicker {
    pub fn open(git: Git) -> Result<Self, String> {
        let branches = git.branch_choices()?;
        if branches.is_empty() {
            return Err("no branches found".to_string());
        }
        let selected = branches
            .iter()
            .position(|branch| branch.current)
            .unwrap_or(0);
        Ok(Self {
            git,
            compare_for: None,
            matches: (0..branches.len()).collect(),
            branches,
            query: String::new(),
            selected,
            scroll: 0,
            rect: Rect::default(),
        })
    }

    /// Switch to "compare with" mode for `branch`, preselecting `base`.
    pub fn compare(&mut self, branch: String, base: Option<&str>) {
        self.compare_for = Some(branch);
        if let Some(i) = base.and_then(|b| self.branches.iter().position(|br| br.name == b)) {
            self.selected = i;
        }
    }

    fn chosen(&self) -> PickerAction {
        self.matches
            .get(self.selected)
            .and_then(|&i| self.branches.get(i).cloned())
            .map(PickerAction::Checkout)
            .unwrap_or(PickerAction::None)
    }

    fn refilter(&mut self) {
        self.matches = filter_branches(&self.branches, &self.query);
        self.selected = 0;
    }

    fn step(&mut self, delta: isize) {
        let last = self.matches.len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }

    /// Every printable key goes to the filter input: no letter shortcuts.
    pub fn key(&mut self, key: KeyEvent) -> PickerAction {
        let ctrl_only = key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc if self.query.is_empty() => return PickerAction::Close,
            KeyCode::Esc => {
                self.query.clear();
                self.refilter();
            }
            KeyCode::Up => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::Char('p') if ctrl_only => self.step(-1),
            KeyCode::Char('n') if ctrl_only => self.step(1),
            KeyCode::PageUp => self.step(-10),
            KeyCode::PageDown => self.step(10),
            KeyCode::Enter => return self.chosen(),
            KeyCode::Backspace => {
                if self.query.pop().is_some() {
                    self.refilter();
                }
            }
            // CONTROL+ALT is AltGr on Windows layouts: text, not a chord.
            KeyCode::Char(c) if !ctrl_only => {
                self.query.push(c);
                self.refilter();
            }
            _ => {}
        }
        PickerAction::None
    }

    pub fn mouse(&mut self, mouse: MouseEvent) -> PickerAction {
        let mut inner = self.rect.inner(ratatui::layout::Margin::new(1, 1));
        // First inner row is the filter input.
        inner.y += 1;
        inner.height = inner.height.saturating_sub(1);
        let item_at = |row: u16, col: u16| {
            (col >= inner.x
                && col < inner.x + inner.width
                && row >= inner.y
                && row < inner.y + inner.height)
                .then(|| self.scroll + usize::from(row - inner.y))
                .filter(|index| *index < self.matches.len())
        };
        match mouse.kind {
            MouseEventKind::Moved => {
                if let Some(index) = item_at(mouse.row, mouse.column) {
                    self.selected = index;
                }
                PickerAction::None
            }
            MouseEventKind::ScrollUp => {
                self.step(-3);
                PickerAction::None
            }
            MouseEventKind::ScrollDown => {
                self.step(3);
                PickerAction::None
            }
            MouseEventKind::Down(MouseButton::Left) => item_at(mouse.row, mouse.column)
                .and_then(|index| self.matches.get(index))
                .and_then(|&i| self.branches.get(i).cloned())
                .map(PickerAction::Checkout)
                .unwrap_or_else(|| {
                    if hits(self.rect, mouse.column, mouse.row) {
                        PickerAction::None
                    } else {
                        PickerAction::Close
                    }
                }),
            _ => PickerAction::None,
        }
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let desired_width = self
            .branches
            .iter()
            .map(|branch| Span::raw(branch.name.as_str()).width() + 13)
            .max()
            .unwrap_or(28)
            .max(28) as u16;
        let width = desired_width.min(area.width);
        // Height follows the full list so the popup does not jump while typing.
        let height = (self.branches.len() as u16 + 3).min(area.height).min(19);
        let popup = Rect::new(
            (area.width.saturating_sub(width)) / 2,
            (area.height.saturating_sub(height)) / 3,
            width,
            height,
        );
        self.rect = popup;
        let visible = usize::from(height.saturating_sub(3));
        self.scroll = keep_visible_scroll(self.selected, visible, self.matches.len());
        let inner_width = usize::from(width.saturating_sub(2));
        let pal = palette();
        let input = if self.query.is_empty() {
            Line::from(vec![
                Span::styled(" ", Style::default().reversed()),
                Span::styled("Filter branches…", Style::default().dim()),
            ])
        } else {
            let shown = truncate_to(self.query.clone(), inner_width.saturating_sub(1));
            Line::from(vec![
                Span::raw(shown),
                Span::styled(" ", Style::default().reversed()),
            ])
        };
        let mut items: Vec<ListItem> = vec![ListItem::new(input)];
        if self.matches.is_empty() {
            items.push(ListItem::new(Span::styled(
                "  no matching branches",
                Style::default().dim(),
            )));
        }
        items.extend(
            self.matches
                .iter()
                .enumerate()
                .skip(self.scroll)
                .take(visible)
                .map(|(index, &bi)| {
                    let branch = &self.branches[bi];
                    let mark = if branch.current { "✓ " } else { "  " };
                    let remote = if branch.remote { "  remote" } else { "" };
                    let mark_width = Span::raw(mark).width();
                    let remote_width = Span::raw(remote).width();
                    let reserved = mark_width + remote_width + 1;
                    let name =
                        truncate_to(branch.name.clone(), inner_width.saturating_sub(reserved));
                    let name_width = Span::raw(name.as_str()).width();
                    let pad = inner_width.saturating_sub(mark_width + name_width + remote_width);
                    let line = Line::from(vec![
                        Span::raw(mark),
                        Span::raw(name),
                        Span::raw(" ".repeat(pad)),
                        Span::styled(remote, Style::default().dim()),
                    ]);
                    if index == self.selected {
                        ListItem::new(line).style(selection_style(true))
                    } else {
                        ListItem::new(line)
                    }
                }),
        );
        frame.render_widget(Clear, popup);
        frame.render_widget(
            List::new(items).block(
                Block::bordered()
                    .title(if self.compare_for.is_some() {
                        " Compare With Branch "
                    } else {
                        " Switch Branch "
                    })
                    .border_style(Style::default().fg(pal.accent)),
            ),
            popup,
        );
    }
}

/// Indices of `branches` whose name contains `query` as a case-insensitive
/// subsequence (substrings included), in original order. Empty = all.
fn filter_branches(branches: &[Branch], query: &str) -> Vec<usize> {
    let query: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    branches
        .iter()
        .enumerate()
        .filter(|(_, branch)| {
            let mut wanted = query.iter().peekable();
            for c in branch.name.chars().flat_map(char::to_lowercase) {
                if wanted.peek() == Some(&&c) {
                    wanted.next();
                }
            }
            wanted.peek().is_none()
        })
        .map(|(i, _)| i)
        .collect()
}

#[derive(Clone, Copy, Default)]
pub struct FooterZones {
    pub branch: Rect,
    pub sync: Rect,
}

pub fn draw_git_footer(
    frame: &mut Frame,
    area: Rect,
    theme: IconTheme,
    status: &Status,
    syncing: bool,
    mouse_pos: Option<(u16, u16)>,
) -> FooterZones {
    let branch_text = format!(" {} {} ", branch_icon(theme), status.branch);
    let sync_icon = sync_glyph(syncing);
    let sync_text = if status.has_upstream {
        format!("{sync_icon} {}↓ {}↑", status.behind, status.ahead)
    } else {
        sync_icon.to_string()
    };
    let sync_width = Span::raw(sync_text.as_str())
        .width()
        .min(area.width as usize) as u16;
    let branch_width = Span::raw(branch_text.as_str())
        .width()
        .min(area.width.saturating_sub(sync_width) as usize) as u16;
    let branch = Rect::new(area.x, area.y, branch_width, 1);
    let sync = Rect::new(area.x + branch_width, area.y, sync_width, 1);
    let button_style = |rect| {
        if mouse_pos.is_some_and(|(x, y)| hits(rect, x, y)) {
            hover_style()
        } else {
            Style::default().dim()
        }
    };
    frame.render_widget(
        Paragraph::new(truncate_to(branch_text, usize::from(branch_width)))
            .style(button_style(branch))
            .alignment(Alignment::Left),
        branch,
    );
    frame.render_widget(
        Paragraph::new(truncate_to(sync_text, usize::from(sync_width)))
            .style(button_style(sync))
            .alignment(Alignment::Left),
        sync,
    );
    FooterZones { branch, sync }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_starts_on_current_branch() {
        let git = Git::discover(std::path::Path::new(".")).unwrap();
        let picker = BranchPicker::open(git).unwrap();
        assert!(picker.branches[picker.matches[picker.selected]].current);
    }

    fn branch(name: &str, remote: bool) -> Branch {
        Branch {
            name: name.to_string(),
            current: false,
            remote,
        }
    }

    #[test]
    fn filter_is_case_insensitive_subsequence() {
        let b = [
            branch("main", false),
            branch("feat/Branch-Picker", false),
            branch("origin/fix-bug", true),
        ];
        assert_eq!(filter_branches(&b, ""), vec![0, 1, 2]);
        assert_eq!(filter_branches(&b, "PICK"), vec![1]);
        assert_eq!(filter_branches(&b, "fbp"), vec![1]);
        assert_eq!(filter_branches(&b, "ofb"), vec![2]);
        assert_eq!(filter_branches(&b, "zz"), Vec::<usize>::new());
    }

    #[test]
    fn typing_filters_and_esc_clears_then_closes() {
        use crossterm::event::KeyModifiers as M;
        let git = Git::discover(std::path::Path::new(".")).unwrap();
        let mut p = BranchPicker::open(git).unwrap();
        p.branches = vec![
            branch("main", false),
            branch("dev", false),
            branch("demo", false),
        ];
        p.refilter();
        let k = |c, m| KeyEvent::new(c, m);
        p.key(k(KeyCode::Char('d'), M::NONE));
        assert_eq!(p.matches, vec![1, 2]);
        p.key(k(KeyCode::Char('n'), M::CONTROL));
        assert_eq!(p.selected, 1);
        p.key(k(KeyCode::Char('e'), M::NONE));
        assert_eq!(p.selected, 0, "selection resets per keystroke");
        // 'j'/'k' are text, AltGr (CONTROL|ALT) chars are text too.
        p.key(k(KeyCode::Char('j'), M::NONE));
        p.key(k(KeyCode::Char('@'), M::CONTROL | M::ALT));
        assert_eq!(p.query, "dej@");
        p.key(k(KeyCode::Backspace, M::NONE));
        p.key(k(KeyCode::Backspace, M::NONE));
        p.key(k(KeyCode::Down, M::NONE));
        match p.key(k(KeyCode::Enter, M::NONE)) {
            PickerAction::Checkout(b) => assert_eq!(b.name, "demo"),
            _ => panic!("expected pick"),
        }
        assert!(matches!(
            p.key(k(KeyCode::Esc, M::NONE)),
            PickerAction::None
        ));
        assert!(p.query.is_empty());
        assert!(matches!(
            p.key(k(KeyCode::Esc, M::NONE)),
            PickerAction::Close
        ));
    }

    #[test]
    fn sync_glyph_restores_refresh_icon_when_idle() {
        assert_eq!(sync_glyph(false), "⟳");
        assert!(SYNC_FRAMES.contains(&sync_glyph(true)));
    }
}
