//! Fuzzy matching utilities.
//!
//! Matches if all query characters appear in order (not necessarily consecutive).
//! Lower score = better match.

/// Result of a fuzzy match.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FuzzyMatch {
    /// Whether the pattern matches.
    pub matches: bool,
    /// Match score (lower is better).
    pub score: i32,
}

impl FuzzyMatch {
    /// Create a non-matching result.
    pub fn no_match() -> Self {
        Self {
            matches: false,
            score: 0,
        }
    }

    /// Create a matching result with the given score.
    pub fn with_score(score: i32) -> Self {
        Self {
            matches: true,
            score,
        }
    }
}

/// Perform fuzzy matching between a query and text.
/// Returns a FuzzyMatch with the result and score.
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let query_lower = query.to_lowercase();
    let text_lower = text.to_lowercase();

    // Try primary match first
    let primary_match = match_query(&query_lower, &text_lower);
    if primary_match.matches {
        return primary_match;
    }

    // Try swapped alphanumeric query (e.g., "2f" for "f2")
    if let Some(swapped) = swap_alphanumeric(&query_lower) {
        let swapped_match = match_query(&swapped, &text_lower);
        if swapped_match.matches {
            return FuzzyMatch::with_score(swapped_match.score + 5);
        }
    }

    primary_match
}

/// Internal matching function.
fn match_query(query: &str, text: &str) -> FuzzyMatch {
    if query.is_empty() {
        return FuzzyMatch::with_score(0);
    }

    if query.len() > text.len() {
        return FuzzyMatch::no_match();
    }

    let mut query_index = 0;
    let mut score: i32 = 0;
    let mut last_match_index: i32 = -1;
    let mut consecutive_matches = 0;

    let text_chars: Vec<char> = text.chars().collect();
    let query_chars: Vec<char> = query.chars().collect();

    for (i, &text_char) in text_chars.iter().enumerate() {
        if query_index >= query_chars.len() {
            break;
        }

        if text_char == query_chars[query_index] {
            // Check for word boundary
            let is_word_boundary = i == 0 || {
                let prev_char = text_chars[i - 1];
                is_word_boundary_char(prev_char)
            };

            // Reward consecutive matches
            if last_match_index >= 0 && i as i32 == last_match_index + 1 {
                consecutive_matches += 1;
                score -= consecutive_matches * 5;
            } else {
                consecutive_matches = 0;
                // Penalize gaps
                if last_match_index >= 0 {
                    score += (i as i32 - last_match_index - 1) * 2;
                }
            }

            // Reward word boundary matches
            if is_word_boundary {
                score -= 10;
            }

            // Slight penalty for later matches
            score += (i as f64 * 0.1) as i32;

            last_match_index = i as i32;
            query_index += 1;
        }
    }

    if query_index < query_chars.len() {
        return FuzzyMatch::no_match();
    }

    // Bonus for exact match
    if query == text {
        score -= 100;
    }

    FuzzyMatch::with_score(score)
}

/// Check if a character is a word boundary.
fn is_word_boundary_char(c: char) -> bool {
    c.is_whitespace() || matches!(c, '-' | '_' | '.' | '/' | ':')
}

/// Swap alphanumeric parts (e.g., "f2" -> "2f", "2file" -> "file2").
fn swap_alphanumeric(query: &str) -> Option<String> {
    let chars: Vec<char> = query.chars().collect();

    // Find the split point between letters and digits
    let mut letter_end = 0;
    let mut digit_start = 0;

    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_lowercase() {
            letter_end = i + 1;
        } else if c.is_ascii_digit() {
            digit_start = i;
            break;
        }
    }

    // Check for pattern: letters followed by digits
    if letter_end > 0 && digit_start >= letter_end {
        let letters: String = chars[..letter_end].iter().collect();
        let digits: String = chars[digit_start..].iter().collect();
        return Some(format!("{}{}", digits, letters));
    }

    // Check for pattern: digits followed by letters
    let mut digit_end = 0;
    let mut letter_start = 0;

    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_digit() {
            digit_end = i + 1;
        } else if c.is_ascii_lowercase() {
            letter_start = i;
            break;
        }
    }

    if digit_end > 0 && letter_start >= digit_end {
        let digits: String = chars[..digit_end].iter().collect();
        let letters: String = chars[letter_start..].iter().collect();
        return Some(format!("{}{}", letters, digits));
    }

    None
}

