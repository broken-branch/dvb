//! The price table: US dollars per million tokens, by model id, for
//! the estimated cost on `run.usage`. It is a release file: a new model or a
//! changed price is a PR. Subscriptions are the billing, so this is an
//! estimate of what the same tokens would cost at API rates, not a charge.

use dvb_core::{Effort, ModelPool, PoolModel, Vendor};

/// Dollars per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub uncached_in: f64,
    pub cached_in: f64,
    pub out: f64,
}

const fn p(uncached_in: f64, cached_in: f64, out: f64) -> Price {
    Price {
        uncached_in,
        cached_in,
        out,
    }
}

/// The release catalogue: supported ids, vendors, efforts and prices.
struct Model {
    id: &'static str,
    vendor: Vendor,
    efforts: &'static [Effort],
    price: Price,
}

const STANDARD: &[Effort] = &[Effort::Low, Effort::Medium, Effort::High];
const FAKE: &[Effort] = &[Effort::Medium, Effort::High];
const TABLE: &[Model] = &[
    Model {
        id: "claude-opus-5-5",
        vendor: Vendor::Claude,
        efforts: STANDARD,
        price: p(5.0, 0.5, 25.0),
    },
    Model {
        id: "claude-fable-5-1",
        vendor: Vendor::Claude,
        efforts: STANDARD,
        price: p(10.0, 0.25, 50.0),
    },
    Model {
        id: "claude-sonnet-5-5",
        vendor: Vendor::Claude,
        efforts: STANDARD,
        price: p(3.0, 0.3, 15.0),
    },
    Model {
        id: "claude-haiku-4-5",
        vendor: Vendor::Claude,
        efforts: STANDARD,
        price: p(1.0, 0.1, 5.0),
    },
    Model {
        id: "gpt-6-astra",
        vendor: Vendor::Codex,
        efforts: STANDARD,
        price: p(1.25, 0.125, 10.0),
    },
    Model {
        id: "gpt-6.1-sol",
        vendor: Vendor::Codex,
        efforts: STANDARD,
        price: p(1.25, 0.125, 10.0),
    },
    Model {
        id: "gpt-6-luna",
        vendor: Vendor::Codex,
        efforts: STANDARD,
        price: p(0.25, 0.025, 2.0),
    },
    Model {
        id: "fake-1",
        vendor: Vendor::Fake,
        efforts: FAKE,
        price: p(0.0, 0.0, 0.0),
    },
    Model {
        id: "fake-next",
        vendor: Vendor::Fake,
        efforts: FAKE,
        price: p(0.0, 0.0, 0.0),
    },
];

/// Availability is confirmed at preflight; Fake installations retain their own pool.
pub(crate) fn model_pool(fake: bool) -> ModelPool {
    ModelPool {
        models: TABLE
            .iter()
            .filter(|m| (m.vendor == Vendor::Fake) == fake)
            .map(|m| PoolModel {
                id: m.id.into(),
                vendor: m.vendor,
                efforts: m.efforts.to_vec(),
            })
            .collect(),
    }
}

/// `claude-haiku-4-5-20251001` is priced as `claude-haiku-4-5`.
pub fn price_of(vendor: Vendor, model: &str) -> Option<Price> {
    match vendor {
        Vendor::Fake => Some(p(0.0, 0.0, 0.0)),
        Vendor::Claude | Vendor::Codex => TABLE
            .iter()
            .find(|entry| {
                let id = entry.id;
                entry.vendor != Vendor::Fake
                    && (model == id
                        || model
                            .strip_prefix(id)
                            .and_then(|r| r.strip_prefix('-'))
                            .is_some_and(|d| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit())))
            })
            .map(|entry| entry.price),
    }
}

/// The estimated cost in dollars; 0 for a model not in the table.
pub fn estimate(vendor: Vendor, model: &str, uncached_in: u64, cached_in: u64, out: u64) -> f64 {
    let Some(price) = price_of(vendor, model) else {
        return 0.0;
    };
    #[expect(
        clippy::cast_precision_loss,
        reason = "Usage pricing intentionally converts bounded token counts to floating point"
    )]
    let m = |tokens: u64, per_million: f64| tokens as f64 / 1_000_000.0 * per_million;
    m(uncached_in, price.uncached_in) + m(cached_in, price.cached_in) + m(out, price.out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pool_model_is_priced_and_the_priced_haiku_is_available() {
        for fake in [false, true] {
            let pool = model_pool(fake);
            assert_eq!(
                pool.models.iter().any(|m| m.id == "claude-haiku-4-5"),
                !fake
            );
            for model in pool.models {
                let price = price_of(model.vendor, &model.id).expect("pool model must be priced");
                assert_eq!(price.out == 0.0, fake);
                assert!(!model.efforts.is_empty());
            }
        }
    }

    #[test]
    fn fable_5_1_lookup_and_cost_include_cache_reads() {
        assert_eq!(
            price_of(Vendor::Claude, "claude-fable-5-1"),
            Some(p(10.0, 0.25, 50.0))
        );
        let cost = estimate(
            Vendor::Claude,
            "claude-fable-5-1",
            2_000_000,
            4_000_000,
            100_000,
        );
        assert!((cost - 26.0).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn cost_is_tokens_times_price() {
        // 1M uncached at $3 + 1M cached at $0.30 + 100k out at $15.
        let c = estimate(
            Vendor::Claude,
            "claude-sonnet-5-5",
            1_000_000,
            1_000_000,
            100_000,
        );
        assert!((c - 4.8).abs() < 1e-9, "{c}");
    }

    #[test]
    fn a_dated_id_is_priced_as_its_family_and_an_unknown_one_as_zero() {
        assert_eq!(
            price_of(Vendor::Claude, "claude-haiku-4-5-20251001"),
            price_of(Vendor::Claude, "claude-haiku-4-5")
        );
        assert!(price_of(Vendor::Claude, "claude-haiku-4-5").is_some());
        assert_eq!(estimate(Vendor::Codex, "gpt-unknown", 1_000_000, 0, 0), 0.0);
        assert_eq!(estimate(Vendor::Fake, "fake-1", 1_000_000, 1, 1), 0.0);
    }
}
