# Fleet CI contract v1

[Soldr #3345](https://github.com/zackees/soldr/issues/3345) requires one shared
implementation with repository adapters. This document identifies the first
selector implementation; it does not claim that the fleet rollout is complete.

The portable standard-library helper is `.github/scripts/ci_mode.py` in
`zackees/soldr`. Its output schema is `fleet-ci-mode/v1`: `selector_schema`,
`mode`, `checkout_sha`, and `author_permission`. The same helper is tested
against all nine fleet repository identities. Adapters must execute this source
from an explicitly pinned, reviewed, merged Soldr commit, rather than copying
and modifying the selection logic. Record that full 40-character revision in
the adapter and verify it in the consumer workflow. Soldr's own PR selector
executes the trusted base-repository revision.

A consumer checks out its candidate separately from the pinned policy. It calls
`python3 <policy-checkout>/.github/scripts/ci_mode.py --checked-out-sha <candidate>`
with `GITHUB_EVENT_NAME`, `GITHUB_EVENT_PATH`, `GITHUB_REPOSITORY`,
`GITHUB_OUTPUT`, and a read-only `GITHUB_TOKEN`. Exact-SHA dispatch additionally
passes `--candidate-sha <40-hex>`. The event must be the real consumer event;
substituting the policy repository's event would query the wrong permissions.
The script writes the normalized outputs for the consumer graph.

| Event | Mode | Candidate |
| --- | --- | --- |
| Trusted PR without literal mode labels | minimal | PR head SHA |
| Trusted PR with `ci-test` | test | PR head SHA |
| PR with `ci-full` | full | PR head SHA |
| PR without effective base-repo write access, or unknown permission | full | PR head SHA |
| Default-branch push | minimal | event `after` SHA |
| Explicit validation dispatch | full | required full candidate SHA |

Full wins over test, fast, and skip labels. Re-evaluate on label addition,
removal, synchronization, reopening, and rerun. Query permission again each
time; association, fork status, prior contributions, and bot identity confer
no trust. Unknown permission remains visible and selects full. Main/master
changes never imply publication.

Each adapter must still declare nonempty test-tier cells and its complete
platform, architecture, toolchain, ABI, board, artifact, and native-execution
coverage. Every required full cell must succeed on one candidate identity;
skipped, missing, failed, cancelled, neutral, or wrong-SHA cells fail. Use a
stable summary context and require it in branch protection after its existence
on the default branch is proven. Source checks and local unit tests are not
live coverage evidence.

Remaining shared-mechanism work includes portable coverage and whole-event cost
interfaces, issue-driven release directives, retry/frozen-artifact behavior,
Bosn-owned Act execution, consumer pins and adapters, and cross-repository live
conformance. The selector tests alone do not satisfy those requirements. Each
repository must record same-SHA label add/remove runs, complete `ci-full` PR and
exact-SHA dispatch proof, external-author proof, and separate PR/default-branch
whole-event cost measurements before its rollout can be declared complete.
