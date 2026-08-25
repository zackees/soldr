# Deterministic multipart scheduler model

Fixture: `multipart_scheduler_fixture.json`.  The executable model uses two
equally sized assets, one-tick parts, and a global Bulk cap of sixteen. A
correct run must never exceed sixteen admissions and must alternate asset
grants while both jobs have ready parts.

| Per-asset window | Peak global admissions | Makespan (ticks) | Throughput (parts/tick) | First 8 grants |
| --- | ---: | ---: | ---: | --- |
| 1 | 2 | 32 | 2.0 | alpha, beta, alpha, beta, alpha, beta, alpha, beta |
| 4 | 8 | 8 | 8.0 | alpha, beta, alpha, beta, alpha, beta, alpha, beta |
| 16 | 16 | 4 | 16.0 | alpha, beta, alpha, beta, alpha, beta, alpha, beta |
| adaptive | 16 | 5 | 12.8 | alpha, beta, alpha, beta, alpha, beta, alpha, beta |

Congestion lane: after the first adaptive completion the affected origin
halves its window (4 → 2) and observes its configured Retry-After cooldown;
the model records no new admission for that origin during the cooldown. This
makes the additive-increase/multiplicative-decrease transition observable
without introducing host scheduling variance.

These are deterministic scheduler expectations, not wall-clock throughput
claims.  The production coordinator test asserts the same alternation and
admission-release properties.
