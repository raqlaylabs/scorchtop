//! Static, hardcoded pricing table. Per-million-token USD rates.
//!
//! Never fetched from the network. Unknown model => `None`: tokens are still
//! counted, cost is shown as `—`, never guessed. All dollar figures are
//! labeled "est. API value" in the UI (subscription users don't pay per
//! token).

use crate::source::TokenUsage;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    /// USD per 1M input tokens.
    pub input: f64,
    /// USD per 1M output tokens.
    pub output: f64,
}

impl ModelPricing {
    /// Cache write = 1.25x input; cache read = 0.1x input.
    pub fn cost(&self, usage: &TokenUsage) -> f64 {
        (usage.input as f64 * self.input
            + usage.output as f64 * self.output
            + usage.cache_create as f64 * self.input * 1.25
            + usage.cache_read as f64 * self.input * 0.1)
            / 1_000_000.0
    }
}

/// Substring-matched pricing, most specific patterns first.
/// Rates per 1M tokens, from Anthropic's published price sheet (2026-06).
const TABLE: &[(&str, ModelPricing)] = &[
    ("fable-5", ModelPricing { input: 10.0, output: 50.0 }),
    ("mythos", ModelPricing { input: 10.0, output: 50.0 }),
    // Opus 4.5+ dropped to $5/$25.
    ("opus-4-5", ModelPricing { input: 5.0, output: 25.0 }),
    ("opus-4-6", ModelPricing { input: 5.0, output: 25.0 }),
    ("opus-4-7", ModelPricing { input: 5.0, output: 25.0 }),
    ("opus-4-8", ModelPricing { input: 5.0, output: 25.0 }),
    // Older Opus (4.1, 4.0, Opus 3) were $15/$75.
    ("opus", ModelPricing { input: 15.0, output: 75.0 }),
    // All Sonnet generations (5, 4.x, 3.x) are $3/$15.
    ("sonnet", ModelPricing { input: 3.0, output: 15.0 }),
    ("haiku-4-5", ModelPricing { input: 1.0, output: 5.0 }),
    ("3-5-haiku", ModelPricing { input: 0.8, output: 4.0 }),
    ("haiku", ModelPricing { input: 0.25, output: 1.25 }),
];

pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    let model = model.to_ascii_lowercase();
    TABLE
        .iter()
        .find(|(pat, _)| model.contains(pat))
        .map(|(_, p)| *p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_by_substring() {
        assert_eq!(
            pricing_for("claude-opus-4-8"),
            Some(ModelPricing { input: 5.0, output: 25.0 })
        );
        assert_eq!(
            pricing_for("claude-sonnet-4-5-20250929"),
            Some(ModelPricing { input: 3.0, output: 15.0 })
        );
        assert_eq!(
            pricing_for("claude-haiku-4-5-20251001"),
            Some(ModelPricing { input: 1.0, output: 5.0 })
        );
        // Older Opus hits the generic (more expensive) row.
        assert_eq!(
            pricing_for("claude-opus-4-1-20250805"),
            Some(ModelPricing { input: 15.0, output: 75.0 })
        );
    }

    #[test]
    fn unknown_model_is_none_not_guessed() {
        assert_eq!(pricing_for("gpt-9-mega"), None);
        assert_eq!(pricing_for("<synthetic>"), None);
    }

    #[test]
    fn cost_applies_cache_multipliers() {
        let p = ModelPricing { input: 10.0, output: 50.0 };
        let usage = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cache_create: 1_000_000,
            cache_read: 1_000_000,
        };
        // 10 + 50 + 12.5 + 1.0
        assert!((p.cost(&usage) - 73.5).abs() < 1e-9);
    }
}
