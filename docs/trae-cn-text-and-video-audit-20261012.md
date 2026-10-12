# TRAE CN migration: text repair and video submission audit

Date: 2026-10-12, Asia/Shanghai. Scope: the already migrated local AI Work bridge, GLM assist recovery, and video submission compatibility. No Studio UI change or deployment was needed for this repair.

**Correction after source investigation:** the video audit below records the state before the video migration repair. Client 0.1.50 in continuation evidence denotes the compiled video adapter, not the installed desktop application's GUI version. `docs/seedance-continuation-capabilities.md` states this explicitly. The adapter remains 0.1.50 / 20260811; desktop login, renewal and entitlement reads remain 3.4.2 / 20250325. Relabeling evidence as 3.4.2 is unnecessary and would misidentify its contract. The repair and final acceptance evidence are tracked in `docs/video-migration-repair-20261012.md`.

## Confirmed text failure and repair

The original Core parent `request_2b9rq9OniwECm_VLhF-3gw` failed in its GLM-5.3-Flash assist step `request_KYVxbRC5xXSEO-yQbw8suA`. There was no video step. The provider returned 4001 (invalid parameter); the original financial disposition remains pending and was not rewritten or replayed.

The native TRAE CN installation identifies itself as client 3.4.2 with product version code 20250325. The existing text service previously used 20260811. A controlled A/B test kept the account, device, model, prompt and client 3.4.2 unchanged, with fresh per-request correlation/session identifiers:

| Text protocol version code | Result |
| --- | --- |
| 20250325 in body and matching headers | SSE error 4001 |
| 20260811 in body and matching headers | Completion with explicit done |

The repair separates the text service compatibility code from native desktop identity. Text generation and the existing text catalog now use one compatible 20260811 header pair; the text body uses the same code. Native login, billing metadata, resource upload and video generation retain their native identity. The requested model is preserved. The fresh request constructor avoids ureq appending a second conflicting X-* version header.

Core capacity metadata failures now log only a closed error category. Raw provider responses and credentials are not logged. This instrumentation did not report a metadata failure during the capacity investigation below.

## Verification and activation

- The initial regression reproduced body code 20250325 instead of 20260811. An initial broader filter also caught the duplicate-header implementation and two tests requiring an exclusive host lease; these were corrected and the host-lease tests rerun during a quiet activation window.
- Focused suites: payload 6, text headers 1, model catalog 6, native client 3. Two existing ciphertext/dispatch-lease tests passed while the real assistant was stopped. This is 18 focused checks, not a full Tauri suite.
- Offline locked release compilation succeeded. Activated binary SHA256: `a15e6ae0efa03de8081a6f6104dfa3d886da782aa66622292cb2080b7efb186c`.
- Real Core work flow `request_W1Zy8QOrd2tp_Z_lEna8Qg` executed GLM-5.3-Flash assist `request_NS2Zwgn5yVPihG82PPbRGg` successfully, asking for the missing selected original video. Actual settled assist fee: 0.0544 credits. No video was requested by that decision. Its persisted body uses native client 3.4.2 and text code 20260811.
- The binary was activated in the existing launcher path. Core/Studio binaries, Web frontend, native installation and financial policies were not changed. Old unresolved financial holds were retained.

Ignored deployment evidence and EXE/ledger backups are under `rollout-backup/`; they contain private runtime material and must not be staged. Studio diagnostics are under the ignored `work/video-failure-4001-20261012/` directory in the Studio checkout.

## Video audit: what is proven

The video body does not contain a text `ide_version_code`. Generation, upload and resource lookup still use native client 3.4.2 / code 20250325 headers. The text repair does not change these paths.

A non-generating A/B test of the official video fee preflight used the same account, `seedance2-fast`, function `video_generation`, and workload 225000. Both 20250325 and 20260811 returned HTTP 200, code 0, sufficient true, estimated 249.75 credits. This validates the preflight only; it does not prove video generation compatibility or actual video cost.

One authorized Core submission used a fresh fixed idempotency key, a plain static-ball prompt, 5 seconds / 480p / 16:9, and no references. It was sent once:

- Parent: `request_HwdNj4lC6ZG8wYPywrYDCg`.
- Assist: `request_ZcKqWKwwZXdIhVfynYDzOQ`, GLM-5.3-Flash, succeeded, settled 0.0368 credits.
- Core initially returned 202 / queued. That was not evidence that Seedance accepted the generation.
- Core continuation logs repeatedly reported `upstream_capacity_unavailable` before a video budget was prepared. Local bridge preparation/execution counts for the parent were both zero, and Core had no video step. No 4001 from a video model was observed.
- Capacity metadata for all five enabled accounts returned HTTP 200. Conservative available capacity is constrained by existing P/R promises and confirmed debits not proven covered by a balance snapshot. These obligations were not released to force the test through.
- The existing minimum video risk allowance may come from a recent matched price or the administrator's 397-credit fallback. There is no justification to lower this policy or erase unknown holds merely to perform a compatibility test.

The local assistant was later found absent (no process or port listener); there was no matching Windows application fault event. The reason for its exit is not established. It was restored from the verified activated EXE using the existing hidden launcher after explicit quiet recovery, which made zero network calls/paid dispatches and retained all 21 unresolved budgets. At 16:38 local status showed inflight 0, unfinished executions 0 and capacity P 0.

Only the diagnostic parent was ended before restarting the bridge, using a consistent Core SQLite backup and an atomic transaction equivalent to the existing parent/operation terminal CAS. Guards required its exact idempotency identity and its sole assist step to be succeeded/settled at 0.0368, with no admitted video step. Its request, operation and work-version execution state became failed with `upstream_capacity_unavailable`; an audit event records the diagnostic termination. Financial rows were compared before/after and left unchanged. This prevents a future capacity change from dispatching this diagnostic. Final public GET returns failed, billing settled, no video resource. Original user tasks were not modified.

## Confirmed continuation migration issue

`video-capabilities.json` still binds its evidence to client 0.1.50. The capability loader requires equality with the current native client 3.4.2. The live internal endpoint therefore returns:

```json
{"tail_reference":false,"native_first_frame":false,"native_video_extend":false,"contract_version":"not_verified"}
```

This disables advertised tail-reference/video-reference continuation after migration. It is separate from basic generation and the text 4001. Relabeling old evidence as 3.4.2 would falsely claim a new-client verification, so neither the config nor evidence was changed.

## Remaining acceptance work

1. Reconcile existing unknown/confirmed capacity obligations using authoritative session receipts and the existing capacity coverage rules, preserving actual charges and unresolved records. Investigate the queued/continuation status separately so capacity rejection is visible rather than appearing as endless queuing.
2. Once legitimate capacity allows admission, run one controlled Seedance generation on the migrated native client, inspect the actual provider response and settle its unique receipt. Do not replay the ended diagnostic or original failed video.
3. Verify migrated-login tail/reference continuation with real completed outputs and settled receipts. Preserve the unchanged adapter's 0.1.50 binding and original evidence bytes; do not bind adapter evidence to desktop display version 3.4.2. Do not claim visual acceptance from the text repair or fee preflight alone.

Conclusion: GLM assist repair is activated and demonstrated by a real Core assist. Video generation is not fully accepted: its fee preflight works, its controlled generation was blocked before submission, and continuation capability verification has a confirmed client-version mismatch.
