//! Portable styled output. No ratatui, no terminal: the view decides what a
//! span *means* and the shell maps that to colours (§2.4a, §2.5).

/// Structural weight. Resolution is theme-dependent (§2.5): under `inherit`
/// `Body`/`Emphasis` are the terminal's default foreground (`Emphasis` adds
/// bold, not a colour); under `lumon` they are that theme's explicit RGB.
/// `Label` and `Rule` are dim and reserved for decoration that may vanish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Role {
    #[default]
    Body,
    Emphasis,
    Label,
    Rule,
}

/// Advisory colour applied over a role. A terminal cell has one foreground, so
/// this replaces the role's colour rather than layering over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Semantic {
    Good,
    Warn,
    Bad,
    Accent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub role: Role,
    pub semantic: Option<Semantic>,
    /// Agent-mark override only (§2.5); `rgb` when the terminal reports
    /// truecolor, `ansi` (0–15) otherwise. Both are carried so the view stays
    /// free of capability detection.
    pub rgb: Option<(u8, u8, u8)>,
    pub ansi: Option<u8>,
    /// Reverse video, used only for the selected card's header (§3.2a).
    pub reverse: bool,
}

impl Style {
    pub fn role(role: Role) -> Self {
        Self {
            role,
            ..Self::default()
        }
    }

    pub fn semantic(role: Role, semantic: Semantic) -> Self {
        Self {
            role,
            semantic: Some(semantic),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

impl Span {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    pub fn body(text: impl Into<String>) -> Self {
        Self::new(text, Style::role(Role::Body))
    }

    pub fn label(text: impl Into<String>) -> Self {
        Self::new(text, Style::role(Role::Label))
    }

    pub fn emphasis(text: impl Into<String>) -> Self {
        Self::new(text, Style::role(Role::Emphasis))
    }
}

pub type Line = Vec<Span>;

use crate::sidebar::layout::LineSpan;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rendered {
    pub scrollable: Vec<Line>,
    /// Drawn below the scrolling region, in order: config notice, idle notice,
    /// key footer. Dropped from the TOP when the frame is short, so the footer
    /// survives longest (§2.4a).
    pub pinned: Vec<Line>,
    pub spans: Vec<(String, LineSpan)>,
}

impl Rendered {
    /// Plain-text projection, for readable golden failures.
    pub fn plain(&self) -> Vec<String> {
        self.scrollable
            .iter()
            .chain(self.pinned.iter())
            .map(|line| line.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }

    pub fn span_for(&self, pane_id: &str) -> Option<LineSpan> {
        self.spans
            .iter()
            .find(|(id, _)| id == pane_id)
            .map(|(_, s)| *s)
    }
}

/// Resolved per-agent appearance (§4.2), keyed by canonical agent id.
/// `BTreeMap`, not `HashMap`: §2.4a declares it, and iteration order decides
/// what a golden sees when the view walks appearances.
pub type AgentAppearances = std::collections::BTreeMap<String, AgentAppearance>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAppearance {
    pub label: String,
    pub rgb: (u8, u8, u8),
    pub ansi: u8,
    pub symbol: Option<String>,
}

/// `problems == 0` renders no notice at all (§4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConfigStatus {
    pub problems: usize,
    pub log_written: bool,
}
