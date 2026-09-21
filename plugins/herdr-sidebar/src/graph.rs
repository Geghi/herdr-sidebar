//! Commit-graph lane layout: turn a window of commits (newest first) into
//! fixed-width rows of glyph cells, the geometry a VS Code "Git Graph" view
//! draws. This is pure geometry — no git access, no terminal, no colors
//! beyond a `0..=7` lane slot the renderer maps to a theme color.
//!
//! Glyphs: `●` is a commit, `│` a lane passing straight through the row, `─`
//! a horizontal joining two lanes on the row, `╭ ╮` the corners where an edge
//! turns, and `├ ┤ ┬ ┴ ┼` the tees where a horizontal joins a rail that keeps
//! running. Newest is at the top and parents lie below, so an edge leaves the
//! commit's row and turns *down* into its parent's lane: a parent lane to the
//! right of the commit gets `╮` (connects left+down), one to the left gets `╭`
//! (connects right+down). A parent's lane is opened on the child's row and
//! closed as soon as the parent itself is laid out. Rows are padded to the
//! widest row, and a cell holds a single glyph — so the glyph is chosen from
//! the lane's WHOLE connection set (up/down/left/right), never from the corner
//! alone, or a rail running through a junction would look cut in two.
//!
//! ```text
//! ●╮   merge: the second parent opens lane 1 and turns down into it
//! ●│   the first-parent rail keeps running down lane 0
//! ├●   the second parent merges back: lane 0's rail runs on (├, not ╭)
//! ●    the common ancestor
//! ```

use std::collections::HashSet;

/// One commit of the log window, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphCommit {
    /// Short hash (the id other commits reference as a parent).
    pub hash: String,
    /// Short hashes of this commit's parents, in order.
    pub parents: Vec<String>,
}

/// A single glyph in one lane of one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphCell {
    /// The glyph to draw in this lane.
    pub glyph: &'static str,
    /// Lane color slot, `0..=7`: the renderer maps it to a theme color.
    pub color: u8,
}

/// One rendered row: the lanes occupied on that row, left to right.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphRow {
    /// Index into the `GraphCommit` slice this row was laid out from.
    pub commit: usize,
    /// Left-to-right lane cells; index 0 is the leftmost lane. Always at
    /// least one cell.
    pub cells: Vec<GraphCell>,
}

const DOT: &str = "●";
const PIPE: &str = "│";
const DASH: &str = "─";
/// A line arriving from the left / the right and turning down.
const CORNER_RIGHT_DOWN: &str = "╮";
const CORNER_LEFT_DOWN: &str = "╭";
/// The rarer upward turns, kept so no connection set is ever guessed wrong.
const CORNER_UP_RIGHT: &str = "╰";
const CORNER_UP_LEFT: &str = "╯";
/// A rail that keeps running while a horizontal joins it.
const TEE_RIGHT: &str = "├";
const TEE_LEFT: &str = "┤";
const TEE_DOWN: &str = "┬";
const TEE_UP: &str = "┴";
const CROSS: &str = "┼";
const BLANK: &str = " ";

/// Which ways the line in one lane leaves its cell: `up`/`down` are the
/// vertical rail, `left`/`right` the horizontal joins drawn on the commit's
/// row. Picking the glyph from the whole connection set is what keeps a rail
/// running through a junction instead of being cut by a bare corner.
#[derive(Clone, Copy, Default)]
struct Links {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
}

impl Links {
    /// The glyph for this connection set.
    fn glyph(self) -> &'static str {
        match (self.up, self.down, self.left, self.right) {
            (true, true, false, false) => PIPE,
            (true, true, false, true) => TEE_RIGHT,
            (true, true, true, false) => TEE_LEFT,
            (true, true, true, true) => CROSS,
            (true, false, true, true) => TEE_UP,
            (false, true, true, true) => TEE_DOWN,
            (false, true, false, true) => CORNER_LEFT_DOWN,
            (false, true, true, false) => CORNER_RIGHT_DOWN,
            (true, false, false, true) => CORNER_UP_RIGHT,
            (true, false, true, false) => CORNER_UP_LEFT,
            (false, false, true, true) => DASH,
            // A rail that runs on with no partner (or a lone horizontal stub)
            // still needs a glyph; only a fully free lane stays blank.
            (true, false, false, false) | (false, true, false, false) => PIPE,
            (false, false, true, false) | (false, false, false, true) => DASH,
            (false, false, false, false) => BLANK,
        }
    }
}

