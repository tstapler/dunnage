//! Size and share cells of the explorer (`tui/ux-explorer.md` 3.2): the same numbers in the TUI, `--text`
//! and `--json`. Pure functions of integers; no float is used for a decision.

const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

/// Widest size cell, in characters (C-10).
pub const SIZE_CELL_MAX: usize = 9;

/// Binary units with one decimal (`0 B`, `512 B`, `21.7 GiB`). A value whose rounded figure reaches 1000
/// steps up one unit (1000 GiB prints `1.0 TiB`), so a cell never exceeds [`SIZE_CELL_MAX`] characters.
pub fn fmt_size_cell(bytes: u64) -> String {
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let wide = u128::from(bytes) * 10;
    for (unit, name) in UNITS.iter().enumerate().skip(1) {
        let div = 1u128 << (10 * unit);
        // Tenths of this unit, rounded half up.
        let tenths = (wide + div / 2) / div;
        if tenths < 10_000 || unit + 1 == UNITS.len() {
            return format!("{}.{} {name}", tenths / 10, tenths % 10);
        }
    }
    unreachable!("the last unit always returns")
}

/// One SHARE cell: the bar's filled count and the percent text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareCell {
    pub filled: usize,
    pub bar_cells: usize,
    /// `65%`, `<1%` or `n/a`, without padding.
    pub percent: String,
}

impl ShareCell {
    /// `[###..]` with `#` filled and `.` empty (ASCII only, in every mode).
    pub fn bar(&self) -> String {
        let filled = self.filled.min(self.bar_cells);
        format!(
            "[{}{}]",
            "#".repeat(filled),
            ".".repeat(self.bar_cells - filled)
        )
    }

    /// The drawn cell: the bar, a space and the percent right-aligned in 4 characters
    /// (`[#####.....]  50%`, 17 wide at 10 cells; 12 at 5).
    pub fn text(&self) -> String {
        format!("{} {:>4}", self.bar(), self.percent)
    }
}

/// `usage / parent` as a bar of `bar_cells` cells and a percent. Filled cells are round-half-up(share x N)
/// with at least 1 when `usage > 0`; the percent is round-half-up, `<1%` below 0.5% and `n/a` when the
/// parent is empty (UX-T13).
pub fn share_cell(usage: u64, parent: u64, bar_cells: usize) -> ShareCell {
    if parent == 0 {
        return ShareCell {
            filled: 0,
            bar_cells,
            percent: "n/a".into(),
        };
    }
    let (u, p, n) = (u128::from(usage), u128::from(parent), bar_cells as u128);
    let mut filled = ((2 * u * n + p) / (2 * p)) as usize;
    if usage > 0 {
        filled = filled.max(1);
    }
    let percent = if u * 200 < p {
        "<1%".to_string()
    } else {
        format!("{}%", (200 * u + p) / (2 * p))
    };
    ShareCell {
        filled: filled.min(bar_cells),
        bar_cells,
        percent,
    }
}
