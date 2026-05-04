//! Plan 12 — query understanding.
//!
//! `QueryIntent` is what the user wants ("how was X done before"
//! versus "where did we configure Y" versus "how did we fix Z"). The
//! deterministic parser in [`parse_query`] classifies a free-form
//! query string into one of a small closed set of intents and
//! extracts explicit filters (language hints, path tokens, quoted
//! symbol names, simple timeframe phrases) into a
//! [`ParsedQuery`].
//!
//! `RetrievalProfile` is what the indexer-side retriever does about
//! it: lane-weight nudges, rerank pool size, recency multiplier,
//! optional language / symbol / path filter overrides. Profiles are
//! deliberately conservative — the v0.7 lift is "don't ship a
//! profile that improves one case but regresses another", with the
//! plan-10 eval as the gate.
//!
//! No LLM dependency in v0.7 — the parser is rule-based and the
//! "what rules matched" trail is exposed in `ParsedQuery::matched_rules`
//! so debug output can show the decision-making.

use serde::{Deserialize, Serialize};

/// What the user is trying to find.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryIntent {
    /// "how have we done X before" / "add Y like the existing Z" /
    /// "is there a pattern for ABC". The default lift for the
    /// 'find_pattern' demo path.
    ImplementationPattern,
    /// "how did we fix the timeout bug" / "what was the original
    /// crash here". Recency-weighted because bug fixes are usually
    /// more relevant in their post-mortem window.
    BugFixPrecedent,
    /// "how is `Foo::bar` called" / "show usages of `retry`". Symbol
    /// lanes (historical + HEAD) carry the load.
    ApiUsage,
    /// "where do we configure CoreML" / "how is the database URL
    /// loaded". Configuration paths usually live in text/comments
    /// rather than tree-sitter symbols, so the text + semantic-text
    /// lanes carry the load.
    Configuration,
    /// Default arm. Profile is the existing lane-weight blend.
    Unknown,
}

/// Parser confidence in its intent classification. `Low` means
/// "matched the catch-all", `High` means "matched a specific
/// keyword pattern".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

/// Output of [`parse_query`]. Extracted filters override the
/// caller-supplied `PatternQuery.language` ONLY when the caller did
/// not set one — see `RetrievalProfile::apply_to_query` for the
/// precedence rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedQuery {
    pub intent: QueryIntent,
    pub confidence: Confidence,
    /// Language hint extracted from the query body (e.g. "in rust").
    pub language: Option<String>,
    /// Path-ish tokens like `src/foo.rs` (slash-bearing identifiers).
    pub path_terms: Vec<String>,
    /// Quoted identifiers like `'retry_with_backoff'` or
    /// `"fetch"` — usually a strong signal for the symbol lanes.
    pub symbol_terms: Vec<String>,
    /// Unix-second cutoff parsed from a relative phrase ("last 30
    /// days", "since last week") or an explicit `since:YYYY-MM-DD`.
    /// `None` means no recency bound.
    pub since_unix: Option<i64>,
    /// Names of the parser rules that fired (for debug output).
    pub matched_rules: Vec<String>,
}

/// Retrieval-side knob set produced from a `ParsedQuery`. Conservative
/// by design: the v0.7 default profile (Unknown intent) returns the
/// same lane behaviour as before plan 12. Variants nudge weights
/// rather than rewriting the lane set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalProfile {
    /// Stable name for response metadata (`_meta.query_profile.name`).
    pub name: String,
    /// Multiplicative nudge applied to the recency weight in
    /// `RankingWeights`. `1.0` = no change. Bug-fix queries set this
    /// above 1.0 because recent fixes are usually the ones the user
    /// wants to read.
    pub recency_multiplier: f32,
    /// Lane-mask flag. When `false`, the retriever skips this lane
    /// entirely. Defaults are all `true`.
    pub vec_lane_enabled: bool,
    /// As above for the BM25-by-(diff/semantic)-text lane.
    pub text_lane_enabled: bool,
    /// As above for the historical / HEAD symbol lane.
    pub symbol_lane_enabled: bool,
    /// Top-K to pull into the cross-encoder reranker. The retriever's
    /// existing default is `RankingWeights::rerank_top_k`; profiles
    /// override only when the intent benefits from a wider candidate
    /// pool (api-usage, configuration).
    pub rerank_top_k: Option<usize>,
    /// Override for `RankingWeights::recency_weight`. `None` means
    /// use the default from `RankingWeights`.
    pub recency_weight: Option<f32>,
    /// Override for `RankingWeights::recency_half_life_days`. `None`
    /// means use the default (90.0 days). Bug-fix profiles may lower
    /// this to surface very recent fixes faster.
    pub recency_half_life_days: Option<f32>,
    /// Override for `RankingWeights::lane_top_k`. `None` means use
    /// the default gather size. Wide-pool profiles may raise this.
    pub lane_top_k: Option<u8>,
    /// Human-readable explanation of the profile's pick. Surfaced in
    /// MCP `_meta` and CLI debug output.
    pub explanation: String,
}

