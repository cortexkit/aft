# Engine boundary

The engine owns the late-result bus and its notice-view readiness gate. It
tracks durable acknowledgments for the `nv1:` entries in the existing view
snapshot hook. The gate queues `PullRecord`, which is only an ID and bytes;
`aft-subc` re-exports it from `subc::rich_ui::notices` and owns the notice
protocol, admission and record encoding.

The engine display retry hands a due record to the existing announce hook.
`aft-subc` checks notice writer capability and the engine's discovered display
reader before persisting or publishing a notice. The engine still owns retry
completion and backoff. The engine depends on neither the daemon nor the tool
handlers.
