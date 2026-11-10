use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

#[test]
fn line_spans_take_both_spellings() {
    let span = |v: Value, n| line_span(v.as_object().unwrap(), n);
    assert_eq!(span(json!({}), 10), None);
    assert_eq!(
        span(json!({"start_line": 3, "end_line": 5}), 10),
        Some((3, 5))
    );
    assert_eq!(span(json!({"start_line": 8}), 10), Some((8, 10)));
    assert_eq!(span(json!({"end_line": 99}), 10), Some((1, 10)));
    assert_eq!(span(json!({"offset": 4, "limit": 2}), 10), Some((4, 5)));
    assert_eq!(span(json!({"limit": 3}), 10), Some((1, 3)));
    assert_eq!(
        span(json!({"start_line": 20, "end_line": 25}), 10),
        Some((10, 10))
    );
    assert_eq!(
        span(json!({"start_line": "2", "end_line": "x"}), 10),
        Some((2, 10))
    );
    assert_eq!(span(json!({"start_line": 0}), 10), Some((1, 10)));
}
