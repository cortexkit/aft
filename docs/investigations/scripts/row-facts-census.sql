-- Bind :start_ms and :end_ms to inclusive epoch-millisecond cutoffs.
-- Open opencode.db with SQLite URI mode=ro and enable query_only before running.
-- This is the exact extraction query used by row-facts-spike.py. The next-five
-- ordering, row parsing, matching, and classification happen in Python.
SELECT
  p.id,
  p.message_id,
  p.session_id,
  p.time_created,
  s.directory,
  pr.worktree AS repository,
  json_extract(p.data, '$.tool') AS tool,
  json_extract(p.data, '$.state.status') AS status,
  json_extract(p.data, '$.state.input') AS input_json,
  json_extract(p.data, '$.state.output') AS output_text,
  json_extract(p.data, '$.state.error') AS error_json
FROM part AS p
JOIN session AS s ON s.id = p.session_id
JOIN project AS pr ON pr.id = s.project_id
WHERE p.time_created BETWEEN :start_ms AND :end_ms
  AND json_extract(p.data, '$.tool') IS NOT NULL
  AND json_extract(p.data, '$.state.status') IN ('completed', 'error')
ORDER BY p.session_id, p.time_created, p.id;

-- Repository volumes used to select the three largest cross-codebase strata.
SELECT
  pr.worktree AS repository,
  COUNT(*) AS completed_searches,
  COUNT(DISTINCT p.session_id) AS sessions
FROM part AS p
JOIN session AS s ON s.id = p.session_id
JOIN project AS pr ON pr.id = s.project_id
WHERE p.time_created BETWEEN :start_ms AND :end_ms
  AND json_extract(p.data, '$.tool') = 'aft_search'
  AND json_extract(p.data, '$.state.status') = 'completed'
GROUP BY pr.worktree
ORDER BY completed_searches DESC;
