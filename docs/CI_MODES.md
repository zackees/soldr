# CI modes

The `CI` workflow selects one mode for each event. An ordinary pull request or
push to `main` runs `Lint` and the Linux x64 prescribed host validation
(`soldr ci-test`). The Linux host job is not the whole platform matrix.

Add the `ci-test` label to run the extended Linux x64 target E2E cell and
the macOS ARM64 archive replay on a hosted `macos-15` runner. Remove it to
return to minimal coverage on the same PR head SHA. This does not run the
complete platform matrix.

Add the `ci-full` label to a pull request when it changes platform-sensitive
code, cross-target behavior, the toolchain, sysroots, wheels, target execution,
or release packaging. Label add/remove and new commits recompute the mode for
the PR head SHA. `ci-full` takes precedence over the older `fast-build` label
and `ci-test` and path-based Windows/wheel policies. Full mode runs every CI
target in `ci/canonical-targets.json` and the platform smoke jobs. `Full
coverage` fails when a required job is skipped, failed, cancelled, or missing.
It also rejects a supported target with no execution job. Full mode replays
the macOS x64 archive on hosted `macos-15-intel` and the ARM64 archive on
hosted `macos-15`. These runner allocations occur only for explicit labels
or exact-SHA release validation; ordinary unlabeled PR and main runs remain
Mac-free.

For a release candidate, dispatch `CI` with `candidate_sha` set to its full
40-character commit SHA. The mode job checks out and verifies that SHA, then
passes it to every checkout and cross-build caller. A release process must wait
for a successful `Full coverage` result on that same SHA before publication.

The normal `Lint` and `Linux x64` status names remain stable for branch
protection. A full run additionally reports `Full coverage`. The runner-minute
budget in [soldr#3344](https://github.com/zackees/soldr/issues/3344) is for all
workflows caused by the event; it needs Actions job-duration measurements and
cannot be inferred from this workflow's job count.
