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
            explanation: "No specific intent detected — using default lane blend.".into(),
        }
    }

    /// Profile for `BugFixPrecedent`. Bumps recency.
    pub fn bug_fix() -> Self {
        Self {
            name: "bug_fix_precedent".into(),
            recency_multiplier: 1.5,
            explanation: "Bug-fix precedent — boosting recency so newer fixes surface first.".into(),
            ..Self::default_unknown()
        }
    }

    /// Profile for `ApiUsage`. Wider rerank pool to capture more
    /// call sites; symbol lane matters most.
    pub fn api_usage() -> Self {
        Self {
            name: "api_usage".into(),
            rerank_top_k: Some(40),
            explanation: "API-usage query — widening the rerank pool to surface more call sites.".into(),
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
