# Reference resource readiness implementation plan

> Execute inline with superpowers:executing-plans. Existing fix, verification, publication and deployment authorization applies.

**Goal:** Reject unreadable native references before video dispatch and expose confirmed provider download failures without guessing their cause or replaying paid requests.

**Architecture:** The upload adapter validates commit acknowledgements, resolves native IDs with the bound account and reads a bounded media prefix from approved public CDN hosts. Prepared and sent payloads remain immutable. Core and Studio carry closed public failure codes while financial reconciliation remains independent.

**Tech Stack:** Existing Rust/ureq, Core Rust, Studio TypeScript; offline builds only.

**Spec:** User request to repair reference-resource download failures; observed native contract in installed TRAE CN 3.4.2.

## Global constraints

- Video adapter remains 0.1.50 / 20260811. Native resource IDs remain generation inputs.
- No automatic paid replay, fabricated zero-charge receipt, historical ledger rewrite or schema change.
- No signed URL, credentials, resource ID or provider body in public errors/logs.
- get_resource_url exposes uri_list/inner/biz_type/scale; no documented lifetime parameter. Public image signatures observed at approximately 600 seconds. Client preflight cannot extend signatures used internally after submission.
- An upper queue/expiry root cause remains unconfirmed; do not label this patch a complete upper-service expiry fix.

## Review focus

- HTTP 200 containing logical upload failure must be rejected.
- Resolve errors, expired CDN signatures, wrong media and private/redirect addresses must fail before paid submission.
- All reference probes together have a 20-second deadline and bounded reads.
- Provider resource errors stay failed with pending billing; observation/replay does not dispatch again.
- Studio retains the specific reason when later billing-only observations are generic.

## Tasks

- [x] Add failing upload-acknowledgement, resource-URL, bounded media-probe and deadline tests; implement the adapter preflight in `trae_resource_upload.rs` and call it during native preparation.
- [x] Add failing Core feedback and replay/status tests plus Studio failure persistence tests; implement closed `reference_resource_unavailable` and `reference_resource_download_failed` codes.
- [x] Run focused regressions and offline builds; request an independent review; deploy with ledger/config preservation and publish the affected repositories.
- [x] Record what was verified and explicitly retain the upper-service expiry limitation.


