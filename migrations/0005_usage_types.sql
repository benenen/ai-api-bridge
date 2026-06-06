-- Generalize usage from cost-only to typed (billing/count/token …).
-- Providers gain a `usage` JSON column (Vec<UsageSpec>); legacy cost_windows/
-- model_prices columns stay and are folded into a Billing spec at load.
ALTER TABLE providers ADD COLUMN usage TEXT NOT NULL DEFAULT '[]';

-- usage_events become typed: rename the dollar `cost` to a generic `amount` and
-- tag each event with its usage kind (existing rows are billing $).
ALTER TABLE usage_events RENAME COLUMN cost TO amount;
ALTER TABLE usage_events ADD COLUMN usage_type TEXT NOT NULL DEFAULT 'billing';
