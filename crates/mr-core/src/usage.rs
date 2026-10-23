//! Reading token usage out of an upstream response, whoever the upstream is.
//!
//! Every provider reports the same three facts under a different spelling, and one of them —
//! cache hits — is the one that decides whether a request was cheap or expensive:
//!
//! | What | OpenAI chat | OpenAI Responses | Anthropic | Google | Command Code |
//! |---|---|---|---|---|---|
//! | input | `usage.prompt_tokens` | `usage.input_tokens` | `usage.input_tokens` | `usageMetadata.promptTokenCount` | `totalUsage.inputTokens` |
//! | output | `usage.completion_tokens` | `usage.output_tokens` | `usage.output_tokens` | `usageMetadata.candidatesTokenCount` | `totalUsage.outputTokens` |
//! | cache read | `usage.prompt_tokens_details.cached_tokens` | `usage.input_tokens_details.cached_tokens` | `usage.cache_read_input_tokens` | `usageMetadata.cachedContentTokenCount` | — |
//! | cache write | — | — | `usage.cache_creation_input_tokens` | — | — |
//!
//! ## Why this is a separate module rather than a few `get`s at each call site
//!
//! The relay reads usage in two very different places: a **buffered** body it can parse whole, and
//! a **stream** it must inspect frame by frame without buffering. Both have to agree about what a
//! cache hit is called, or the same request would report different numbers depending on whether the
//! client asked for `stream: true`. One parser, used by both, is what makes that agreement
//! structural instead of hoped for.
//!
//! ## The rule that matters: absent is not zero
//!
//! [`TokenUsage`] keeps each count as an `Option`. An upstream that reported nothing is not the
//! same as one that reported zero, and the store, the panel and any future cost math all depend on
//! telling those apart — a stream that ended before its usage frame arrived must not be recorded as
//! "0 tokens in, 0 tokens out", because that silently reports a real request as free.
//!
//! The one field that *is* derived is [`TokenUsage::cache_hit_rate`], and it is deliberately
//! `None` rather than `0.0` when the input count is unknown: "0 % of an unknown number" is not a
//! rate, it is a guess.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The bytes-per-token approximation this project uses wherever it must guess a token count.
///
/// Four bytes per token is the usual approximation for English text. It is an **estimate** and is
/// treated as one: it sizes a quota reservation before the provider has said anything, and it is
/// what the panel process turns observed byte counts into when the provider reported nothing. It
/// is never written over a provider's own count.
pub const CHARS_PER_TOKEN: u64 = 4;

/// Estimate a token count from a byte count, with the one rule the whole project shares.
///
/// The relay uses it to size a quota reservation, and the panel process uses it to turn the byte
/// counts the relay observed into an estimate. Both read it from here, so the number the router
/// reserves against and the number the panel shows cannot drift apart.
///
/// This is arithmetic, not measurement: it is deliberately **not** on the router's request path.
/// The relay only ever counts bytes; the division happens in the panel process, off the path.
pub fn estimate_tokens_from_bytes(bytes: u64) -> u64 {
    bytes / CHARS_PER_TOKEN
}

/// Token counts as one upstream reported them, in a provider-neutral shape.
///
/// Every field is optional and means exactly what it says: `None` is "the upstream did not tell
/// us", never "zero". A caller that wants to treat unknown as zero for arithmetic must say so at
/// the point of use — see [`TokenUsage::total`] for how the store does it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Tokens the prompt occupied, including any that were served from cache.
    pub input_tokens: Option<u64>,
    /// Tokens the model generated.
    pub output_tokens: Option<u64>,
    /// Prompt tokens served from the provider's cache. A subset of [`Self::input_tokens`], not an
    /// additional count — adding it to the input would double-count the prompt.
    pub cache_read_tokens: Option<u64>,
    /// Prompt tokens the provider charged a *write* for (Anthropic's cache creation). Also a
    /// subset of the input count, and reported separately because it is billed at a different rate.
    pub cache_write_tokens: Option<u64>,
}

