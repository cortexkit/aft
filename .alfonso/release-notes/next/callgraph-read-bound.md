With views on, a call-graph read no longer waits for the whole call graph to rebuild after files change; it answers "building" within a few seconds, and the next read uses the new graph.
