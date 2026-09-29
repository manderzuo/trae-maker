# Reference assets in v2 budgets

The v2 planner validates uploaded asset ownership, size and SHA-256 before
account selection. The native upload repeats the size/digest check. Core's
mirror Key is the billing identity; assets remain owned by the authenticated
bridge credential after Core has checked the ordinary user's ownership.

Image and video IDs cannot be interchanged. Reference videos require local
uploaded MP4 bytes, not an arbitrary URL, TOS URI or caller-supplied duration.
The initial metadata adapter accepts non-fragmented MP4 with one video track
(and optional audio), checks box boundaries and matching video mdhd/stts durations,
and conservatively includes the longest decode or presentation timeline.
Normal unit-rate edit lists (including leading empty edits) are supported;
their movie timescale and aggregate durations are bounded independently.
Edited audio may include priming/padding in its sample table; its full decoded
duration is retained even when the edit list trims playback. Audio timing
mismatches without an edit list and shorter sample tables remain rejected.
Fragmented media, retimed edit lists, WebM, ambiguous/malformed timing and over-60-second aggregate references are
rejected explicitly before paid dispatch. This is a bounded container metadata
adapter, not a general-purpose media decoder.

The reference total is rounded up to seconds. For example, a 5.042-second
reference changes the pricing profile from
`seedance2-fast:480p:16:9:5s:images0:video1` to
`seedance2-fast:480p:16:9:5s:images0:video1:ref_seconds6`.
Count-only video-reference policies are never accepted.

An administrator must configure the exact profile in data-root
`bridge-budget-policy.json`, version 1, with `hold_credits`, `policy_version`,
`source`, and future integer `expires_at_ms`. These are finite `policy_only`
risk budgets, not official prices, quotes or final charges. Unknown profiles
are not silently priced as text-to-video. Native-estimate support still only
uses captured exact workload mappings. All charges require verified receipts.

Reference input errors return HTTP 400 with one of
`reference_video_budget_metadata_required`, `reference_video_metadata_invalid`,
`reference_video_format_unsupported`, `reference_asset_type_mismatch` or
`reference_asset_unavailable`. Missing/expired administrator policies remain
configuration errors, separate from invalid client data and capacity errors.

2026-09-29 public tail-reference acceptance used original image plus a
verified parent tail: `seedance2-fast:480p:16:9:5s:images2:video0`.
Request `request_vxaGh0i2QQhr6CFE3XiVpg` had matched final video cost56.208;
the temporary bounded test hold124 was replaced locally with hold62
(10% buffer, rounded up), expiring1791150800000. This is one calibrated
sample, not an official maximum or calibration for other reference counts.
The helper uses the existing built-in2-credit bounded allowance, separately
settled by its exact receipt; adding an unused `assist:` policy is unnecessary.
Operators must refresh expired policies or collect a new bounded calibration;
do not delete references, freeze an entire Key, or treat missing prices as free.

The native uploader uses `override_resource_id` when provided, falling back to
`store_uri` and removing the query suffix, matching the installed native
remote-attachment uploader. The PUT storage location is not necessarily the
generation service's resource identifier. Images retain their raw-image upload
contract. Generation reference videos use `biz_type=video` and raw MP4 bytes,
not the generic `remote_resource` Magic V2 envelope. A real non-generating
probe confirmed that the video namespace resolves through the default resource
lookup to an MP4; the generic attachment namespace did not return an HTTPS
resource through that lookup. A retrievable chat attachment alone does not
prove that the generation backend can consume it.