impl RetrievalProfile {
    /// The "no-op" profile — same behaviour as the existing
    /// retriever before plan 12. Used for `QueryIntent::Unknown` and
    /// as the baseline that other profiles override from.
    pub fn default_unknown() -> Self {
        Self {
            name: "unknown".into(),
            recency_multiplier: 1.0,
            vec_lane_enabled: true,
            text_lane_enabled: true,
            symbol_lane_enabled: true,
            rerank_top_k: None,
            recency_weight: None,
            recency_half_life_days: None,
            lane_top_k: None,
            explanation: "No specific intent detected — using default lane blend.".into(),
        }
    }

    /// Profile for `BugFixPrecedent`. Bumps recency.
    pub fn bug_fix() -> Self {
        Self {
            name: "bug_fix_precedent".into(),
            recency_multiplier: 1.5,
            explanation: "Bug-fix precedent — boosting recency so newer fixes surface first."
                .into(),
            ..Self::default_unknown()
        }
    }

    /// Profile for `ApiUsage`. Wider rerank pool to capture more
    /// call sites; symbol lane matters most.
    pub fn api_usage() -> Self {
        Self {
            name: "api_usage".into(),
            rerank_top_k: Some(40),
            explanation: "API-usage query — widening the rerank pool to surface more call sites."
                .into(),
            ..Self::default_unknown()
        }
    }

    /// Profile for `Configuration`. Disables the symbol lane —
    /// configuration usually lives in text/comments, not in
    /// tree-sitter symbols.
    pub fn configuration() -> Self {
        Self {
            name: "configuration".into(),
            symbol_lane_enabled: false,
            explanation: "Configuration query — focusing on text/semantic-text lanes (config rarely shows up as a symbol).".into(),
            ..Self::default_unknown()
        }
    }

    /// Profile for `ImplementationPattern`. Default lane blend; no
    /// changes today. Kept as a named profile so metadata distinguishes
    /// "we recognised this as a pattern query" from "we had no idea".
    pub fn implementation_pattern() -> Self {
        Self {
            name: "implementation_pattern".into(),
            explanation: "Implementation-pattern query — using the default lane blend.".into(),
            ..Self::default_unknown()
        }
    }

    /// Pick the profile for a parsed intent.
    pub fn for_intent(intent: QueryIntent) -> Self {
        match intent {
            QueryIntent::ImplementationPattern => Self::implementation_pattern(),
            QueryIntent::BugFixPrecedent => Self::bug_fix(),
            QueryIntent::ApiUsage => Self::api_usage(),
            QueryIntent::Configuration => Self::configuration(),
            QueryIntent::Unknown => Self::default_unknown(),
        }
    }
}

/// Classify `query` into a [`ParsedQuery`]. Pure / deterministic;
/// no I/O, no LLM. The parser walks the query body once, accumulates
/// matched rules, and picks the highest-priority intent that matched.
///
/// Order of intent checks (first match wins):
/// 1. **Configuration** — `where (do|did) we configure`,
///    `how is .* (loaded|configured)`, query contains `config` /
///    `configuration` / `env`.
/// 2. **BugFixPrecedent** — `how (did|do) we fix`, query contains
///    `bug` / `crash` / `error` + `fix` / `fixed`.
/// 3. **ApiUsage** — `how is .* (called|used)`, `usages of`,
///    quoted symbol + `usage` / `caller`.
/// 4. **ImplementationPattern** — `how (did|do) we`,
///    `pattern for`, `like (the|our) existing`,
///    `add .* like .* before`. Default for "how was X done before".
/// 5. **Unknown** — fallthrough.
///
/// Confidence:
/// - `High` when the matched rule is a verbatim phrase pattern.
/// - `Medium` when only a topic keyword fired (e.g. `config` alone).
/// - `Low` for `Unknown`.
pub fn parse_query(query: &str) -> ParsedQuery {
    let lower = query.to_lowercase();
    let mut matched_rules: Vec<String> = Vec::new();

    let language = extract_language_hint(&lower, &mut matched_rules);
    let path_terms = extract_path_terms(query, &mut matched_rules);
    let symbol_terms = extract_quoted_symbols(query, &mut matched_rules);
    let since_unix = extract_since(&lower, &mut matched_rules);

    let (intent, confidence) = classify_intent(&lower, &mut matched_rules);

    ParsedQuery {
        intent,
        confidence,
        language,
        path_terms,
        symbol_terms,
        since_unix,
        matched_rules,
    }
}

