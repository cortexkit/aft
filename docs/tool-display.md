# Tool display delivery

When tool displays are enabled, AFT sends provider-authored records, details and
completions to the single live display server with `display.write`, without
delaying tool replies. AFT also announces each record on the CortexKit bus; the
display server can fetch the same data with `display.read` after receiving that
wake. Existing pushes remain unchanged.

Before announcing a record, AFT commits the exact `display.read` reply bytes to
its private `aft.db` delivery cache. Reads check memory first, then that cache,
under the existing route checks: the caller must be the daemon-attested module
uniquely declaring `tool-display/v1`, on an admitted management route rather than
a project-free or ordinary tool route. Restarting AFT does not lose an announced
record's durable read window.

Acknowledged wakes protect their records through ten minutes after the bus
acknowledgement. Unacknowledged wakes, including uncertain publish outcomes,
protect their records for 24 hours after the latest attempted announcement.
Memory may retain a record longer while its push remains live. Expired or
never-retained identifiers return `display_record_unknown`; no wakes are replayed
from the cache after restart.

The durable cache holds at most 4,096 records and 16 MiB of pull bytes. AFT reclaims
expired rows first and never evicts protected rows. When protected records fill
the cache, AFT defers new wake announcements. It logs the first deferral with the
cache's count and byte usage, and how many wakes it deferred once it admits a
record again; existing `display.write` pushes continue independently. Bounded expiry
sweeps run on writable database opens and the regular retention tick. Read-only
or older databases without the optional cache use memory only.

This optional cache adds no numbered database migration and changes neither
`schema_version` nor the reader floor (the per-store minimum format versions a
build must support to open the storage root). Older AFT builds ignore the cache, so rolling
back remains a binary swap.
