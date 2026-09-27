-- The price we saw at the moment of the decision.
--
-- Without it the slippage threshold cannot be tuned: `skip:slippage_exceeded`
-- recorded the single fact of a refusal and stored the miss nowhere, while an
-- empty book arrived in the same bucket via an `unwrap_or(1.0)` substitution and
-- was indistinguishable from genuine slippage. NULL here means "that side of the
-- book did not exist", not "the price was zero": an invented price would have
-- silently ruined the whole statistic at once.

ALTER TABLE signals ADD COLUMN best_ask numeric(8,6);
ALTER TABLE signals ADD COLUMN best_bid numeric(8,6);
