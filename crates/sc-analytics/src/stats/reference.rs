//! The values R recorded for the tests (`tests/r/test_reference.R`), and the
//! comparisons the tests make with them.

use serde_json::Value;

/// What `tests/r/test_reference.R` wrote, on R's own data sets.
pub fn reference() -> Value {
    serde_json::from_str(include_str!("../../tests/r/test_reference.json")).unwrap_or(Value::Null)
}

/// An array of numbers.
pub fn nums(v: &Value) -> Vec<f64> {
    v.as_array()
        .map(|a| a.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default()
}

/// Assert that `got` is `want` to within `tolerance`, relative to `want`'s
/// size when that is more than 1.
#[track_caller]
pub fn close(got: f64, want: f64, tolerance: f64, what: &str) {
    let scale = want.abs().max(1.0);
    assert!(
        (got - want).abs() <= tolerance * scale,
        "{what}: got {got}, R has {want} (difference {:e})",
        (got - want).abs()
    );
}
