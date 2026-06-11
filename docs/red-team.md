# Red-team suite

The red-team suite is the security acceptance gate for the memory substrate. It is
ordinary Rust test code, so a failing probe fails CI, and each probe produces a
structured report instead of a free-form log line. The report shape lives in
`aionforge-redteam` and records the task, probe name, full denominator, observed
attack successes, naive-baseline successes, the binding ceiling, rates, and the
pass/fail status.

M6.T04 establishes the convention with three structural probes:

- **Query-only memory injection** checks that hostile search text does not mutate
  memory, get reflected into rendered recall, or break the recall wrapper.
- **Poisoned-RAG recall** stores a malicious memory containing tag breakouts and
  asserts that both full and compact recall render it as escaped untrusted data.
- **Malicious skill promotion** saves a hostile skill, submits quorum-shaped signed
  attestations using the skill id, and asserts the promotion gate treats it as not
  applicable because only facts promote to `global`.

The M6.T04 structural ceiling is zero. A raw wrapper breakout, query-only write or
reflection, or skill-to-global promotion is a security failure, not a number to tune.
The report still carries a naive-baseline count over the same denominator so the
release gate can show how far the substrate is from the raw-splice baseline without
shipping a vulnerable implementation. An empty probe is a failed probe; a report has to
measure at least one attempt before it can pass.

Later probes add to the same crate rather than inventing separate report formats:
M6.T05 should add effect-size reports for subliminal-trait transfer, M6.T06 should
add signature, clock-skew, and extraction probes, and M8.T06 should aggregate the
reports into the single release gate.
