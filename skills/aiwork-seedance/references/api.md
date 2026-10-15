# Runner contract

The runner maps to the AI Work gateway as follows:

| Runner action | Gateway request | Side effect |
|---|---|---|
| `doctor` | `GET /health` | none |
| `upload` | `POST /v1/assets` | stores one explicitly selected asset |
| `submit` | `POST /v1/videos/generations` with `Idempotency-Key` | creates one video task |
| `status` | `GET /v1/videos/{task_id}` | none |
| `wait` | repeated `status` requests | none |
| `download` | `GET /v1/videos/{task_id}/content` | writes a local MP4 |

Asset uploads are limited by the gateway (currently 32 MiB per file). A local
path is read only when the user supplied it. A URL returned by the gateway is
treated as untrusted data and is not opened automatically unless the user
requested a download.

## Reference lifetime and independent tasks

`ImagePath` / `VideoPath` are read and uploaded for every independent submission,
immediately before its single generation POST. Earlier `ImageAssetId` /
`VideoAssetId` values resolve to source records saved by `upload`, scoped to the
configured gateway and credential. The original file's SHA256 and media type
must still match; the runner then uploads it again and uses the new ID. It
does not extend an old server handle or depend on a canvas connection staying
visible. Unregistered IDs require an explicitly selected source path.

All selected sources are checked before the first upload. The established
parameter ordering is preserved: asset-ID references in their supplied order,
followed by local-path references in their supplied order, separately for
images and videos. Use a single ordered `ImagePath` list to interleave tail
frames and character references precisely. Images are limited to nine for the
current Seedance 2.0 Fast model.

Source records and submission receipts are stored under the current user's
`APPDATA/AIWork/seedance-references`, with a gateway-and-credential scope tag.
They contain source paths/digests and task receipts, never API keys or prompts.
Each idempotency key is durably claimed before uploads. Successful duplicate
calls return the original task receipt without a second upload or generation.
Concurrent, changed, incomplete and unknown-outcome submissions are blocked;
there is no paid POST retry in this client. Do not delete these receipts to
work around an unknown outcome.

HTTP errors expose only an allowlisted gateway code, numeric HTTP status and a
validated request ID. Raw provider text, signed URLs, credentials and prompts
are not included. Asset upload failure or lost response never triggers another
generation automatically.
