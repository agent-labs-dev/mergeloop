//! What a model's tokens cost, from the vendors' list prices, so every run is priced the same way
//! whichever CLI ran it.
//!
//! Prices are USD per million tokens at the short-context, standard tier. A request over a
//! model's long-context threshold (OpenAI: 272K input; Claude Haiku 5.5: 100K prompt) bills
//! higher, but the CLIs report a run's totals, not each request's, so a run with such requests
//! costs at least what this says.
//!
//! Sources, checked 2026-11-07:
//! - Anthropic: <https://platform.claude.com/docs/en/about-claude/pricing>
//! - OpenAI: <https://developers.openai.com/api/docs/pricing> (Standard), and
//!   <https://developers.openai.com/api/docs/guides/prompt-caching> for which write rate applies.
//!
//! When a vendor changes a price, or `Provider::models` gains a model, update `PRICES` and the
//! date above. A test fails while an offered model has no price.

/// One model's list prices, USD per million tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    pub input: f64,
    /// A cache hit.
    pub cache_read: f64,
    /// A write to the short cache: Anthropic's 5-minute, OpenAI's 30-minute (its default).
    pub cache_write: f64,
    /// A write to the long cache: Anthropic's 1-hour, which Claude Code uses; OpenAI's 12-hour.
    pub cache_write_long: f64,
    /// Output, reasoning and thinking included: both vendors bill those as output.
    pub output: f64,
}

const fn p(
    input: f64,
    cache_read: f64,
    cache_write: f64,
    cache_write_long: f64,
    output: f64,
) -> Price {
    Price {
        input,
        cache_read,
        cache_write,
        cache_write_long,
        output,
    }
}

const PRICES: &[(&str, Price)] = &[
    ("claude-opus-5-5", p(4.0, 0.20, 5.0, 8.0, 20.0)),
    ("claude-fable-5-1", p(10.0, 0.25, 12.50, 20.0, 50.0)),
    ("claude-sonnet-5-5", p(2.0, 0.10, 2.50, 4.0, 10.0)),
    ("claude-haiku-5-5", p(0.10, 0.01, 0.125, 0.20, 0.50)),
    ("gpt-6.1-sol", p(2.0, 0.10, 2.50, 5.0, 10.0)),
    ("gpt-6-astra", p(10.0, 1.0, 12.50, 25.0, 50.0)),
    ("gpt-6.1-terra", p(1.0, 0.05, 1.25, 2.50, 5.0)),
    ("gpt-6-luna", p(0.10, 0.01, 0.125, 0.25, 0.50)),
];

impl Price {
    /// The model's price. `None` for a model we have no price for, such as one OpenRouter serves.
    pub fn of(model: &str) -> Option<Price> {
        PRICES.iter().find(|(m, _)| *m == model).map(|(_, p)| *p)
    }
}

/// The tokens a run used on one model, each kind counted once.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    /// Input that neither hit nor wrote the cache.
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cache_write_long: u64,
    pub output: u64,
}

impl Usage {
    /// What the tokens cost on `model`, or `None` when it has no price: never $0.
    pub fn cost_usd(&self, model: &str) -> Option<f64> {
        let p = Price::of(model)?;
        let usd = |tokens: u64, per_million: f64| tokens as f64 * per_million / 1e6;
        Some(
            usd(self.input, p.input)
                + usd(self.cache_read, p.cache_read)
                + usd(self.cache_write, p.cache_write)
                + usd(self.cache_write_long, p.cache_write_long)
                + usd(self.output, p.output),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Provider;

    #[test]
    fn every_offered_model_has_a_price() {
        for provider in Provider::SUPPORTED {
            for model in provider.models() {
                assert!(
                    Price::of(model.id).is_some(),
                    "{} has no price: add it to PRICES in crates/core/src/price.rs",
                    model.id
                );
            }
        }
    }

    #[test]
    fn each_kind_of_token_costs_its_own_rate() {
        // A real Claude Code run on Haiku 5.5, whose CLI said it cost $0.00360255.
        let usage = Usage {
            input: 1184,
            cache_read: 62225,
            cache_write: 0,
            cache_write_long: 14067,
            output: 97,
        };
        let cost = usage.cost_usd("claude-haiku-5-5").unwrap();
        assert!((cost - 0.00360255).abs() < 1e-12, "{cost}");
    }

    #[test]
    fn a_model_without_a_price_is_unpriced_not_free() {
        assert_eq!(Usage::default().cost_usd("some/openrouter-model"), None);
        assert_eq!(Usage::default().cost_usd("claude-haiku-5-5"), Some(0.0));
    }
}
