//! Compact Mermaid rendering for terminal TUI.
//!
//! Terminals do not have a portable Mermaid canvas. This component renders the
//! useful structure of common flowcharts and sequence diagrams as aligned
//! Unicode text, and falls back to a readable source panel for unsupported
//! Mermaid constructs.

use std::any::Any;
use std::collections::HashMap;

use super::component::Component;
use super::theme::theme;
use super::utils::wrap_text_with_ansi;

pub struct Mermaid {
    source: String,
    padding_x: usize,
}

impl Mermaid {
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            padding_x: 1,
        }
    }

    fn body_lines(&self, width: usize) -> Vec<String> {
        let source = self.source.trim();
        let mut lines = Vec::new();
        let first = source.lines().next().unwrap_or_default().trim();
        let is_sequence = first.eq_ignore_ascii_case("sequencediagram");
        let is_flow = first.eq_ignore_ascii_case("graph")
            || first.to_ascii_lowercase().starts_with("graph ")
            || first.eq_ignore_ascii_case("flowchart")
            || first.to_ascii_lowercase().starts_with("flowchart ");

        if is_sequence {
            lines.extend(self.render_sequence(source));
        } else if is_flow {
            lines.extend(self.render_flow(source));
        }

        if lines.is_empty() {
            lines = source
                .lines()
                .skip_while(|line| {
                    let trimmed = line.trim().to_ascii_lowercase();
                    trimmed == "mermaid"
                        || trimmed == "graph"
                        || trimmed.starts_with("graph ")
                        || trimmed == "flowchart"
                        || trimmed.starts_with("flowchart ")
                        || trimmed == "sequencediagram"
                })
                .map(|line| format!("  {line}"))
                .collect();
        }

        let max_width = width.saturating_sub(self.padding_x * 2).max(1);
        lines
            .into_iter()
            .flat_map(|line| wrap_text_with_ansi(&line, max_width))
            .collect()
    }

    fn render_flow(&self, source: &str) -> Vec<String> {
        let edges: Vec<_> = source
            .lines()
            .skip(1)
            .map(str::trim)
            .filter(|line| {
                !line.is_empty()
                    && !line.starts_with("%%")
                    && !line.starts_with("class")
                    && !line.starts_with("style")
                    && !line.starts_with("linkStyle")
            })
            .filter_map(parse_flow_edge)
            .collect();

        // Mermaid lets later edges refer to a node by its identifier alone
        // (`B --> C` after `A --> B[Ready]`). Preserve the readable node label
        // across those references instead of showing the implementation ID.
        let mut labels = HashMap::new();
        for (left, _, _, right) in &edges {
            for raw_node in [*left, *right] {
                let id = node_id(raw_node);
                let label = node_label(raw_node);
                if !id.is_empty() && !label.is_empty() && id != label {
                    labels.insert(id, label);
                }
            }
        }

        edges
            .into_iter()
            .filter_map(|(left, arrow, edge_label, right)| {
                let left = labels
                    .get(&node_id(left))
                    .cloned()
                    .unwrap_or_else(|| node_label(left));
                let right = labels
                    .get(&node_id(right))
                    .cloned()
                    .unwrap_or_else(|| node_label(right));
                if left.is_empty() || right.is_empty() {
                    return None;
                }
                let label_suffix = edge_label
                    .filter(|label| !label.is_empty())
                    .map(|label| format!(" ({label})"))
                    .unwrap_or_default();
                Some(format!("  {left} {arrow}{label_suffix} {right}"))
            })
            .collect()
    }

    fn render_sequence(&self, source: &str) -> Vec<String> {
        let mut out = Vec::new();
        for raw in source.lines().skip(1) {
            let line = raw.trim();
            if line.is_empty() || line.starts_with("%%") || line.starts_with("participant ") {
                continue;
            }
            let Some((left, rest)) = line.split_once("->>") else {
                continue;
            };
            let Some((right, message)) = rest.split_once(':') else {
                continue;
            };
            out.push(format!(
                "  {} ──▶ {}: {}",
                left.trim(),
                right.trim(),
                message.trim()
            ));
        }
        out
    }
}

