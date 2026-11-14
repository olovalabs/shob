//! Session search — mirrors `packages/agent/src/harness/session/search-backend.ts`.
//!
//! Provides full-text search across session entries. Searches user and assistant
//! message content, tool calls, and custom entries. Returns ranked results with
//! context snippets.
//!
//! # Use Cases
//!
//! - **Session picker**: Search across all sessions to find a specific conversation.
//! - **Content discovery**: Find where a topic was discussed across sessions.
//! - **Audit**: Locate specific tool calls or decisions in session history.
//!
//! # Example
//!
//! ```rust,no_run
//! use rpi_harness::session::search::{SessionSearch, SessionSearchOptions};
//! use rpi_harness::session::types::{SessionMetadata, Entry};
//! use std::collections::HashMap;
//!
//! # async fn example() {
//! let search = SessionSearch::new();
//! let sessions: HashMap<String, (SessionMetadata, Vec<Entry>)> = HashMap::new();
//!
//! let results = search.search(
//!     &sessions,
//!     "database migration",
//!     SessionSearchOptions {
//!         limit: Some(10),
//!         include_tool_calls: true,
//!         include_custom_entries: false,
//!     }
//! ).await;
//!
//! for hit in results {
//!     println!("Session {}: {}", hit.session_id, hit.snippet);
//! }
//! # }
//! ```
//! ```

use std::collections::HashMap;

use rpi_agent::message::AgentMessage;
use rpi_ai::types::Content;

use super::types::{Entry, SessionMetadata};

/// Options for a session search.
#[derive(Debug, Clone, Default)]
pub struct SessionSearchOptions {
    /// Maximum number of results to return. `None` means unlimited.
    pub limit: Option<usize>,
    /// Whether to include tool call names and arguments in the search.
    pub include_tool_calls: bool,
    /// Whether to include custom entry data in the search.
    pub include_custom_entries: bool,
}

/// A search result hit.
#[derive(Debug, Clone)]
pub struct SessionSearchHit {
    /// The session id where the hit was found.
    pub session_id: String,
    /// The entry id where the hit was found.
    pub entry_id: String,
    /// The role of the message (user, assistant, or system).
    pub role: String,
    /// A snippet of text around the match.
    pub snippet: String,
    /// The position of the match in the full text (character offset).
    pub match_position: usize,
    /// Relevance score (higher is better).
    pub score: f64,
}

/// Session search engine.
pub struct SessionSearch;

impl SessionSearch {
    /// Create a new session search engine.
    pub fn new() -> Self {
        Self
    }

    /// Search across multiple sessions for a query string.
    ///
    /// # Arguments
    ///
    /// * `sessions` - Map of session metadata to session entries.
    /// * `query` - The search query (case-insensitive).
    /// * `options` - Search options (limit, what to include, etc.).
    ///
    /// # Returns
    ///
    /// A list of search hits, sorted by relevance score (highest first).
    pub async fn search(
        &self,
        sessions: &HashMap<String, (SessionMetadata, Vec<Entry>)>,
        query: &str,
        options: SessionSearchOptions,
    ) -> Vec<SessionSearchHit> {
        let query_lower = query.to_lowercase();
        let mut hits = Vec::new();

        for (session_id, (_metadata, entries)) in sessions {
            for entry in entries {
                if let Some(hit) = self.search_entry(
                    session_id,
                    entry,
                    &query_lower,
                    options.include_tool_calls,
                    options.include_custom_entries,
                ) {
                    hits.push(hit);
                }
            }
        }

        // Sort by score (highest first)
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Apply limit
        if let Some(limit) = options.limit {
            hits.truncate(limit);
        }

        hits
    }