fn extract_language_hint(lower: &str, rules: &mut Vec<String>) -> Option<String> {
    // Recognise `in <lang>` / `<lang> code` / standalone language
    // tokens. Bounded to the four languages the indexer parses.
    for lang in ["rust", "python", "java", "kotlin"] {
        if lower.contains(&format!(" {lang} "))
            || lower.starts_with(&format!("{lang} "))
            || lower.ends_with(&format!(" {lang}"))
            || lower == lang
            || lower.contains(&format!("in {lang}"))
        {
            rules.push(format!("language_hint:{lang}"));
            return Some(lang.to_string());
        }
    }
    None
}

fn extract_path_terms(query: &str, rules: &mut Vec<String>) -> Vec<String> {
    // Slash-bearing identifiers — naive but effective. Splits the
    // query on whitespace + punctuation and keeps anything containing
    // `/` and at least one extension-like dot.
    let mut out = Vec::new();
    for word in query.split(|c: char| c.is_whitespace() || c == ',' || c == ';') {
        let trimmed = word
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '/' && c != '.' && c != '_');
        if trimmed.contains('/') && trimmed.contains('.') {
            out.push(trimmed.to_string());
        }
    }
    if !out.is_empty() {
        rules.push("path_terms".into());
    }
    out
}

fn extract_quoted_symbols(query: &str, rules: &mut Vec<String>) -> Vec<String> {
    // Anything inside backticks, single-quotes, or double-quotes is
    // treated as an identifier hint. Strings with whitespace are
    // skipped — those are usually phrases, not symbol names.
    let mut out = Vec::new();
    let mut chars = query.chars().peekable();
    while let Some(c) = chars.next() {
        let opener = match c {
            '`' | '\'' | '"' => c,
            _ => continue,
        };
        let mut buf = String::new();
        let mut closed = false;
        for c in chars.by_ref() {
            if c == opener {
                closed = true;
                break;
            }
            buf.push(c);
        }
        if closed && !buf.is_empty() && !buf.contains(char::is_whitespace) {
            out.push(buf);
        }
    }
    if !out.is_empty() {
        rules.push("quoted_symbols".into());
    }
    out
}

fn extract_since(lower: &str, rules: &mut Vec<String>) -> Option<i64> {
    // Recognises a small set of "last N days/weeks/months" phrases
    // and converts them to a unix-second cutoff against the current
    // time. More exotic forms ("since the v0.5 release") aren't
    // handled — explicit since_unix on PatternQuery is the escape
    // hatch.
    let now = chrono::Utc::now().timestamp();
    for phrase in [
        ("last week", 7 * 86400),
        ("last 7 days", 7 * 86400),
        ("last 30 days", 30 * 86400),
        ("last month", 30 * 86400),
        ("last quarter", 90 * 86400),
        ("last 90 days", 90 * 86400),
        ("last 6 months", 180 * 86400),
        ("last year", 365 * 86400),
    ] {
        if lower.contains(phrase.0) {
            rules.push(format!("since:{}", phrase.0));
            return Some(now - phrase.1);
        }
    }
    None
}

