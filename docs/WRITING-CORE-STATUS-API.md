# Writing Core Status API v1

This document describes the local v1 integration contract. Status remains a
stable read-only view; the two loopback-only mutation routes are narrow editor
adapter inputs owned by Writing Core.

## Endpoint

```text
GET http://127.0.0.1:43180/v1/status
```

The server is owned by the Writing Monitor Tauri process and binds only to
`127.0.0.1`. It is not a LAN service and has no CORS policy or authentication.
Writing Monitor may continue to run normally if this port is already occupied;
the API is then unavailable and the process logs the bind failure.

The API is available only while Writing Monitor is running. It is not a
systemd service and does not leave a separate server process behind.

## Response schema

Successful responses are `200 OK` with `Content-Type: application/json`.

```json
{
  "schema_version": 1,
  "service_instance_id": "7d3c1e57-4d10-4f7e-9fd2-6b4f1f6a56c1",
  "revision": 1,
  "tracking": false,
  "activity_state": "inactive",
  "document": null,
  "session": null
}
```

When a Markdown document is tracked but no real正文 change has started a
session, the response has this shape:

```json
{
  "schema_version": 1,
  "service_instance_id": "7d3c1e57-4d10-4f7e-9fd2-6b4f1f6a56c1",
  "revision": 2,
  "tracking": true,
  "activity_state": "inactive",
  "document": {
    "id": "opaque-document-id",
    "name": "mvp-smoke.md",
    "path_hash": "sha256-of-canonical-path",
    "writing_count": 8471
  },
  "session": null
}
```

An active session adds the following facts:

```json
{
  "schema_version": 1,
  "service_instance_id": "7d3c1e57-4d10-4f7e-9fd2-6b4f1f6a56c1",
  "revision": 3,
  "tracking": true,
  "activity_state": "active",
  "document": {
    "id": "opaque-document-id",
    "name": "mvp-smoke.md",
    "writing_count": 8478
  },
  "session": {
    "id": "opaque-session-id",
    "started_at": "2026-08-15T08:21:08Z",
    "span_seconds": 4821,
    "active_seconds": 3510,
    "idle_seconds": 1311,
    "since_last_edit_seconds": 8,
    "writing_inserted_units": 1840,
    "writing_deleted_units": 378,
    "writing_net_units": 1462
  }
}
```

`started_at` is an RFC3339 UTC timestamp. Writing unit fields are nullable
when the stored session predates Writing Count v1; clients must not interpret
`null` as zero. `document.writing_count` is the current Writing Count v1 total
for the tracked Markdown document and is nullable only if the current Core
state cannot provide it.

## Field semantics

- `schema_version` is the protocol version. It is `1` in this contract.
- `service_instance_id` is a UUID generated when the Writing Core process
  starts. It remains unchanged for that process lifetime and changes after a
  full process restart. It is not a PID.
- `tracking` is true only while a Markdown path is actively configured in the
  current Writing Monitor process.
- `document` is null when no Markdown document is being tracked. It contains a
  safe display name, not the absolute path or正文 text.
- `document.path_hash` is an opaque SHA-256 identity used by trusted local
  adapters to compare their current path without exposing the absolute path.
- `session` is null before the first accepted real正文 change for the current
  tracked document. It contains the latest Core session while tracking,
  including a session whose state is `ended`.
- `session.id` is the stable opaque SQLite session identity. It is not derived
  from `started_at`.
- `writing_inserted_units`, `writing_deleted_units`, and `writing_net_units`
  use Writing Count v1. RAW Unicode scalar statistics are not part of this
  public contract.

## Activity state

`activity_state` is always one of:

- `inactive`: not tracking, or tracking has not yet observed a real正文 edit.
- `active`: a session exists and the last edit is inside the idle threshold.
- `idle`: a session exists and the last edit is past the idle threshold but
  has not reached the session-end threshold.
- `ended`: the latest session reached the session-end threshold. The response
  retains that ended session until a later edit creates a new session.

The API does not expose the Core-internal `waiting` label. It maps that
pre-session condition to `inactive`.

## Revision

`revision` starts at `1` for the initial no-tracking state and is monotonic for
the lifetime of the process. It increases when the observable Core facts have
a discrete change, including:

- tracking starts, stops, or switches documents;
- a first accepted正文 edit starts a session;
- a new accepted edit changes the accumulated edit facts;
- `active`/`idle`/`ended` changes;
- a new session is created.

It does not increase for:

- another `GET /v1/status` request;
- client polling frequency;
- `span_seconds` or `since_last_edit_seconds` naturally advancing with the
  wall clock;
- a repeated request with no discrete Core fact change.

The API reads a read-only Core status snapshot. It does not refresh
`last_edit_at`, create an edit event, or write SQLite merely because a client
polls. Core may still persist a lifecycle transition through its normal
non-API operations.

## Editor adapter routes

`POST /v1/track` selects the one explicit Markdown document tracked by the
Core. Its JSON body is `{ "path": "/absolute/canonical/document.md" }`.
It reuses the same canonical-path validation, initial disk snapshot, document
identity, Core state, and file watcher as Standalone Monitor selection.

`POST /v1/snapshot` accepts a stable full Markdown snapshot from an editor
adapter. Its JSON body contains `path`, `content`, fixed adapter `source`,
`editor_instance_id`, monotonic per-instance `editor_revision`, and an optional
client `timestamp_ms`. Core uses its own receive time for activity/session
policy. Requests are capped at 1 MiB of content;正文 is kept only in the existing
in-memory snapshot path and is never logged.

Accepted live snapshots and later file-watch saves share the same latest
snapshot/hash baseline. Saving the exact live content therefore updates the
disk metadata without recording a second EditEvent. A different editor instance
has its own revision sequence; this is identity/deduplication, not CRDT or
collaborative merging.

## Errors and privacy

Normal business states such as `tracking: false`, `document: null`, and
`session: null` return `200 OK`. Unknown paths return `404`; unsupported methods
return `405`; invalid adapter payloads return `400`/`409`/`413` as applicable.

Status responses contain no正文 content, diff payload, inserted/deleted text,
Markdown snapshot, absolute正文 path, SQLite access, or desktop-pet behavior.
Consumers must use this HTTP contract and must not open the Writing Core
SQLite database or watch the Markdown file themselves.

## Compatibility

Adding optional fields without changing existing meanings may remain within
schema version 1. Removing a field, changing a field type, changing its
meaning, or changing null semantics requires a schema version change. No
negotiation endpoint is implemented.

## curl

```bash
curl -sS http://127.0.0.1:43180/v1/status
```
