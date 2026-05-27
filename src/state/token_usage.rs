use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct TokenUsage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_creation_tokens: u64,
    pub(crate) cost_usd: f64,
}

impl TokenUsage {
    pub(crate) fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_creation_tokens
    }

    pub(crate) fn compact_display(&self) -> String {
        let total_k = self.total_tokens() as f64 / 1000.0;
        if self.cost_usd >= 0.01 {
            format!("{:.0}k/${:.2}", total_k, self.cost_usd)
        } else {
            format!("{:.0}k", total_k)
        }
    }
}
