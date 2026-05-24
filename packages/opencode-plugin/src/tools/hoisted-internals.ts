/**
 * Test-only re-exports of internal helpers from hoisted.ts.
 *
 * Kept in a separate file so the production import surface of hoisted.ts
 * stays focused. Bun's test runner imports this directly; the bundled
 * plugin output never references this module.
 */

export { _buildUnifiedDiffForTest } from "./hoisted.js";
