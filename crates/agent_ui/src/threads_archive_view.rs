//! Shared helpers for the sidebar's thread history list: fuzzy title matching
//! and age formatting. The dedicated archive view surface was merged into the
//! sidebar list.

use chrono::{DateTime, Utc};

pub fn fuzzy_match_positions(query: &str, candidate: &str) -> Option<Vec<usize>> {
    let query_chars: Vec<char> = query.chars().collect();
    if query_chars.is_empty() {
        return Some(Vec::new());
    }

    let candidate_chars: Vec<(usize, char)> = candidate.char_indices().collect();
    let window_count = candidate_chars.len().checked_sub(query_chars.len() - 1)?;

    'outer: for window_start in 0..window_count {
        for (qi, &query_char) in query_chars.iter().enumerate() {
            let (_, cand_char) = candidate_chars[window_start + qi];
            if !cand_char.eq_ignore_ascii_case(&query_char) {
                continue 'outer;
            }
        }
        return Some(
            (0..query_chars.len())
                .map(|qi| candidate_chars[window_start + qi].0)
                .collect(),
        );
    }

    None
}

pub fn format_history_entry_timestamp(entry_time: DateTime<Utc>) -> String {
    format_age(Utc::now(), entry_time)
}

/// One terse age token: `5m`, `2h`, `3d`, `2w`, `4mo`, `2y`. Times at or after
/// `now` (the empty-draft pin sorts with a future timestamp) read as `1m`.
pub fn format_age(now: DateTime<Utc>, entry_time: DateTime<Utc>) -> String {
    let duration = now.signed_duration_since(entry_time);

    let minutes = duration.num_minutes();
    let hours = duration.num_hours();
    let days = duration.num_days();
    let weeks = days / 7;
    let months = days / 30;
    let years = days / 365;

    if minutes < 60 {
        format!("{}m", minutes.max(1))
    } else if hours < 24 {
        format!("{}h", hours)
    } else if days < 7 {
        format!("{}d", days)
    } else if weeks < 4 {
        format!("{}w", weeks)
    } else if years < 1 {
        format!("{}mo", months.max(1))
    } else {
        format!("{}y", years)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fuzzy_match_positions_returns_byte_indices() {
        // "🔥abc" — the fire emoji is 4 bytes, so 'a' starts at byte 4, 'b' at 5, 'c' at 6.
        let text = "🔥abc";
        let positions = fuzzy_match_positions("ab", text).expect("should match");
        assert_eq!(positions, vec![4, 5]);

        // Verify positions are valid char boundaries (this is the assertion that
        // panicked before the fix).
        for &pos in &positions {
            assert!(
                text.is_char_boundary(pos),
                "position {pos} is not a valid UTF-8 boundary in {text:?}"
            );
        }
    }

    #[test]
    fn test_fuzzy_match_positions_ascii_still_works() {
        let positions = fuzzy_match_positions("he", "hello").expect("should match");
        assert_eq!(positions, vec![0, 1]);
    }

    #[test]
    fn test_fuzzy_match_positions_case_insensitive() {
        let positions = fuzzy_match_positions("HE", "hello").expect("should match");
        assert_eq!(positions, vec![0, 1]);
    }

    #[test]
    fn test_fuzzy_match_positions_no_match() {
        assert!(fuzzy_match_positions("xyz", "hello").is_none());
    }

    #[test]
    fn test_fuzzy_match_positions_multi_byte_interior() {
        // "café" — 'é' is 2 bytes (0xC3 0xA9), so 'f' starts at byte 4, 'é' at byte 5.
        let text = "café";
        let positions = fuzzy_match_positions("fé", text).expect("should match");
        // 'c'=0, 'a'=1, 'f'=2, 'é'=3..4 — wait, let's verify:
        // Actually: c=1 byte, a=1 byte, f=1 byte, é=2 bytes
        // So byte positions: c=0, a=1, f=2, é=3
        assert_eq!(positions, vec![2, 3]);
        for &pos in &positions {
            assert!(
                text.is_char_boundary(pos),
                "position {pos} is not a valid UTF-8 boundary in {text:?}"
            );
        }
    }
}
