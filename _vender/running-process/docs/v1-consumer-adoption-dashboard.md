# v1 Consumer Adoption Dashboard

This dashboard mirrors the cross-consumer tracker in
[#242](https://github.com/zackees/running-process/issues/242) for repo-local
documentation. The consumer tracker issues remain the source of truth; this
document records the current narrowed-regime status from those trackers.

## Final state (2026-06-12)

The minimal-regime consumer adoption work is complete and every first-party
consumer has shipped a release on top of running-process 4.2.0:

| Consumer | Released version | Broker payload protocol | Adoption regime |
|---|---|---|---|
| soldr | 0.7.54 | — (daemon-side `BackendHandle` probe) | opt-in, direct fallback retained |
| zccache | 1.11.22 | `0x7A63` registered | opt-in, direct IPC fallback retained |
| fbuild | 2.2.27 | — (diagnostics/direct-fallback seam) | opt-in, direct fallback retained |
| clud | 2.1.0 | `0x7C4C` registered | opt-in, direct fallback retained |

All consumers adopted the broker **opt-in** with `RUNNING_PROCESS_DISABLE=1` (or
the equivalent direct daemon path) retained as the correctness fallback.
Default-on rollout and escape-hatch removal are **explicitly deferred** — see the
Phase 7 / Phase 8 disposition in [v1-rollout-policy.md](v1-rollout-policy.md) and
[#388](https://github.com/zackees/running-process/issues/388).

The remaining deferred broker-completion work formerly tracked under
[#354](https://github.com/zackees/running-process/issues/354) is now carried by
the triage meta [#421](https://github.com/zackees/running-process/issues/421) and
its sub-issues — second-pass integration SDK
([#412](https://github.com/zackees/running-process/issues/412)), runpm supervisor
([#222](https://github.com/zackees/running-process/issues/222)), and process
observation ([#221](https://github.com/zackees/running-process/issues/221)) — plus
the rollout disposition in #388. No deferred #228 work is left untracked.

## Consumers

| Consumer | Tracker issue | Adoption summary |
|---|---|---|
| soldr-daemon | [zackees/soldr#718](https://github.com/zackees/soldr/issues/718) (closed minimal) | Minimal current-regime path is merged through soldr#721/#722/#723/#724. Active `BackendHandle` probing, local `.servicedef` install, `RUNNING_PROCESS_DISABLE=1` fallback, and a managed-zccache `session-start` daemon-loss retry are present; package/postinstall install, `connect_to_backend`, broad matrix, lint/dylint, and rollout remain deferred. |
| zccache | [zackees/zccache#698](https://github.com/zackees/zccache/issues/698) (closed minimal) | Minimal current-regime path is merged through zccache#708/#709. Direct daemon identity probing uses `BackendHandle` when identity is available, and `RUNNING_PROCESS_DISABLE=1` keeps direct IPC fallback; package `.servicedef`, full broker client, published-crate/full-matrix evidence, and full prost/Frame migration remain deferred. |
| clud | [zackees/clud#308](https://github.com/zackees/clud/issues/308) (closed minimal) | Current-regime clud work is diagnostics-only. clud#319 reports direct daemon fallback, previews the canonical `clud.servicedef` path, and explicitly reports broker client wiring as deferred. |
| fbuild | [FastLED/fbuild#510](https://github.com/FastLED/fbuild/issues/510) (closed minimal) | Minimal current-regime path is merged through fbuild#529/#530. fbuild has a service metadata/direct-fallback seam and diagnostics-only service-definition preview; active `BackendHandle`, real binary package install, `connect_to_backend`, and broad acceptance remain deferred. |

## Current Minimal-Regime Snapshot

This section records the latest landed adoption evidence so the repo-local
dashboard does not lag the #242 issue comments.

| Item | Latest landed evidence | Dashboard impact |
|---|---|---|
| [zackees/soldr#721](https://github.com/zackees/soldr/pull/721) / [#722](https://github.com/zackees/soldr/pull/722) / [#723](https://github.com/zackees/soldr/pull/723) / [#724](https://github.com/zackees/soldr/pull/724) | soldr added active `BackendHandle` endpoint probing, local `soldr-daemon.servicedef` writing/install during direct daemon spawn, `RUNNING_PROCESS_DISABLE=1` direct fallback, and a one-shot retry when managed zccache `session-start` loses the daemon mid-request. | soldr#718 is closed minimal; package/postinstall install coverage and broker-client wiring stay deferred. |
| [zackees/zccache#708](https://github.com/zackees/zccache/pull/708) / [#709](https://github.com/zackees/zccache/pull/709) | zccache added the minimal `BackendHandle` daemon probe path and then made `RUNNING_PROCESS_DISABLE=1` bypass that probe for direct IPC fallback. | zccache#698 is closed minimal; published crate/full matrix, package `.servicedef`, `connect_to_backend`, and full prost/Frame migration stay deferred. |
| [FastLED/fbuild#529](https://github.com/FastLED/fbuild/pull/529) / [#530](https://github.com/FastLED/fbuild/pull/530) | fbuild added a service metadata/direct-fallback seam, service-definition template stub, and diagnostics-only `fbuild daemon running-process --json` / `servicedef --json` preview. | FastLED/fbuild#510 is closed minimal; real binary install, active `BackendHandle`, and broker-client wiring stay deferred. |
| [zackees/clud#319](https://github.com/zackees/clud/pull/319) | clud added diagnostics-only `clud daemon running-process --json` plus the `servicedef` alias, reporting direct daemon fallback and deferred broker client wiring. | zackees/clud#308 is closed minimal under the current regime; real broker adoption remains deferred. |

## Milestone Dashboard

Status language matches #242 and records the current narrowed regime. "Closed
minimal" means the consumer tracker was closed after the minimal working or
diagnostic surface landed; it does not mean full v1 broker adoption is done.

| Consumer | Tracker issue | BackendHandle | .servicedef | Broker client | Default-on | Escape-hatch removal |
|---|---|---|---|---|---|---|
| soldr-daemon | [zackees/soldr#718](https://github.com/zackees/soldr/issues/718) (closed minimal) | Active probe merged in [zackees/soldr#721](https://github.com/zackees/soldr/pull/721); managed-zccache daemon-loss retry merged in [zackees/soldr#724](https://github.com/zackees/soldr/pull/724); release + 3-OS acceptance deferred | CLI install merged in [zackees/soldr#722](https://github.com/zackees/soldr/pull/722); package/postinstall auto-install deferred | `RUNNING_PROCESS_DISABLE=1` honored in [zackees/soldr#723](https://github.com/zackees/soldr/pull/723); `connect_to_backend` deferred | Deferred / stubbed on [#238](https://github.com/zackees/running-process/issues/238) | Deferred / stubbed on [#239](https://github.com/zackees/running-process/issues/239) |
| zccache | [zackees/zccache#698](https://github.com/zackees/zccache/issues/698) (closed minimal) | Minimal `BackendHandle` daemon probe merged in [zackees/zccache#708](https://github.com/zackees/zccache/pull/708); published crate + full matrix deferred | Stubbed/deferred; no package servicedef yet | `RUNNING_PROCESS_DISABLE=1` honored in [zackees/zccache#709](https://github.com/zackees/zccache/pull/709); direct IPC fallback works; `connect_to_backend` deferred | Deferred / stubbed on [#238](https://github.com/zackees/running-process/issues/238) | Deferred / stubbed on [#239](https://github.com/zackees/running-process/issues/239) |
| clud | [zackees/clud#308](https://github.com/zackees/clud/issues/308) (closed minimal) | Diagnostics-only direct daemon fallback merged in [zackees/clud#319](https://github.com/zackees/clud/pull/319); `BackendHandle` deferred | Diagnostics preview for canonical `clud.servicedef` merged in [zackees/clud#319](https://github.com/zackees/clud/pull/319); binary/package install deferred | [zackees/clud#319](https://github.com/zackees/clud/pull/319) reports `broker_client_wired: false` and direct fallback; `connect_to_backend` deferred | Deferred / stubbed on [#238](https://github.com/zackees/running-process/issues/238) | Deferred / stubbed on [#239](https://github.com/zackees/running-process/issues/239) |
| fbuild | [FastLED/fbuild#510](https://github.com/FastLED/fbuild/issues/510) (closed minimal) | Minimal direct-fallback seam merged in [FastLED/fbuild#529](https://github.com/FastLED/fbuild/pull/529); active `BackendHandle` probe deferred | Template stub merged in [FastLED/fbuild#529](https://github.com/FastLED/fbuild/pull/529); diagnostics preview merged in [FastLED/fbuild#530](https://github.com/FastLED/fbuild/pull/530); binary package install deferred | Diagnostics-only direct fallback in [FastLED/fbuild#530](https://github.com/FastLED/fbuild/pull/530); `connect_to_backend` deferred until stable broker APIs are worth pinning | Deferred / stubbed on [#238](https://github.com/zackees/running-process/issues/238) | Deferred / stubbed on [#239](https://github.com/zackees/running-process/issues/239) |

## Dependency Gates

| Gate | Milestone | Dashboard rule |
|---|---|---|
| [#232 Phase 2.5 BackendHandle](https://github.com/zackees/running-process/issues/232) | BackendHandle | Minimal consumer slices landed where useful; full downstream migration and three-OS runtime signoff remain deferred. |
| [#235 Phase 4 broker](https://github.com/zackees/running-process/issues/235) | Broker client | Consumers record direct fallback or diagnostics-only status; `connect_to_backend` and Hello-skip wiring remain deferred. |
| [#238 Phase 7 rollout](https://github.com/zackees/running-process/issues/238) | Default-on | Default-on rollout is explicitly re-deferred; disposition tracked in [#388](https://github.com/zackees/running-process/issues/388) and [v1-rollout-policy.md](v1-rollout-policy.md). |
| [#239 Phase 8 escape-hatch removal](https://github.com/zackees/running-process/issues/239) | Escape-hatch removal | Escape-hatch removal is explicitly re-deferred; `RUNNING_PROCESS_DISABLE=1` stays load-bearing per [#388](https://github.com/zackees/running-process/issues/388). |

Full `.servicedef` package/install coverage remains deferred unless the
consumer row above names a local CLI/template/diagnostic surface as already
merged. Binary package install is not considered complete in this regime.

## Related Docs

- [Integrate your daemon (one-page quickstart)](INTEGRATE.md)
- [clud consumer adoption guide](consumer-adoption-clud.md)
- [zccache consumer adoption guide](consumer-adoption-zccache.md)
- [soldr consumer adoption guide](consumer-adoption-soldr.md)
- [fbuild consumer adoption guide](consumer-adoption-fbuild.md)
- [v1 rollout policy](v1-rollout-policy.md)
- [v1 escape hatch](v1-escape-hatch.md)
