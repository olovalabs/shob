//! Internal text component layout used by the TUI renderer.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PanelAnchor {
    TopLeft,
    TopCenter,
    #[default]
    TopRight,
    LeftCenter,
    Center,
    RightCenter,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PanelLayout {
    #[default]
    Overlay,
    Sidebar,
    Footer,
}

/// Theme colors for literal text highlights; terminal escapes are never accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PanelHighlightColor {
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct PanelHighlight {
    pub text: String,
    pub color: PanelHighlightColor,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionPanel {
    pub version: u32,
    #[serde(default)]
    pub order: i32,
    #[serde(default)]
    pub layout: PanelLayout,
    #[serde(default)]
    pub anchor: PanelAnchor,
    #[serde(default)]
    pub offset_x: i32,
    #[serde(default)]
    pub offset_y: i32,
    #[serde(default = "default_width")]
    pub width: usize,
    #[serde(default = "default_height")]
    pub max_height: usize,
    #[serde(default)]
    pub min_screen_width: usize,
    #[serde(default = "default_border")]
    pub border: bool,
    #[serde(default)]
    pub title: String,
    pub lines: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub highlights: Vec<PanelHighlight>,
}
fn default_width() -> usize {
    44
}
fn default_height() -> usize {
    16
}
fn default_border() -> bool {
    true
}
impl ExtensionPanel {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported panel version (expected 1)".into());
        }
        if !(4..=240).contains(&self.width)
            || !(1..=80).contains(&self.max_height)
            || self.min_screen_width > 1000
            || self.offset_x.unsigned_abs() > 1000
            || self.offset_y.unsigned_abs() > 1000
        {
            return Err("panel geometry is out of range".into());
        }
        if self.lines.len() > 64
            || self.lines.iter().any(|line| line.len() > 2048)
            || self.title.len() > 256
        {
            return Err("panel content exceeds limits".into());
        }
        if self.highlights.len() > 32
            || self.highlights.iter().any(|h| {
                h.text.is_empty() || h.text.len() > 256 || h.text.chars().any(char::is_control)
            })
        {
            return Err("invalid panel highlights".into());
        }
        Ok(())
    }
    /// Content updates retain the same overlay and its stacking position.
    pub fn same_geometry(&self, other: &Self) -> bool {
        self.layout == other.layout
            && self.anchor == other.anchor
            && self.offset_x == other.offset_x
            && self.offset_y == other.offset_y
            && self.width == other.width
            && self.max_height == other.max_height
    }
}