/// Filter and sort items by fuzzy match quality (best matches first).
/// Supports whitespace- and slash-separated tokens: all tokens must match.
pub fn fuzzy_filter<T, F>(items: &[T], query: &str, get_text: F) -> Vec<T>
where
    T: Clone,
    F: Fn(&T) -> &str,
{
    let query = query.trim();
    if query.is_empty() {
        return items.to_vec();
    }

    // Split query into tokens
    let tokens: Vec<&str> = query
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|t| !t.is_empty())
        .collect();

    if tokens.is_empty() {
        return items.to_vec();
    }

    let mut results: Vec<(T, i32)> = Vec::new();

    for item in items {
        let text = get_text(item);
        let mut total_score: i32 = 0;
        let mut all_match = true;

        for token in &tokens {
            let match_result = fuzzy_match(token, text);
            if match_result.matches {
                total_score += match_result.score;
            } else {
                all_match = false;
                break;
            }
        }

        if all_match {
            results.push((item.clone(), total_score));
        }
    }

    // Sort by score (lower is better)
    results.sort_by_key(|(_, score)| *score);
    results.into_iter().map(|(item, _)| item).collect()
}

/// Filter items with owned strings.
pub fn fuzzy_filter_owned<T, F>(items: Vec<T>, query: &str, get_text: F) -> Vec<T>
where
    T: Clone,
    F: Fn(&T) -> String,
{
    let query = query.trim();
    if query.is_empty() {
        return items;
    }

    let tokens: Vec<&str> = query
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|t| !t.is_empty())
        .collect();

    if tokens.is_empty() {
        return items;
    }

    let mut results: Vec<(T, i32)> = Vec::new();

    for item in items {
        let text = get_text(&item);
        let mut total_score: i32 = 0;
        let mut all_match = true;

        for token in &tokens {
            let match_result = fuzzy_match(token, &text);
            if match_result.matches {
                total_score += match_result.score;
            } else {
                all_match = false;
                break;
            }
        }

        if all_match {
            results.push((item, total_score));
        }
    }

    results.sort_by_key(|(_, score)| *score);
    results.into_iter().map(|(item, _)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fuzzy_match_exact() {
        let result = fuzzy_match("hello", "hello world");
        assert!(result.matches);
        assert!(result.score < 0); // Should have a negative (good) score
    }

    #[test]
    fn test_fuzzy_match_partial() {
        let result = fuzzy_match("hlo", "hello");
        assert!(result.matches);
    }

    #[test]
    fn test_fuzzy_match_no_match() {
        let result = fuzzy_match("xyz", "hello");
        assert!(!result.matches);
    }

    #[test]
    fn test_fuzzy_match_word_boundary() {
        let result1 = fuzzy_match("hw", "hello world");
        let result2 = fuzzy_match("hw", "helloworld");
        assert!(result1.matches);
        assert!(result2.matches);
        // Word boundary match should have better (lower) score
        assert!(result1.score < result2.score);
    }

    #[test]
    fn test_fuzzy_filter() {
        let items = vec!["hello world", "goodbye world", "hello there"];
        let filtered = fuzzy_filter(&items, "hw", |s| *s);
        assert!(!filtered.is_empty());
        assert!(filtered.contains(&"hello world"));
    }

    #[test]
    fn test_fuzzy_filter_multiple_tokens() {
        let items = vec![
            "src/main.rs",
            "src/lib.rs",
            "test/main.rs",
            "docs/readme.md",
        ];
        let filtered = fuzzy_filter(&items, "src main", |s| *s);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0], "src/main.rs");
    }

    #[test]
    fn test_fuzzy_match_case_insensitive() {
        let result = fuzzy_match("HELLO", "hello world");
        assert!(result.matches);
    }

    #[test]
    fn test_swap_alphanumeric() {
        assert_eq!(swap_alphanumeric("f2"), Some("2f".to_string()));
        assert_eq!(swap_alphanumeric("2file"), Some("file2".to_string()));
        assert_eq!(swap_alphanumeric("abc"), None);
    }
}