impl TokenUsage {
    /// Whether the upstream reported anything at all.
    ///
    /// Used to decide whether a row is worth writing: a record with no counts and no cache
    /// information is an empty row, and writing it would inflate the request count of a table whose
    /// other columns say nothing.
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.cache_read_tokens.is_none()
            && self.cache_write_tokens.is_none()
    }

    /// Input plus output, treating an unreported count as zero.
    ///
    /// This is the one place unknown folds to zero, and it is deliberate: it is what the store's
    /// aggregates already do in SQL (`COALESCE(SUM(...), 0)`), so a total computed here and a total
    /// computed there cannot disagree. Callers that need to distinguish "no tokens" from "not
    /// reported" must read the fields directly.
    pub fn total(&self) -> u64 {
        self.input_tokens
            .unwrap_or(0)
            .saturating_add(self.output_tokens.unwrap_or(0))
    }

    /// The share of the prompt that came from cache, in `0.0..=1.0`.
    ///
    /// `None` when the input count is unknown, or when it is zero — a request with no prompt tokens
    /// has no cache-hit *rate*, and reporting one would be a division by zero dressed up as a
    /// percentage. A reported cache count larger than the input count is clamped to `1.0` rather
    /// than reported above 100 %: providers do occasionally report a cache count that includes
    /// tokens from a previous turn, and a "137 % cached" panel is a bug report, not information.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        let cached = self.cache_read_tokens?;
        match self.input_tokens {
            Some(input) if input > 0 => Some((cached as f64 / input as f64).clamp(0.0, 1.0)),
            _ => None,
        }
    }
}

/// Read a count from the first of several candidate paths, accepting an integer or a float.
///
/// Providers are inconsistent about the JSON type of a token count — Google emits an integer,
/// several OpenAI-compatible gateways emit `1234.0`, and some proxy an unbounded number as a
/// string. All of those mean the same thing, so they are all accepted; a value that is negative or
/// not numeric at all is treated as absent rather than as zero, because "0" is a real measurement
/// and this must not invent one.
fn count_at(value: &Value, paths: &[&[&str]]) -> Option<u64> {
    for path in paths {
        let mut cursor = value;
        let mut found = true;
        for key in *path {
            match cursor.get(*key) {
                Some(next) => cursor = next,
                None => {
                    found = false;
                    break;
                }
            }
        }
        if !found {
            continue;
        }
        if let Some(n) = as_token_count(cursor) {
            return Some(n);
        }
    }
    None
}

/// A JSON value as a token count, or `None` when it is not a non-negative number.
fn as_token_count(value: &Value) -> Option<u64> {
    if let Some(n) = value.as_u64() {
        return Some(n);
    }
    if let Some(f) = value.as_f64() {
        // A fractional token count is not meaningful, but a float that is exactly integral is how
        // several gateways spell an integer. Round, then refuse a negative.
        if f.is_finite() && f >= 0.0 {
            let rounded = f.round();
            // A count that rounds past `u64::MAX` has no honest representation. Saturating the cast
            // would turn a broken or hostile report into the largest possible charge.
            if rounded < u64::MAX as f64 {
                return Some(rounded as u64);
            }
        }
        return None;
    }
    // A count that arrived as a string ("1234") is still a count.
    value.as_str().and_then(|s| s.trim().parse::<u64>().ok())
}