    /// Search a single entry for the query.
    fn search_entry(
        &self,
        session_id: &str,
        entry: &Entry,
        query_lower: &str,
        include_tool_calls: bool,
        include_custom_entries: bool,
    ) -> Option<SessionSearchHit> {
        match entry {
            Entry::Message(msg_entry) => {
                let role = match &msg_entry.message {
                    AgentMessage::User(_) => "user",
                    AgentMessage::Assistant(_) => "assistant",
                    AgentMessage::ToolResult(_) => "tool_result",
                    AgentMessage::Custom(_) => return None,
                };

                let text = self.extract_message_text(&msg_entry.message, include_tool_calls);
                self.find_match(session_id, &msg_entry.base.id, role, &text, query_lower)
            }
            Entry::Custom(custom_entry) if include_custom_entries => {
                let text = serde_json::to_string(&custom_entry.data).unwrap_or_default();
                self.find_match(
                    session_id,
                    &custom_entry.base.id,
                    "custom",
                    &text,
                    query_lower,
                )
            }
            _ => None,
        }
    }

    /// Extract searchable text from a message.
    fn extract_message_text(&self, message: &AgentMessage, include_tool_calls: bool) -> String {
        match message {
            AgentMessage::User(user_msg) => {
                // User messages can be text or blocks
                match &user_msg.content {
                    rpi_ai::types::UserContent::Text(text) => text.clone(),
                    rpi_ai::types::UserContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|block| match block {
                            Content::Text(t) => Some(t.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                }
            }
            AgentMessage::Assistant(assistant_msg) => {
                let mut parts: Vec<String> = Vec::new();
                for content in &assistant_msg.content {
                    match content {
                        Content::Text(t) => parts.push(t.text.clone()),
                        Content::ToolCall(tc) if include_tool_calls => {
                            parts.push(tc.name.clone());
                            // Tool-call arguments are JSON; include their string form so
                            // searches can match on argument values too.
                            if let Ok(args_str) = serde_json::to_string(&tc.arguments) {
                                parts.push(args_str);
                            }
                        }
                        _ => {}
                    }
                }
                parts.join("\n")
            }
            AgentMessage::ToolResult(tr) => {
                let mut parts = Vec::new();
                for content in &tr.content {
                    match content {
                        Content::Text(t) => parts.push(t.text.as_str()),
                        _ => {}
                    }
                }
                parts.join("\n")
            }
            AgentMessage::Custom(c) => serde_json::to_string(&c.data).unwrap_or_default(),
        }
    }

    /// Find a match in the text and create a hit.
    fn find_match(
        &self,
        session_id: &str,
        entry_id: &str,
        role: &str,
        text: &str,
        query_lower: &str,
    ) -> Option<SessionSearchHit> {
        let text_lower = text.to_lowercase();
        if let Some(position) = text_lower.find(query_lower) {
            // Extract a snippet around the match
            let snippet = self.extract_snippet(text, position, query_lower.len());

            // Calculate a simple relevance score
            // More matches = higher score, earlier position = higher score
            let match_count = text_lower.matches(query_lower).count();
            let position_factor = 1.0 - (position as f64 / text.len() as f64).min(1.0);
            let score = (match_count as f64) * (0.5 + 0.5 * position_factor);

            Some(SessionSearchHit {
                session_id: session_id.to_string(),
                entry_id: entry_id.to_string(),
                role: role.to_string(),
                snippet,
                match_position: position,
                score,
            })
        } else {
            None
        }
    }

    /// Extract a snippet of text around the match position.
    fn extract_snippet(&self, text: &str, position: usize, match_len: usize) -> String {
        let context_chars = 100; // Show 100 chars before and after
        let start = position.saturating_sub(context_chars);
        let end = (position + match_len + context_chars).min(text.len());

        let mut snippet = String::new();
        if start > 0 {
            snippet.push_str("...");
        }
        snippet.push_str(&text[start..end]);
        if end < text.len() {
            snippet.push_str("...");
        }

        snippet
    }
}

impl Default for SessionSearch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ai::types::TextContentType;
    use rpi_ai::types::{AssistantMessage, TextContent, UserContent, UserMessage};

    use crate::session::types::{EntryBase, MessageEntry};

    fn create_user_entry(id: &str, text: &str) -> Entry {
        Entry::Message(MessageEntry {
            base: EntryBase {
                entry_type: "message".to_string(),
                id: id.to_string(),
                seq: 0,
                timestamp: 0,
                parent_id: None,
            },
            message: AgentMessage::User(UserMessage {
                role: Default::default(),
                content: UserContent::Text(text.to_string()),
                timestamp: 0,
            }),
            terminate: None,
        })
    }

    fn create_assistant_entry(id: &str, text: &str) -> Entry {
        Entry::Message(MessageEntry {
            base: EntryBase {
                entry_type: "message".to_string(),
                id: id.to_string(),
                seq: 0,
                timestamp: 0,
                parent_id: None,
            },
            message: AgentMessage::Assistant(Box::new(AssistantMessage {
                role: Default::default(),
                content: vec![Content::Text(TextContent {
                    kind: TextContentType,
                    text: text.to_string(),
                    text_signature: None,
                })],
                api: rpi_ai::types::Api::AnthropicMessages,
                provider: "test".to_string(),
                model: "test-model".to_string(),
                response_model: None,
                response_id: None,
                usage: rpi_ai::types::Usage::zero(),
                stop_reason: rpi_ai::types::StopReason::Stop,
                deferred: None,
                error_message: None,
                raw_stop_reason: None,
                end_turn: None,
                timestamp: 0,
            })),
            terminate: None,
        })
    }

    fn make_metadata(id: &str) -> SessionMetadata {
        SessionMetadata {
            id: id.to_string(),
            created_at: 0,
            parent_session_id: None,
        }
    }

    #[tokio::test]
    async fn search_finds_user_messages() {
        let search = SessionSearch::new();
        let mut sessions = HashMap::new();

        let metadata = make_metadata("session-1");
        let entries = vec![
            create_user_entry("e1", "Hello world"),
            create_user_entry("e2", "Database migration is important"),
        ];

        sessions.insert("session-1".to_string(), (metadata, entries));

        let results = search
            .search(
                &sessions,
                "database",
                SessionSearchOptions {
                    limit: None,
                    include_tool_calls: false,
                    include_custom_entries: false,
                },
            )
            .await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].session_id, "session-1");
        assert_eq!(results[0].entry_id, "e2");
        assert_eq!(results[0].role, "user");
        assert!(results[0].snippet.contains("Database"));
    }

    #[tokio::test]
    async fn search_finds_assistant_messages() {
        let search = SessionSearch::new();
        let mut sessions = HashMap::new();

        let metadata = make_metadata("session-1");
        let entries = vec![create_assistant_entry(
            "e1",
            "I can help with database migration",
        )];

        sessions.insert("session-1".to_string(), (metadata, entries));

        let results = search
            .search(
                &sessions,
                "migration",
                SessionSearchOptions {
                    limit: None,
                    include_tool_calls: false,
                    include_custom_entries: false,
                },
            )
            .await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].role, "assistant");
    }

    #[tokio::test]
    async fn search_respects_limit() {
        let search = SessionSearch::new();
        let mut sessions = HashMap::new();

        let metadata = make_metadata("session-1");
        let entries = vec![
            create_user_entry("e1", "test match 1"),
            create_user_entry("e2", "test match 2"),
            create_user_entry("e3", "test match 3"),
        ];

        sessions.insert("session-1".to_string(), (metadata, entries));

        let results = search
            .search(
                &sessions,
                "test",
                SessionSearchOptions {
                    limit: Some(2),
                    include_tool_calls: false,
                    include_custom_entries: false,
                },
            )
            .await;

        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn search_is_case_insensitive() {
        let search = SessionSearch::new();
        let mut sessions = HashMap::new();

        let metadata = make_metadata("session-1");
        let entries = vec![create_user_entry("e1", "DATABASE migration")];

        sessions.insert("session-1".to_string(), (metadata, entries));

        let results = search
            .search(
                &sessions,
                "database",
                SessionSearchOptions {
                    limit: None,
                    include_tool_calls: false,
                    include_custom_entries: false,
                },
            )
            .await;

        assert_eq!(results.len(), 1);
    }
}
