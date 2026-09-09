# aft_search follow-up failure rubric v1

Classify an episode as a search failure only when the later grep-family call exposes an acted-on file that the preceding `aft_search` should have returned within its declared corpus and `includeTests` scope. Keep `not_a_search_failure`, `scope_mismatch`, and unresolved `other` episodes out of the failure numerator unless the retained evidence independently satisfies that rule.

## Estimator assumptions

The conditional labels are modeled as independent Bernoulli observations within each sampled stratum, with the normalized census weights represented by the effective sample size. The measured-window discriminating fraction D/N is modeled as an independent binomial proportion. The two samples are collected by separate procedures, so covariance is fixed to zero. The projected delta-method variance is `q² Var(p_w) + p_w² Var(q)`. Reports call this an approximation and make no significance or improvement claim.