impl TokenUsage {
    /// Parse usage from an already-identified usage object.
    ///
    /// Accepts every spelling at once rather than being told the dialect, and that is a deliberate
    /// choice: the relay knows the *upstream's* dialect when it sends the request, but a provider is
    /// free to answer in a shape it did not advertise (Copilot proxies OpenAI, Antigravity wraps
    /// Gemini, and a gateway in front of either may re-spell the fields). Matching on the fields
    /// actually present is what makes the reading survive that, and there is no ambiguity to lose:
    /// no provider means two different things by `prompt_tokens` and `input_tokens`.
    pub fn from_usage_object(usage: &Value) -> Self {
        Self {
            input_tokens: count_at(
                usage,
                &[
                    &["prompt_tokens"],
                    &["input_tokens"],
                    &["promptTokenCount"],
                    &["inputTokens"],
                ],
            ),
            output_tokens: count_at(
                usage,
                &[
                    &["completion_tokens"],
                    &["output_tokens"],
                    &["candidatesTokenCount"],
                    &["outputTokens"],
                ],
            ),
            cache_read_tokens: count_at(
                usage,
                &[
                    &["prompt_tokens_details", "cached_tokens"],
                    &["input_tokens_details", "cached_tokens"],
                    &["cache_read_input_tokens"],
                    &["cachedContentTokenCount"],
                    &["cacheReadInputTokens"],
                    &["cached_tokens"],
                    // Command Code / the Vercel AI SDK put the split one level down, under a
                    // camelCase `inputTokenDetails`. Without these two paths a cached turn records
                    // a cache miss, which overstates its cost.
                    &["inputTokenDetails", "cacheReadTokens"],
                ],
            ),
            cache_write_tokens: count_at(
                usage,
                &[
                    &["cache_creation_input_tokens"],
                    &["cacheCreationInputTokens"],
                    &["prompt_tokens_details", "cache_creation_tokens"],
                    &["inputTokenDetails", "cacheWriteTokens"],
                ],
            ),
        }
    }

    /// Read usage out of a whole, buffered response body in any dialect.
    ///
    /// Returns an empty [`TokenUsage`] rather than an error when the body is not JSON or carries no
    /// usage: a response whose shape this does not recognise is still a response the client
    /// received, and failing to parse it for telemetry must never turn into a failed request.
    ///
    /// The search is deliberately shallow-but-broad. Usage lives at the top level for every dialect
    /// this router speaks, under one of four names, and a Google *stream* is the one case that
    /// nests it inside a `response` envelope — handled by looking through that envelope first.
    pub fn from_body(body: &[u8]) -> Self {
        let Ok(value) = serde_json::from_slice::<Value>(body) else {
            return Self::default();
        };
        Self::from_response_value(&value)
    }

    /// Read usage from a parsed response object, looking through known envelopes.
    pub fn from_response_value(value: &Value) -> Self {
        // A Google Cloud Code frame wraps the payload in `{"response": {...}}`; the usage is on the
        // inner object, so unwrap before searching rather than teaching every path about both.
        let inner = value.get("response").unwrap_or(value);

        for key in ["usage", "usageMetadata", "usage_metadata", "totalUsage"] {
            if let Some(usage) = inner.get(key) {
                if !usage.is_null() {
                    let parsed = Self::from_usage_object(usage);
                    if !parsed.is_empty() {
                        return parsed;
                    }
                }
            }
        }

        // Some Responses-API servers put the counts at the top level of the event rather than under
        // a `usage` key. Only accept that when the fields are actually present, so an unrelated
        // `input_tokens` in a request echo cannot be mistaken for an answer's usage.
        if inner.get("input_tokens").is_some() || inner.get("output_tokens").is_some() {
            let parsed = Self::from_usage_object(inner);
            if !parsed.is_empty() {
                return parsed;
            }
        }

        Self::default()
    }

    /// Merge a later report over an earlier one, keeping whatever the later one omitted.
    ///
    /// A stream reports usage across frames — OpenAI sends one final frame, Anthropic splits input
    /// across `message_start` and output across `message_delta`, and a gateway may send both. Later
    /// frames win field by field, which is what makes "the most complete report so far" fall out of
    /// the fold instead of needing a choice about which frame to trust.
    pub fn merged_with(self, later: Self) -> Self {
        Self {
            input_tokens: later.input_tokens.or(self.input_tokens),
            output_tokens: later.output_tokens.or(self.output_tokens),
            cache_read_tokens: later.cache_read_tokens.or(self.cache_read_tokens),
            cache_write_tokens: later.cache_write_tokens.or(self.cache_write_tokens),
        }
    }
}