/// Parse a common Mermaid flowchart edge. Mermaid permits edge labels in
/// `A -->|yes| B` form and several connector styles; terminal rendering keeps
/// the graph readable as one labelled edge per row.
fn parse_flow_edge(line: &str) -> Option<(&str, &'static str, Option<String>, &str)> {
    let (connector, arrow) = if line.contains("-.->") {
        ("-.->", "╌╌▶")
    } else if line.contains("==>") {
        ("==>", "══▶")
    } else if line.contains("-->") {
        ("-->", "──▶")
    } else if line.contains("---") {
        ("---", "───")
    } else {
        return None;
    };
    let (left, raw_right) = line.split_once(connector)?;
    let raw_right = raw_right.trim();
    // Standard Mermaid syntax: `A -->|approved| B`.
    if let Some(rest) = raw_right.strip_prefix('|') {
        let (label, right) = rest.split_once('|')?;
        return Some((
            left,
            arrow,
            Some(label.trim().trim_matches('"').to_string()),
            right,
        ));
    }
    // Retain compatibility with the original compact renderer's `B: label`
    // notation as it is useful in hand-authored terminal diagrams.
    if let Some((right, label)) = raw_right.split_once(':') {
        if !label.trim().is_empty() {
            return Some((
                left,
                arrow,
                Some(label.trim().trim_matches('"').to_string()),
                right,
            ));
        }
    }
    Some((left, arrow, None, raw_right))
}

/// Mermaid node identifier, before any shape delimiter. A bare node is both
/// its identifier and label.
fn node_id(raw: &str) -> String {
    raw.trim()
        .trim_matches(';')
        .trim()
        .split(['(', '[', '{'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn node_label(raw: &str) -> String {
    let raw = raw.trim().trim_matches(';').trim();
    let Some(open) = raw.find(['(', '[', '{']) else {
        return raw.to_string();
    };
    let close = match raw.as_bytes().get(open) {
        Some(b'(') => ')',
        Some(b'[') => ']',
        Some(b'{') => '}',
        _ => return raw.to_string(),
    };
    // `A((Start))` and `A([Start])` use nested delimiters. Use the final
    // matching delimiter and remove presentation-only wrapper punctuation.
    raw[open + 1..]
        .rfind(close)
        .map(|end| {
            raw[open + 1..open + 1 + end]
                .trim()
                .trim_matches(|c| matches!(c, '"' | '(' | ')' | '[' | ']' | '{' | '}'))
                .trim()
                .to_string()
        })
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| raw.to_string())
}

impl Component for Mermaid {
    fn render(&self, width: usize) -> Vec<String> {
        let colors = theme().colors;
        let panel_width = width.max(1).saturating_sub(self.padding_x * 2).max(1);
        let mut lines = Vec::new();
        lines.push(format!(
            "{}{}",
            " ".repeat(self.padding_x),
            colors.md_code_block_border.fg("mermaid")
        ));
        lines.extend(
            self.body_lines(panel_width)
                .into_iter()
                .map(|line| format!("{}{}", " ".repeat(self.padding_x), line)),
        );
        lines
    }

    fn invalidate(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strip_ansi;

    #[test]
    fn renders_flowchart_edges_as_terminal_arrows() {
        let output = strip_ansi(
            &Mermaid::new("flowchart LR\nA[Start] --> B{Done?}")
                .render(60)
                .join("\n"),
        );
        assert!(output.contains("Start ──▶ Done?"));
    }

    #[test]
    fn renders_labelled_and_styled_flowchart_edges() {
        let output = strip_ansi(
            &Mermaid::new(
                "flowchart TD\nA((Start)) -->|continue| B{Ready?}\nB -.->|no| C[Retry]\nB ==> D([Done])",
            )
            .render(60)
            .join("\n"),
        );
        assert!(output.contains("Start ──▶ (continue) Ready?"), "{output}");
        assert!(output.contains("Ready? ╌╌▶ (no) Retry"), "{output}");
        assert!(output.contains("Ready? ══▶ Done"), "{output}");
    }

    #[test]
    fn long_flowchart_edges_wrap_instead_of_eliding() {
        let output = strip_ansi(
            &Mermaid::new(
                "flowchart LR\nA[Very long source node] --> B[Very long destination node]",
            )
            .render(20)
            .join("\n"),
        );
        let compact: String = output.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(compact.contains("Verylongsourcenode"), "{output}");
        assert!(compact.contains("Verylongdestinationnode"), "{output}");
    }

    #[test]
    fn renders_sequence_messages() {
        let output = strip_ansi(
            &Mermaid::new("sequenceDiagram\nAlice->>Bob: Hello")
                .render(60)
                .join("\n"),
        );
        assert!(output.contains("Alice ──▶ Bob: Hello"));
    }

    #[test]
    fn falls_back_to_source_for_unknown_diagrams() {
        let output = strip_ansi(&Mermaid::new("pie\n  \"A\" : 50").render(60).join("\n"));
        assert!(output.contains("\"A\" : 50"));
    }
}