fn classify_intent(lower: &str, rules: &mut Vec<String>) -> (QueryIntent, Confidence) {
    // 1. Configuration.
    if lower.contains("where do we configure")
        || lower.contains("where did we configure")
        || lower.contains("how is") && (lower.contains("loaded") || lower.contains("configured"))
        || lower.contains("load configuration")
    {
        rules.push("intent:configuration:phrase".into());
        return (QueryIntent::Configuration, Confidence::High);
    }
    if lower.contains("config") || lower.contains(" env ") || lower.contains("environment variable")
    {
        rules.push("intent:configuration:keyword".into());
        return (QueryIntent::Configuration, Confidence::Medium);
    }

    // 2. BugFixPrecedent.
    if lower.contains("how did we fix")
        || lower.contains("how do we fix")
        || lower.contains("regression for")
    {
        rules.push("intent:bug_fix:phrase".into());
        return (QueryIntent::BugFixPrecedent, Confidence::High);
    }
    if (lower.contains(" bug") || lower.contains(" crash") || lower.contains(" error"))
        && (lower.contains(" fix") || lower.contains("fixed"))
    {
        rules.push("intent:bug_fix:keyword".into());
        return (QueryIntent::BugFixPrecedent, Confidence::Medium);
    }

    // 3. ApiUsage.
    if lower.contains("how is")
        && (lower.contains("called") || lower.contains("used") || lower.contains("invoked"))
    {
        rules.push("intent:api_usage:phrase".into());
        return (QueryIntent::ApiUsage, Confidence::High);
    }
    if lower.contains("usages of") || lower.contains("callers of") || lower.contains("call sites") {
        rules.push("intent:api_usage:phrase".into());
        return (QueryIntent::ApiUsage, Confidence::High);
    }

    // 4. ImplementationPattern.
    if lower.contains("how did we")
        || lower.contains("how do we")
        || lower.contains("pattern for")
        || lower.contains("like the existing")
        || lower.contains("like our existing")
        || lower.contains("how was")
            && (lower.contains("done before") || lower.contains("implemented"))
        || lower.contains("add ") && lower.contains(" like ") && lower.contains("before")
    {
        rules.push("intent:implementation_pattern:phrase".into());
        return (QueryIntent::ImplementationPattern, Confidence::High);
    }

    // 5. Fallthrough.
    rules.push("intent:unknown:fallthrough".into());
    (QueryIntent::Unknown, Confidence::Low)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsed_query_round_trips_through_serde() {
        let pq = ParsedQuery {
            intent: QueryIntent::BugFixPrecedent,
            confidence: Confidence::High,
            language: Some("rust".into()),
            path_terms: vec!["src/fetch.rs".into()],
            symbol_terms: vec!["retry_with_backoff".into()],
            since_unix: Some(1_700_000_000),
            matched_rules: vec!["bug_keyword".into(), "language_hint".into()],
        };
        let json = serde_json::to_string(&pq).expect("serialize");
        // snake_case enum representation contracts the API surface
        // — MCP clients filter on these strings.
        assert!(json.contains("\"intent\":\"bug_fix_precedent\""));
        assert!(json.contains("\"confidence\":\"high\""));
        let back: ParsedQuery = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, pq);
    }

    #[test]
    fn profile_name_is_part_of_the_serialised_shape() {
        // Plan 12 Task 1.1 Step 3: response metadata exposes the
        // profile name + explanation but not the unstable internal
        // weights. The full struct serialises today (no skip
        // attribute); MCP / CLI surface only `name + explanation` in
        // user-facing metadata.
        let p = RetrievalProfile::bug_fix();
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(json.contains("\"name\":\"bug_fix_precedent\""));
        assert!(json.contains("recency_multiplier"));
    }

    #[test]
    fn for_intent_picks_distinct_profiles_per_variant() {
        // Quick smoke that Self::for_intent returns the profile
        // matching the variant — the wiring is what the retriever
        // calls; if a future refactor breaks this, the routing is
        // wrong everywhere.
        assert_eq!(
            RetrievalProfile::for_intent(QueryIntent::Unknown).name,
            "unknown"
        );
        assert_eq!(
            RetrievalProfile::for_intent(QueryIntent::BugFixPrecedent).name,
            "bug_fix_precedent"
        );
        assert_eq!(
            RetrievalProfile::for_intent(QueryIntent::ApiUsage).name,
            "api_usage"
        );
        assert_eq!(
            RetrievalProfile::for_intent(QueryIntent::Configuration).name,
            "configuration"
        );
        assert_eq!(
            RetrievalProfile::for_intent(QueryIntent::ImplementationPattern).name,
            "implementation_pattern"
        );
    }

    #[test]
    fn parse_query_classifies_implementation_pattern_phrasing() {
        for query in [
            "add retry like before",
            "How did we add retry like the previous backoff?",
            "is there a pattern for connection pooling",
        ] {
            let pq = parse_query(query);
            assert!(
                matches!(pq.intent, QueryIntent::ImplementationPattern),
                "expected ImplementationPattern for {query:?}, got {:?}",
                pq.intent
            );
        }
    }

    #[test]
    fn parse_query_classifies_bug_fix_precedent() {
        for query in [
            "how did we fix the timeout bug",
            "how do we fix the crash on resume",
        ] {
            let pq = parse_query(query);
            assert!(
                matches!(pq.intent, QueryIntent::BugFixPrecedent),
                "expected BugFixPrecedent for {query:?}, got {:?}",
                pq.intent
            );
        }
    }

    #[test]
    fn parse_query_classifies_api_usage() {
        for query in [
            "how is `retry` called",
            "show me usages of FastEmbedReranker",
            "what are the callers of fetch",
        ] {
            let pq = parse_query(query);
            assert!(
                matches!(pq.intent, QueryIntent::ApiUsage),
                "expected ApiUsage for {query:?}, got {:?}",
                pq.intent
            );
        }
    }

    #[test]
    fn parse_query_classifies_configuration() {
        for query in [
            "where did we configure coreml",
            "how is the database url loaded",
            "what's the env config for the indexer",
        ] {
            let pq = parse_query(query);
            assert!(
                matches!(pq.intent, QueryIntent::Configuration),
                "expected Configuration for {query:?}, got {:?}",
                pq.intent
            );
        }
    }

    #[test]
    fn parse_query_unknown_for_unrecognised_text() {
        let pq = parse_query("just some random text");
        assert_eq!(pq.intent, QueryIntent::Unknown);
        assert_eq!(pq.confidence, Confidence::Low);
        assert!(pq.matched_rules.iter().any(|r| r.contains("fallthrough")));
    }

    #[test]
    fn parse_query_extracts_explicit_filters() {
        // Plan 12 Task 1.2 Step 2: language hint, path token, quoted
        // symbol name, and a "last 30 days" timeframe all surface in
        // ParsedQuery.
        let pq =
            parse_query("how is `retry_with_backoff` called in rust src/fetch.rs last 30 days");
        assert_eq!(pq.language.as_deref(), Some("rust"));
        assert!(pq.path_terms.iter().any(|p| p == "src/fetch.rs"));
        assert!(pq.symbol_terms.iter().any(|s| s == "retry_with_backoff"));
        assert!(pq.since_unix.is_some());
        assert!(pq
            .matched_rules
            .iter()
            .any(|r| r.starts_with("language_hint")));
        assert!(pq.matched_rules.iter().any(|r| r == "path_terms"));
        assert!(pq.matched_rules.iter().any(|r| r == "quoted_symbols"));
        assert!(pq.matched_rules.iter().any(|r| r.starts_with("since:")));
    }

    #[test]
    fn parse_query_high_confidence_for_phrase_match_medium_for_keyword_only() {
        // Plan 12 Task 1.2 Step 3: confidence reflects the strength
        // of the matching rule. Phrase patterns are High; topic
        // keywords alone are Medium; fallthrough is Low.
        assert_eq!(
            parse_query("how did we fix the timeout").confidence,
            Confidence::High
        );
        assert_eq!(
            parse_query("there was a bug we fixed").confidence,
            Confidence::Medium
        );
        assert_eq!(
            parse_query("just some random text").confidence,
            Confidence::Low
        );
    }

    #[test]
    fn default_unknown_profile_does_not_change_lane_set() {
        // Invariant: the Unknown profile is the no-op baseline. If
        // any field here drifts, the v0.7 promise of "unknown-intent
        // queries preserve today's default retrieval behavior"
        // breaks. The retriever's RankingWeights are unchanged when
        // recency_multiplier == 1.0 and every lane flag is true.
        let p = RetrievalProfile::default_unknown();
        assert!((p.recency_multiplier - 1.0).abs() < f32::EPSILON);
        assert!(p.vec_lane_enabled && p.text_lane_enabled && p.symbol_lane_enabled);
        assert!(p.rerank_top_k.is_none());
    }
}
