//! Shared CLI formatting helpers reused across subcommand summaries.
//! Keep these as pure functions over primitives so they stay
//! unit-testable without a git repo or index.

/// Render a millisecond duration for human-readable summaries:
/// `>= 1000` shows as seconds with one decimal (e.g. `8.4s`), below
/// that as whole milliseconds (e.g. `420ms`). Used by both the
/// `ohara index` and `ohara plan` closing banners.
pub fn fmt_duration_ms(ms: u64) -> String {
    match ms >= 1000 {
        true => format!("{:.1}s", ms as f64 / 1000.0),
        false => format!("{ms}ms"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_duration_ms_renders_seconds_above_one_second() {
        assert_eq!(fmt_duration_ms(8_400), "8.4s");
        assert_eq!(fmt_duration_ms(1_000), "1.0s");
        assert_eq!(fmt_duration_ms(47_300), "47.3s");
    }

    #[test]
    fn fmt_duration_ms_renders_millis_below_one_second() {
        assert_eq!(fmt_duration_ms(420), "420ms");
        assert_eq!(fmt_duration_ms(0), "0ms");
        assert_eq!(fmt_duration_ms(999), "999ms");
    }
}
