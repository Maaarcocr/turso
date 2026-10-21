use super::{ComparisonResult, parse_expected_rows};
use std::collections::HashMap;

/// Compare rows as multisets: the order is free, but every row must appear
/// as many times as expected.
pub fn compare(actual: &[Vec<String>], expected_lines: &[String]) -> ComparisonResult {
    let expected_rows = parse_expected_rows(expected_lines);

    // Occurrences in the actual rows minus occurrences in the expected rows.
    let mut balance: HashMap<String, i64> = HashMap::new();
    for row in actual {
        *balance.entry(row.join("|")).or_default() += 1;
    }
    for row in &expected_rows {
        *balance.entry(row.join("|")).or_default() -= 1;
    }
    if balance.values().all(|count| *count == 0) {
        return ComparisonResult::Match;
    }

    let mut rows: Vec<(String, i64)> = balance.into_iter().filter(|(_, c)| *c != 0).collect();
    rows.sort();
    let mut reason = String::new();
    for (heading, sign, missing) in [
        ("Missing rows:\n", '-', true),
        ("Extra rows:\n", '+', false),
    ] {
        let mut lines = rows
            .iter()
            .filter(|(_, count)| (*count < 0) == missing)
            .flat_map(|(row, count)| std::iter::repeat_n(row, count.unsigned_abs() as usize))
            .peekable();
        if lines.peek().is_some() {
            reason.push_str(heading);
            for row in lines {
                reason.push_str(&format!("  {sign} {row}\n"));
            }
        }
    }

    ComparisonResult::mismatch(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unordered_empty() {
        let actual: Vec<Vec<String>> = vec![];
        let expected: Vec<String> = vec![];
        assert!(compare(&actual, &expected).is_match());
    }

    #[test]
    fn test_unordered_same_order() {
        let actual = vec![vec!["1".to_string()], vec!["2".to_string()]];
        let expected = vec!["1".to_string(), "2".to_string()];
        assert!(compare(&actual, &expected).is_match());
    }

    #[test]
    fn test_unordered_different_order() {
        let actual = vec![vec!["2".to_string()], vec!["1".to_string()]];
        let expected = vec!["1".to_string(), "2".to_string()];
        assert!(compare(&actual, &expected).is_match());
    }

    #[test]
    fn test_unordered_multi_column() {
        let actual = vec![
            vec!["2".to_string(), "Bob".to_string()],
            vec!["1".to_string(), "Alice".to_string()],
        ];
        let expected = vec!["1|Alice".to_string(), "2|Bob".to_string()];
        assert!(compare(&actual, &expected).is_match());
    }

    #[test]
    fn test_unordered_missing_row() {
        let actual = vec![vec!["1".to_string()]];
        let expected = vec!["1".to_string(), "2".to_string()];

        let result = compare(&actual, &expected);
        assert!(!result.is_match());

        if let ComparisonResult::Mismatch { reason } = result {
            assert!(reason.contains("Missing rows"));
            assert!(reason.contains("2"));
        }
    }

    #[test]
    fn test_unordered_extra_row() {
        let actual = vec![
            vec!["1".to_string()],
            vec!["2".to_string()],
            vec!["3".to_string()],
        ];
        let expected = vec!["1".to_string(), "2".to_string()];

        let result = compare(&actual, &expected);
        assert!(!result.is_match());

        if let ComparisonResult::Mismatch { reason } = result {
            assert!(reason.contains("Extra rows"));
            assert!(reason.contains("3"));
        }
    }

    #[test]
    fn test_unordered_counts_duplicate_rows() {
        let rows = |values: &[&str]| -> Vec<Vec<String>> {
            values.iter().map(|v| vec![v.to_string()]).collect()
        };
        let expected = vec!["1".to_string(), "1".to_string(), "2".to_string()];
        assert!(compare(&rows(&["1", "2", "1"]), &expected).is_match());

        let result = compare(&rows(&["1", "2"]), &expected);
        assert_eq!(
            result,
            ComparisonResult::mismatch("Missing rows:\n  - 1\n"),
            "a missing duplicate is a mismatch"
        );

        let result = compare(&rows(&["1", "1", "1", "2", "2"]), &expected);
        assert_eq!(
            result,
            ComparisonResult::mismatch("Extra rows:\n  + 1\n  + 2\n"),
            "an extra duplicate is a mismatch"
        );
    }

    #[test]
    fn test_unordered_wrong_value() {
        let actual = vec![vec!["1".to_string()], vec!["3".to_string()]];
        let expected = vec!["1".to_string(), "2".to_string()];

        let result = compare(&actual, &expected);
        assert!(!result.is_match());

        if let ComparisonResult::Mismatch { reason } = result {
            assert!(reason.contains("Missing rows"));
            assert!(reason.contains("2"));
            assert!(reason.contains("Extra rows"));
            assert!(reason.contains("3"));
        }
    }
}
