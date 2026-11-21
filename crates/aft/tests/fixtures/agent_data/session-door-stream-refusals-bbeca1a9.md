| code | methods | meaning | next |
| --- | --- | --- | --- |
| `invalid_request` | `session.admit`, `session.cover`, `session.read`, `session.offsets`, `session.offsets_watch` | A parameter is missing, malformed or out of range; fix `detail.field` before resending. | Stop |
| `caller_forbidden` | `session.admit`, `session.cover`, `session.read`, `session.offsets`, `session.offsets_watch` | The caller needs a current, unscoped route binding with a daemon-attested module principal. | Stop |
| `stream_label_too_long` | `session.admit`, `session.cover`, `session.read`, `session.offsets`, `session.offsets_watch` | The caller's module ID makes a stream label longer than 64 bytes; use a shorter module ID. | Stop |
| `unavailable` | `session.admit`, `session.cover`, `session.read`, `session.offsets`, `session.offsets_watch` | Storage, cloud access, authentication or complete range verification failed; retry after service or verified data is available. | Retry |
| `record_too_large` | `session.admit`, `session.read` | The payload or read limit exceeds `detail.cap`; send a smaller admission or page. | Stop |
| `response_too_large` | `session.admit`, `session.cover`, `session.read`, `session.offsets`, `session.offsets_watch` | The serialized reply exceeds `detail.cap`; the operation may have completed. Check offsets or state before a narrower request or repairing the oversized error source. | Stop |
| `offset_mismatch` | `session.admit` | The admission does not start at the current durable end; resend from `detail.expectedOffset`. | Stop |
| `lease_acquiring` | `session.admit` | Lease acquisition is ambiguous or takeover verification is pending; clears after acquisition and import finish. | Retry |
| `lease_unavailable` | `session.admit`, `session.cover`, `session.read` | Cloud work needs enrollment, a cloud target or a successful cloud exchange; retry once those are available (reads do not acquire a lease). | Retry |
| `lease_held_elsewhere` | `session.admit` | Another device holds an unexpired lease; stop this writer and use the holder or arrange a handoff. | Stop |
| `lease_not_writable` | `session.admit` | The local lease is in a non-writable terminal or handoff state; stop this writer and recover on the rightful holder. | Stop |
| `container_handed_off` | `session.admit`, `session.offsets_watch` | This device handed off the container; stop this writer and continue on the receiver. | Stop |
| `cut_pending_handoff` | `session.admit` | A source cut awaiting handoff or a verified receiver cut awaiting acceptance fences admission; stop this writer. A source can clear the fence with `cut.abort` before handoff starts; on the receiver it clears on `cut.accept` or `cut.abort`. | Stop |
| `offset_out_of_range` | `session.read` | The read starts past the stream or cut end; resend at or below `detail.streamEnd`. | Stop |
| `below_floor` | `session.read` | The read starts below the committed trim floor; stop a cut-bound import because the requested bytes are no longer available through the door. For an unbound read, restart at `detail.floor` using its authenticated midstate. | Stop |
| `cut_not_published` | `session.read` | The local cut is not readable, or no local row or published cloud manifest can serve this ID. Unlike `cut.read`, `session.read` does not return `not_found` for a missing cut. | Stop |
| `cut_import_pending` | `session.read` | The receiver has not imported this stream yet; retry after takeover progresses. | Retry |
| `cut_takeover_closed` | `session.read` | The receiver's takeover failed permanently before importing this stream; do not retry this cut. | Stop |
| `cut_range_not_local` | `session.read` | This computer no longer holds the hub cut's full stream range; stop reading it here and use the receiving computer once it has imported the cut. | Stop |
| `verify_failed` | `session.read` | The cut manifest fails authentication, historical roster or cutId binding checks; do not import it. | Stop |
| `cut_item_missing` | `session.read` | The verified manifest has no item for this session and caller's stream; use a cut that includes it. | Stop |
