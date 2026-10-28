### Flow-callable AFT diagnostics

AFT now serves `diagnostics.findings`, a read-only query for agent heads and
daemon-authorized flows. It reports standing and recent problems using evidence
AFT already records, without running repairs or scheduling executor work.
Worker routes cannot call it. The optional `window_ms` defaults to six hours
and accepts one millisecond through seven days; invalid windows are rejected.
Replies are severity-ordered and bounded to less than 64 KiB, with at most five
samples per finding. A bounded reply is not an exhaustive evidence ledger.
