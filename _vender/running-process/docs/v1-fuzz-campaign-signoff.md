# v1 Fuzz Campaign And Security Signoff

This document defines the evidence artifact required before #241 can close.
It is security evidence for #241, but it is not the final v1.0.0 signoff.

Release gate status: PENDING

## Completion Rule

#241 stays open until every fuzz target below has a successful release-candidate
campaign with at least 3600 seconds of fuzzing, all required regression and
audit checks are linked, and the security reviewer records a final signed
decision.

The release-candidate commit under review must be the commit that will ship, or
the evidence must be re-run after any security-sensitive code, dependency,
workflow, or generated-proto change.

## Fuzz Campaign Evidence

Each row is machine-checked by
`crates/running-process/tests/security/fuzz_campaign_signoff.rs`. The
`minimum_seconds` column must stay at or above 3600. Replace `TBD` evidence
values with GitHub Actions run URLs and artifact names only after the campaign
has completed successfully.

The release evidence run is the `security-fuzz` workflow dispatched with
`fuzz_seconds=3600` or larger. The workflow runs one matrix job per fuzz target
so all twelve one-hour campaigns can complete in parallel. Each successful
release-dispatch job uploads `release-fuzz-evidence-<target>`, which contains a
run summary and the target corpus path. Record the workflow run URL and the
matching per-target artifact name in the table below.

| Target | minimum_seconds | release_run_url | corpus_or_artifact | status |
|---|---:|---|---|---|
| `fuzz_admin_decode` | 3600 | https://github.com/zackees/running-process/actions/runs/27405384619 | release-fuzz-evidence-fuzz_admin_decode | passed |
| `fuzz_cache_manifest_decode` | 3600 | https://github.com/zackees/running-process/actions/runs/27405384619 | release-fuzz-evidence-fuzz_cache_manifest_decode | passed |
| `fuzz_frame_decode` | 3600 | https://github.com/zackees/running-process/actions/runs/27405384619 | release-fuzz-evidence-fuzz_frame_decode | passed |
| `fuzz_framing_read` | 3600 | https://github.com/zackees/running-process/actions/runs/27405384619 | release-fuzz-evidence-fuzz_framing_read | passed |
| `fuzz_handoff_decode` | 3600 | https://github.com/zackees/running-process/actions/runs/27405384619 | release-fuzz-evidence-fuzz_handoff_decode | passed |
| `fuzz_hello_decode` | 3600 | https://github.com/zackees/running-process/actions/runs/27405384619 | release-fuzz-evidence-fuzz_hello_decode | passed |
| `fuzz_helloreply_decode` | 3600 | https://github.com/zackees/running-process/actions/runs/27405384619 | release-fuzz-evidence-fuzz_helloreply_decode | passed |
| `fuzz_pipe_name_parse` | 3600 | https://github.com/zackees/running-process/actions/runs/27405384619 | release-fuzz-evidence-fuzz_pipe_name_parse | passed |
| `fuzz_probe_framing_read` | 3600 | https://github.com/zackees/running-process/actions/runs/27405384619 | release-fuzz-evidence-fuzz_probe_framing_read | passed |
| `fuzz_probe_identity_decode` | 3600 | https://github.com/zackees/running-process/actions/runs/27405384619 | release-fuzz-evidence-fuzz_probe_identity_decode | passed |
| `fuzz_service_def_decode` | 3600 | https://github.com/zackees/running-process/actions/runs/27405384619 | release-fuzz-evidence-fuzz_service_def_decode | passed |
| `fuzz_service_name_validate` | 3600 | https://github.com/zackees/running-process/actions/runs/27405384619 | release-fuzz-evidence-fuzz_service_name_validate | passed |

All twelve campaigns ran 61 minutes each on commit `f0c2db1` via
`workflow_dispatch` with `fuzz_seconds=3600` (2026-06-12), with zero crashes
and one `release-fuzz-evidence-<target>` artifact per target.

## Required Release Evidence

| Evidence item | Required value | Current value |
|---|---|---|
| release_candidate_commit | Full Git commit SHA under review | TBD |
| security_fuzz_workflow_run | Successful `security-fuzz` workflow run with `fuzz_seconds=3600` | TBD |
| cargo_audit_run | Successful `cargo audit --deny warnings` run for the same commit | TBD |
| security_test_run | Successful `soldr cargo test -p running-process --test security --features client` for the same commit | TBD |
| cve_regression_run | Successful CVE-class regression tests for the same commit | TBD |
| dependency_surface_review | `docs/v1-dependency-surface.md` reviewed against `crates/running-process/Cargo.toml` | TBD |
| unsafe_inventory_review | Broker `unsafe` inventory reviewed against current source | TBD |
| privileged_operation_review | Spawn, handle-pass, pipe-create, credential, and filesystem authority reviewed | TBD |
| input_boundary_review | Wire, manifest, service-definition, pipe-name, and version-validation inputs reviewed | TBD |

## Reviewer Signoff

| Field | Required value | Current value |
|---|---|---|
| reviewer_name | Human reviewer name or GitHub handle | TBD |
| reviewer_affiliation | Maintainer, external reviewer, or organization | TBD |
| review_date | ISO-8601 date | TBD |
| reviewed_commit | Full Git commit SHA matching the release candidate | TBD |
| final_decision | `approved` after all evidence is complete | pending |
| reviewer_notes | Link to review notes, issue comment, or PR thread | TBD |

## Closure Checklist

- [ ] Every fuzz campaign row has a successful release-run URL.
- [ ] Every fuzz campaign row has a corpus or artifact reference.
- [ ] Every fuzz campaign row has status `passed`.
- [ ] Every required release evidence row has a non-`TBD` current value.
- [ ] Reviewer signoff fields are complete.
- [ ] `final_decision` is `approved`.
