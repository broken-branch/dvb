//! Provider token values are normalized before computing cumulative deltas.
use dvb_core::provider::RunItem;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tokens {
    pub uncached_in: u64,
    pub cached_in: u64,
    pub out: u64,
}

impl Tokens {
    pub fn delta(self, before: Self) -> Self {
        Self {
            uncached_in: self.uncached_in.saturating_sub(before.uncached_in),
            cached_in: self.cached_in.saturating_sub(before.cached_in),
            out: self.out.saturating_sub(before.out),
        }
    }

    pub fn item(self) -> Option<RunItem> {
        (self != Self::default()).then_some(RunItem::Usage {
            uncached_in: self.uncached_in,
            cached_in: self.cached_in,
            out: self.out,
        })
    }
}
