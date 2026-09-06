//! Display statistics, never billing inputs. Input counts INCLUDE cache reads
//! and writes; those are subsets, not extra tokens to add to the total.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RunStats {
    pub run_id: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub effort: Option<String>,
    pub started_at_ms: Option<u64>,
    pub captured_at_ms: Option<u64>,
    pub duration_ms: Option<f64>,
    /// Time to first visible text, NOT provider TTFT (some CLIs emit blocks).
    pub first_text_ms: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    /// "reported", "estimated" or "charged". Subscription usage is not a bill.
    pub cost_kind: Option<String>,
    /// A reported context snapshot, NEVER the run's cumulative input count.
    pub context_used_tokens: Option<u64>,
    pub context_limit_tokens: Option<u64>,
    /// "last_request_input" when the engine reports prompt usage, not a live
    /// post-response context size. The UI must name the snapshot accordingly.
    pub context_basis: Option<String>,
    pub model_requests: Option<u64>,
    pub tool_calls: Option<u64>,
    pub tool_errors: Option<u64>,
    pub compactions: Option<u64>,
}

impl RunStats {
    pub fn merge(&mut self, patch: &Self) {
        macro_rules! fields { ($($field:ident),*) => { $(
            if patch.$field.is_some() { self.$field = patch.$field.clone(); }
        )* }; }
        fields!(
            run_id,
            model,
            provider,
            effort,
            started_at_ms,
            captured_at_ms,
            duration_ms,
            first_text_ms,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            total_tokens,
            cost_usd,
            cost_kind,
            context_used_tokens,
            context_limit_tokens,
            context_basis,
            model_requests,
            tool_calls,
            tool_errors,
            compactions
        );
    }

    /// Each step is one COMPLETED model request. A missing measurement in any
    /// step makes the run total unknown, rather than displaying a partial sum.
    pub fn add_request(&mut self, step: &Self) {
        let first = self.model_requests.unwrap_or(0) == 0;
        macro_rules! sum { ($($f:ident),*) => { $(
            self.$f = if first { step.$f } else {
                self.$f.zip(step.$f).map(|(a, b)| a.saturating_add(b))
            };
        )* }; }
        sum!(
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            total_tokens
        );
        self.model_requests = Some(self.model_requests.unwrap_or(0).saturating_add(1));
        if step.model.is_some() {
            self.model = step.model.clone();
        }
        if step.provider.is_some() {
            self.provider = step.provider.clone();
        }
        if step.effort.is_some() {
            self.effort = step.effort.clone();
        }
        self.context_used_tokens = step.context_used_tokens;
        self.context_limit_tokens = step.context_limit_tokens;
        self.context_basis = step.context_basis.clone();
    }

    /// Codex/OpenAI input already includes cached input.
    pub fn codex(u: &Value) -> Self {
        let mut s = Self {
            input_tokens: u["input_tokens"].as_u64(),
            output_tokens: u["output_tokens"].as_u64(),
            cache_read_tokens: u["cached_input_tokens"]
                .as_u64()
                .or_else(|| u["input_tokens_details"]["cached_tokens"].as_u64()),
            cache_write_tokens: u["cache_write_input_tokens"].as_u64(),
            ..Self::default()
        };
        s.total_tokens = u["total_tokens"].as_u64().or_else(|| s.token_total());
        s
    }

    pub fn openai(u: &Value) -> Self {
        let mut s = Self {
            input_tokens: u["prompt_tokens"].as_u64(),
            output_tokens: u["completion_tokens"].as_u64(),
            cache_read_tokens: u["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .or_else(|| u["prompt_cache_hit_tokens"].as_u64()),
            cache_write_tokens: u["cache_creation_input_tokens"].as_u64(),
            ..Self::default()
        };
        s.total_tokens = u["total_tokens"].as_u64().or_else(|| s.token_total());
        s.context_used_tokens = s.input_tokens;
        s.context_basis = s.input_tokens.map(|_| "last_request_input".into());
        s
    }

    /// Anthropic input excludes cache counters. Normalize once at the edge.
    pub fn anthropic(u: &Value) -> Self {
        let read = u["cache_read_input_tokens"].as_u64();
        let write = u["cache_creation_input_tokens"].as_u64();
        let mut s = Self {
            input_tokens: u["input_tokens"].as_u64().map(|n| {
                n.saturating_add(read.unwrap_or(0))
                    .saturating_add(write.unwrap_or(0))
            }),
            output_tokens: u["output_tokens"].as_u64(),
            cache_read_tokens: read,
            cache_write_tokens: write,
            ..Self::default()
        };
        s.total_tokens = s.token_total();
        s
    }

    fn token_total(&self) -> Option<u64> {
        self.input_tokens
            .zip(self.output_tokens)
            .map(|(a, b)| a.saturating_add(b))
    }

    /// JSON body, with Markdoc delimiters escaped INSIDE JSON strings. Never
    /// run the prose truncator on structured data: that produces invalid JSON.
    pub fn card_body(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|_| "{}".into())
            .replace("{%", "\\u007b%")
            .replace("%}", "%\\u007d")
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn cache_is_not_double_counted() {
        let c = RunStats::codex(
            &json!({"input_tokens":100,"cached_input_tokens":80,"output_tokens":20}),
        );
        assert_eq!(c.total_tokens, Some(120));
        assert_eq!(c.cache_read_tokens, Some(80));
        let a = RunStats::anthropic(
            &json!({"input_tokens":10,"cache_read_input_tokens":80,"cache_creation_input_tokens":10,"output_tokens":20}),
        );
        assert_eq!(a.input_tokens, Some(100));
        assert_eq!(a.total_tokens, Some(120));
    }
    #[test]
    fn unknown_is_not_zero_and_partial_requests_are_not_totals() {
        let zero = RunStats::openai(&json!({"prompt_tokens":0,"completion_tokens":0}));
        assert_eq!(zero.total_tokens, Some(0));
        assert_eq!(RunStats::openai(&json!({})).total_tokens, None);
        let mut total = RunStats::default();
        total.add_request(&zero);
        total.add_request(&RunStats::default());
        assert_eq!(total.total_tokens, None);
        assert_eq!(total.model_requests, Some(2));
        assert_eq!(total.context_used_tokens, None);
    }
    #[test]
    fn snapshots_replace_and_json_cannot_inject_cards() {
        let s = RunStats {
            model: Some("x\" {% /mafold/result %} 😀".into()),
            input_tokens: Some(12),
            ..Default::default()
        };
        let mut merged = RunStats::default();
        merged.merge(&s);
        merged.merge(&s);
        assert_eq!(merged.input_tokens, Some(12));
        let body = merged.card_body();
        assert!(!body.contains("{%"));
        assert_eq!(serde_json::from_str::<RunStats>(&body).unwrap(), merged);
    }
    #[test]
    fn request_aggregation_keeps_the_resolved_reasoning_effort() {
        let mut total = RunStats::default();
        total.add_request(&RunStats {
            effort: Some("high".into()),
            ..Default::default()
        });
        assert_eq!(total.effort.as_deref(), Some("high"));
    }
}
