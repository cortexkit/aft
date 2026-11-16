# Tool display delivery

When tool displays are enabled, AFT sends provider-authored records, details and
completions to the single live display server with `display.write`, without
delaying tool replies. AFT also announces each record on the CortexKit bus; the
display server can fetch the same data with `display.read` after receiving that
wake. Existing pushes remain unchanged.

Records remain readable throughout push delivery and for at least ten minutes
after the wake is acknowledged. Bounded-memory pressure can evict older records
earlier, and discarded optional details become unavailable immediately. Expired,
evicted or discarded records return `display_record_unknown`.

Nothing is stored on disk. A restart loses all retained display records; reads
of those identifiers return `display_record_unknown` and no wakes are replayed.