/// Lay out `commits` (newest first, as `git log` returns them) into rows.
pub fn layout(commits: &[GraphCommit]) -> Vec<GraphRow> {
    let present: HashSet<&str> = commits.iter().map(|c| c.hash.as_str()).collect();
    let mut lanes: Vec<Option<&str>> = Vec::new();
    let mut rows: Vec<GraphRow> = Vec::new();

    for (index, commit) in commits.iter().enumerate() {
        // The rails entering this row from above, read before the commit
        // consumes its own.
        let above: Vec<bool> = lanes.iter().map(|lane| lane.is_some()).collect();
        let own = match find_lane(&lanes, &commit.hash) {
            Some(lane) => {
                lanes[lane] = None;
                lane
            }
            None => {
                let lane = lanes
                    .iter()
                    .position(|l| l.is_none())
                    .unwrap_or(lanes.len());
                if lane == lanes.len() {
                    lanes.push(None);
                }
                lane
            }
        };

        let mut edges: Vec<usize> = Vec::new();
        for (order, parent) in commit.parents.iter().enumerate() {
            if !present.contains(parent.as_str()) {
                continue;
            }
            let lane = match find_lane(&lanes, parent) {
                Some(lane) => lane,
                None if order == 0 => own,
                None => open_lane_right(&mut lanes, own),
            };
            while lanes.len() <= lane {
                lanes.push(None);
            }
            lanes[lane] = Some(parent.as_str());
            edges.push(lane);
        }

        let width = lanes.len().max(own + 1);
        let mut links: Vec<Links> = (0..width)
            .map(|lane| Links {
                up: above.get(lane).copied().unwrap_or(false),
                down: lanes.get(lane).copied().flatten().is_some(),
                ..Links::default()
            })
            .collect();
        for &parent in &edges {
            if parent == own {
                continue;
            }
            let (lo, hi) = (own.min(parent), own.max(parent));
            for (offset, link) in links[lo..=hi].iter_mut().enumerate() {
                let lane = lo + offset;
                if lane == own {
                    continue;
                }
                if lane == parent {
                    // The turn: this lane's rail drops down out of the
                    // horizontal, so only the side it comes FROM is joined.
                    if parent > own {
                        link.left = true;
                    } else {
                        link.right = true;
                    }
                } else {
                    link.left = true;
                    link.right = true;
                }
            }
        }

        let glyphs: Vec<&'static str> = (0..width)
            .map(|lane| {
                if lane == own {
                    DOT
                } else {
                    links[lane].glyph()
                }
            })
            .collect();
        while matches!(lanes.last(), Some(None)) {
            lanes.pop();
        }
        rows.push(GraphRow {
            commit: index,
            cells: glyphs
                .into_iter()
                .enumerate()
                .map(|(lane, glyph)| cell(glyph, lane))
                .collect(),
        });
    }

    let width = rows.iter().map(|r| r.cells.len()).max().unwrap_or(0);
    for row in &mut rows {
        while row.cells.len() < width {
            row.cells.push(cell(BLANK, row.cells.len()));
        }
    }
    rows
}

/// The leftmost lane currently expecting `hash`, if any.
fn find_lane(lanes: &[Option<&str>], hash: &str) -> Option<usize> {
    lanes.iter().position(|l| *l == Some(hash))
}

/// The nearest free lane to the right of `from`, or a fresh one at the end.
fn open_lane_right(lanes: &mut Vec<Option<&str>>, from: usize) -> usize {
    if let Some(lane) = (from + 1..lanes.len()).find(|&lane| lanes[lane].is_none()) {
        lane
    } else {
        lanes.push(None);
        lanes.len() - 1
    }
}

