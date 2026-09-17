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
