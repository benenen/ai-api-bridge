-- Per-model pay-as-you-go prices ($/1M tokens), keyed by upstream model name.
-- Used to estimate spend when the upstream reports cost = $0 (subscription plans).
ALTER TABLE providers ADD COLUMN model_prices TEXT NOT NULL DEFAULT '{}';