/// A blank or glyph cell colored by its lane index modulo eight.
fn cell(glyph: &'static str, lane: usize) -> GraphCell {
    GraphCell {
        glyph,
        color: (lane % 8) as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gc(spec: &[(&str, &[&str])]) -> Vec<GraphCommit> {
        spec.iter()
            .map(|(hash, parents)| GraphCommit {
                hash: (*hash).to_string(),
                parents: parents.iter().map(|p| (*p).to_string()).collect(),
            })
            .collect()
    }

    fn rows_str(commits: &[GraphCommit]) -> Vec<String> {
        layout(commits)
            .iter()
            .map(|row| row.cells.iter().map(|c| c.glyph).collect())
            .collect()
    }

    #[test]
    fn empty_input_returns_no_rows() {
        assert!(layout(&[]).is_empty());
    }

    #[test]
    fn linear_history_is_a_single_column() {
        let commits = gc(&[("c", &["b"]), ("b", &["a"]), ("a", &[])]);
        assert_eq!(rows_str(&commits), ["●", "●", "●"]);
        assert!(layout(&commits).iter().all(|r| r.cells[0].color == 0));
    }

    #[test]
    fn merge_opens_and_closes_a_second_lane() {
        let commits = gc(&[("m", &["a", "b"]), ("a", &["r"]), ("b", &["r"]), ("r", &[])]);
        assert_eq!(rows_str(&commits), ["●╮", "●│", "├●", "● "]);
    }

    #[test]
    fn two_parallel_branches_stay_side_by_side() {
        let commits = gc(&[("a", &["x"]), ("b", &["y"]), ("x", &[]), ("y", &[])]);
        assert_eq!(rows_str(&commits), ["● ", "│●", "●│", " ●"]);
    }

    #[test]
    fn a_lone_root_commit_is_one_cell() {
        assert_eq!(rows_str(&gc(&[("a", &[])])), ["●"]);
    }

    #[test]
    fn truncated_parents_never_leave_a_dangling_lane() {
        let commits = gc(&[("m", &["gone1", "gone2"])]);
        assert_eq!(rows_str(&commits), ["●"]);
        let mixed = gc(&[("x", &["a", "gone"]), ("a", &[])]);
        assert_eq!(rows_str(&mixed), ["●", "●"]);
    }

    #[test]
    fn octopus_merge_opens_each_parent_lane() {
        let commits = gc(&[
            ("m", &["a", "b", "c"]),
            ("a", &["r"]),
            ("b", &["r"]),
            ("c", &["r"]),
            ("r", &[]),
        ]);
        assert_eq!(rows_str(&commits), ["●┬╮", "●││", "├●│", "├─●", "●  "]);
    }

    #[test]
    fn freed_lanes_are_reused_so_the_width_stays_compact() {
        let commits = gc(&[
            ("a", &["x"]),
            ("b", &["y"]),
            ("x", &[]),
            ("y", &[]),
            ("c", &["z"]),
            ("z", &[]),
        ]);
        let rows = layout(&commits);
        assert_eq!(
            rows.iter().map(|r| r.cells.len()).max(),
            Some(2),
            "a freed two-lane window must not widen for the next tip"
        );
        assert_eq!(rows_str(&commits), ["● ", "│●", "●│", " ●", "● ", "● "]);
        assert_eq!(rows[4].cells[0].color, rows[0].cells[0].color);
    }

    #[test]
    fn every_row_is_padded_to_the_same_width() {
        let commits = gc(&[("m", &["a", "b"]), ("a", &["r"]), ("b", &["r"]), ("r", &[])]);
        let rows = layout(&commits);
        let width = rows[0].cells.len();
        assert!(width >= 1);
        assert!(rows.iter().all(|r| r.cells.len() == width));
        assert!(rows.iter().enumerate().all(|(i, r)| r.commit == i));
    }

    #[test]
    fn colors_track_lane_index_modulo_eight() {
        let parents: Vec<&str> = vec!["a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8"];
        let mut spec: Vec<(&str, &[&str])> = vec![("m", &parents)];
        for parent in &parents {
            spec.push((*parent, &[]));
        }
        let rows = layout(&gc(&spec));
        assert_eq!(rows[0].cells.len(), 9);
        assert_eq!(
            rows[0].cells.iter().map(|c| c.color).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4, 5, 6, 7, 0]
        );
    }

    #[test]
    fn layout_is_deterministic() {
        let commits = gc(&[
            ("m", &["a", "b", "c"]),
            ("a", &["r"]),
            ("b", &["r"]),
            ("c", &["r"]),
            ("r", &[]),
        ]);
        assert_eq!(layout(&commits), layout(&commits));
    }
}