/// Tokens per second, or `None` when it cannot be computed honestly.
///
/// The rate is **output** tokens over the generation time, not over the whole request: input tokens
/// are consumed by the prefill, which finishes before the first token is emitted, so dividing the
/// whole prompt by the whole wall time would report a rate that falls as the prompt grows. That is
/// the number that says nothing about how fast the model is writing.
///
/// `generation_ms` must therefore be the time from the first output token to the last, not the full
/// request duration. The caller owns that measurement; this function only refuses to invent a rate
/// from numbers that cannot support one:
///
/// * no reported output tokens — `None`, because a rate of zero would claim the model wrote nothing;
/// * a non-positive duration — `None`, because a division by zero is not a fast model;
/// * zero output tokens — `None`, not `0.0`, because a request that wrote nothing has no speed, and
///   `0 tok/s` in a panel reads as a stall rather than as "nothing was generated".
pub fn tokens_per_second(output_tokens: Option<u64>, generation_ms: Option<u64>) -> Option<f64> {
    let tokens = output_tokens?;
    let millis = generation_ms?;
    if tokens == 0 || millis == 0 {
        return None;
    }
    Some(tokens as f64 / (millis as f64 / 1000.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The live Command Code shape: camelCase `totalUsage`, with the cache split nested one level
    /// further down under `inputTokenDetails`.
    ///
    /// This is a verbatim `finish` event from the real endpoint. The nesting is the part that
    /// matters: a reader that only looks at the top level records a cache miss for every cached
    /// turn, which overstates what that turn cost.
    #[test]
    fn a_command_code_finish_event_yields_counts_and_a_cache_split() {
        let body = br#"{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":44,"inputTokenDetails":{"noCacheTokens":44,"cacheReadTokens":0},"outputTokens":19,"totalTokens":63,"cachedInputTokens":0}}"#;
        let usage = TokenUsage::from_body(body);
        assert_eq!(usage.input_tokens, Some(44));
        assert_eq!(usage.output_tokens, Some(19));
        assert_eq!(usage.cache_read_tokens, Some(0));

        // `noCacheTokens` is the *uncached* share and must never be mistaken for a cache read.
        let cached = br#"{"type":"finish","totalUsage":{"inputTokens":100,"inputTokenDetails":{"noCacheTokens":20,"cacheReadTokens":80,"cacheWriteTokens":15},"outputTokens":25}}"#;
        let usage = TokenUsage::from_body(cached);
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(25));
        assert_eq!(
            usage.cache_read_tokens,
            Some(80),
            "the cache read is the nested figure, not the uncached share"
        );
        assert_eq!(usage.cache_write_tokens, Some(15));
        assert_eq!(usage.cache_hit_rate(), Some(0.8));
    }

    #[test]
    fn every_dialects_spelling_of_input_and_output_is_read() {
        // One case per column of the table in the module docs, so a dialect added later has an
        // obvious place to add its own row.
        let cases: &[(&str, Value, u64, u64)] = &[
            (
                "openai chat",
                json!({"prompt_tokens": 11, "completion_tokens": 5}),
                11,
                5,
            ),
            (
                "openai responses",
                json!({"input_tokens": 11, "output_tokens": 5}),
                11,
                5,
            ),
            (
                "anthropic",
                json!({"input_tokens": 11, "output_tokens": 5}),
                11,
                5,
            ),
            (
                "google",
                json!({"promptTokenCount": 11, "candidatesTokenCount": 5}),
                11,
                5,
            ),
            (
                "command code",
                json!({"inputTokens": 11, "outputTokens": 5}),
                11,
                5,
            ),
        ];
        for (name, usage, input, output) in cases {
            let parsed = TokenUsage::from_usage_object(usage);
            assert_eq!(parsed.input_tokens, Some(*input), "{name} input");
            assert_eq!(parsed.output_tokens, Some(*output), "{name} output");
        }
    }

    #[test]
    fn every_dialects_spelling_of_a_cache_read_is_read() {
        let cases: &[(&str, Value, u64)] = &[
            (
                "openai chat",
                json!({"prompt_tokens": 100, "prompt_tokens_details": {"cached_tokens": 80}}),
                80,
            ),
            (
                "openai responses",
                json!({"input_tokens": 100, "input_tokens_details": {"cached_tokens": 80}}),
                80,
            ),
            (
                "anthropic",
                json!({"input_tokens": 100, "cache_read_input_tokens": 80}),
                80,
            ),
            (
                "google",
                json!({"promptTokenCount": 100, "cachedContentTokenCount": 80}),
                80,
            ),
        ];
        for (name, usage, cached) in cases {
            let parsed = TokenUsage::from_usage_object(usage);
            assert_eq!(parsed.cache_read_tokens, Some(*cached), "{name} cache read");
        }
    }

    #[test]
    fn anthropic_cache_writes_are_read_separately_from_cache_reads() {
        // The two are distinct on Anthropic and billed differently; folding them together would
        // overstate the saving a cache produced.
        let usage = json!({
            "input_tokens": 100,
            "output_tokens": 5,
            "cache_read_input_tokens": 80,
            "cache_creation_input_tokens": 20,
        });
        let parsed = TokenUsage::from_usage_object(&usage);
        assert_eq!(parsed.cache_read_tokens, Some(80));
        assert_eq!(parsed.cache_write_tokens, Some(20));
    }

    #[test]
    fn a_float_or_string_count_is_still_a_count() {
        // Real gateways emit `1234.0` and, occasionally, `"1234"`.
        let usage = json!({
            "prompt_tokens": 100.0,
            "completion_tokens": "5",
            "prompt_tokens_details": {"cached_tokens": 80.0},
        });
        let parsed = TokenUsage::from_usage_object(&usage);
        assert_eq!(parsed.input_tokens, Some(100));
        assert_eq!(parsed.output_tokens, Some(5));
        assert_eq!(parsed.cache_read_tokens, Some(80));
    }

    #[test]
    fn absent_is_not_zero() {
        // The distinction the whole module exists to preserve.
        let parsed = TokenUsage::from_body(br#"{"choices":[]}"#);
        assert_eq!(parsed.input_tokens, None);
        assert_eq!(parsed.output_tokens, None);
        assert!(parsed.is_empty());
    }

    #[test]
    fn a_reported_zero_is_kept_as_zero() {
        let parsed =
            TokenUsage::from_body(br#"{"usage":{"prompt_tokens":0,"completion_tokens":0}}"#);
        assert_eq!(parsed.input_tokens, Some(0));
        assert_eq!(parsed.output_tokens, Some(0));
        assert!(
            !parsed.is_empty(),
            "a reported zero is a measurement, not an absence"
        );
    }

    #[test]
    fn a_negative_or_nonsense_count_is_absent_rather_than_guessed_at() {
        let usage = json!({"prompt_tokens": -5, "completion_tokens": "lots"});
        let parsed = TokenUsage::from_usage_object(&usage);
        assert_eq!(parsed.input_tokens, None);
        assert_eq!(parsed.output_tokens, None);
    }

    #[test]
    fn a_google_envelope_is_looked_through() {
        // Cloud Code frames carry the payload one level down.
        let body =
            br#"{"response":{"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}}}"#;
        let parsed = TokenUsage::from_body(body);
        assert_eq!(parsed.input_tokens, Some(7));
        assert_eq!(parsed.output_tokens, Some(3));
    }

    #[test]
    fn command_codes_total_usage_is_found() {
        let body = br#"{"type":"finish","totalUsage":{"inputTokens":21,"outputTokens":8}}"#;
        let parsed = TokenUsage::from_body(body);
        assert_eq!(parsed.input_tokens, Some(21));
        assert_eq!(parsed.output_tokens, Some(8));
    }

    #[test]
    fn cache_hit_rate_is_a_share_of_the_prompt() {
        let usage = TokenUsage {
            input_tokens: Some(200),
            cache_read_tokens: Some(50),
            ..Default::default()
        };
        assert_eq!(usage.cache_hit_rate(), Some(0.25));
    }

    #[test]
    fn cache_hit_rate_is_unknown_rather_than_zero_when_it_cannot_be_known() {
        // 0 % of an unknown prompt is a guess, and a panel showing "0 % cached" for a request whose
        // usage never arrived would be lying in the direction that looks like a saving.
        let no_cache_report = TokenUsage {
            input_tokens: Some(200),
            cache_read_tokens: None,
            ..Default::default()
        };
        assert_eq!(no_cache_report.cache_hit_rate(), None);

        let no_input = TokenUsage {
            input_tokens: None,
            cache_read_tokens: Some(50),
            ..Default::default()
        };
        assert_eq!(no_input.cache_hit_rate(), None);

        let empty_prompt = TokenUsage {
            input_tokens: Some(0),
            cache_read_tokens: Some(0),
            ..Default::default()
        };
        assert_eq!(empty_prompt.cache_hit_rate(), None);
    }

    #[test]
    fn an_over_reported_cache_count_is_clamped_not_reported_above_one_hundred_percent() {
        let usage = TokenUsage {
            input_tokens: Some(100),
            cache_read_tokens: Some(137),
            ..Default::default()
        };
        assert_eq!(usage.cache_hit_rate(), Some(1.0));
    }

    #[test]
    fn totals_treat_an_unreported_count_as_zero_like_the_stores_sql_does() {
        let partial = TokenUsage {
            input_tokens: Some(10),
            output_tokens: None,
            ..Default::default()
        };
        assert_eq!(partial.total(), 10);
    }

    #[test]
    fn merging_keeps_what_the_later_report_omitted() {
        // Anthropic splits input across `message_start` and output across `message_delta`; folding
        // field-by-field is what makes the complete picture available at the end.
        let start = TokenUsage {
            input_tokens: Some(100),
            cache_read_tokens: Some(80),
            ..Default::default()
        };
        let delta = TokenUsage {
            output_tokens: Some(42),
            ..Default::default()
        };
        let merged = start.merged_with(delta);
        assert_eq!(merged.input_tokens, Some(100));
        assert_eq!(merged.cache_read_tokens, Some(80));
        assert_eq!(merged.output_tokens, Some(42));
    }

    #[test]
    fn merging_lets_a_later_explicit_value_win() {
        let early = TokenUsage {
            output_tokens: Some(1),
            ..Default::default()
        };
        let late = TokenUsage {
            output_tokens: Some(99),
            ..Default::default()
        };
        assert_eq!(early.merged_with(late).output_tokens, Some(99));
    }

    #[test]
    fn tokens_per_second_is_output_over_generation_time() {
        // 50 tokens in 2 s is 25 tok/s.
        assert_eq!(tokens_per_second(Some(50), Some(2000)), Some(25.0));
    }

    #[test]
    fn tokens_per_second_refuses_to_invent_a_rate() {
        assert_eq!(tokens_per_second(None, Some(1000)), None);
        assert_eq!(tokens_per_second(Some(50), None), None);
        assert_eq!(tokens_per_second(Some(0), Some(1000)), None);
        assert_eq!(tokens_per_second(Some(50), Some(0)), None);
    }

    #[test]
    fn a_body_that_is_not_json_reports_nothing_rather_than_failing() {
        // Telemetry must never be able to fail a request that already succeeded.
        assert!(TokenUsage::from_body(b"not json at all").is_empty());
        assert!(TokenUsage::from_body(b"").is_empty());
    }
}
