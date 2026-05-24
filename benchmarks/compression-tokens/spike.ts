import Tokenizer from "ai-tokenizer";
import * as claudeEncoding from "ai-tokenizer/encoding/claude";

type SpikeOutputEntry = {
  file: string;
  command: string;
  category: string;
  tier: string;
  original_bytes: number;
  compressed_bytes: number;
  original_text: string;
  compressed_text: string;
};

type Measurement = SpikeOutputEntry & {
  a_pre_tokens: number;
  a_post_tokens: number;
  a_saved_tokens: number;
  b35_pre_tokens: number;
  b35_post_tokens: number;
  b35_saved_tokens: number;
  b35_drift_pct: number | null;
  b40_pre_tokens: number;
  b40_post_tokens: number;
  b40_saved_tokens: number;
  b40_drift_pct: number | null;
};

const BENCH_DIR = new URL(".", import.meta.url).pathname.replace(/\/$/, "");
const REPO_ROOT = new URL("../..", import.meta.url).pathname.replace(/\/$/, "");
const DATA_PATH = `${BENCH_DIR}/data/spike-output.json`;
const REPORT_PATH = `${BENCH_DIR}/REPORT.md`;

const tokenizer = new Tokenizer(claudeEncoding);

function estimateTokens(text: string): number {
  if (!text) return 0;
  return tokenizer.count(text);
}

function ratioTokens(text: string, bytesPerToken: number): number {
  return Buffer.byteLength(text, "utf8") / bytesPerToken;
}

function driftPct(approxSaved: number, preciseSaved: number): number | null {
  if (preciseSaved === 0) return null;
  return ((approxSaved - preciseSaved) / preciseSaved) * 100;
}

function percentile(values: number[], p: number): number {
  if (values.length === 0) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  const idx = Math.ceil((p / 100) * sorted.length) - 1;
  return sorted[Math.max(0, Math.min(sorted.length - 1, idx))];
}

function median(values: number[]): number {
  if (values.length === 0) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  if (sorted.length % 2 === 1) return sorted[mid];
  return (sorted[mid - 1] + sorted[mid]) / 2;
}

function mean(values: number[]): number {
  if (values.length === 0) return 0;
  return values.reduce((sum, value) => sum + value, 0) / values.length;
}

function fmt(n: number, digits = 1): string {
  return Number.isFinite(n) ? n.toFixed(digits) : "n/a";
}

function fmtDrift(n: number | null): string {
  return n === null ? "n/a" : `${fmt(n)}%`;
}

function aggregate(rows: Measurement[], bytesPerToken: 3.5 | 4.0) {
  const preciseSaved = rows.reduce((sum, row) => sum + row.a_saved_tokens, 0);
  const approxSaved = rows.reduce(
    (sum, row) => sum + (bytesPerToken === 3.5 ? row.b35_saved_tokens : row.b40_saved_tokens),
    0,
  );
  const drift = driftPct(approxSaved, preciseSaved) ?? 0;
  const perFixtureDrifts = rows
    .map((row) => (bytesPerToken === 3.5 ? row.b35_drift_pct : row.b40_drift_pct))
    .filter((value): value is number => value !== null);
  const absoluteDrifts = perFixtureDrifts.map((value) => Math.abs(value));
  const savedBytes = rows.reduce(
    (sum, row) => sum + Math.max(0, row.original_bytes - row.compressed_bytes),
    0,
  );
  return {
    count: rows.length,
    preciseSaved,
    approxSaved,
    drift,
    meanDrift: mean(perFixtureDrifts),
    medianDrift: median(perFixtureDrifts),
    p95AbsDrift: percentile(absoluteDrifts, 95),
    maxAbsDrift: absoluteDrifts.length ? Math.max(...absoluteDrifts) : 0,
    calibratedBytesPerSavedToken: preciseSaved > 0 ? savedBytes / preciseSaved : 0,
  };
}

function markdownTable(headers: string[], rows: string[][]): string {
  return [
    `| ${headers.join(" | ")} |`,
    `| ${headers.map(() => "---").join(" | ")} |`,
    ...rows.map((row) => `| ${row.join(" | ")} |`),
  ].join("\n");
}

function runCompressionTest() {
  const proc = Bun.spawnSync({
    cmd: ["cargo", "test", "--test", "compress_spike", "--", "--nocapture"],
    cwd: REPO_ROOT,
    stdout: "pipe",
    stderr: "pipe",
  });
  if (proc.exitCode !== 0) {
    process.stderr.write(proc.stdout.toString());
    process.stderr.write(proc.stderr.toString());
    throw new Error(`cargo compression spike failed with exit code ${proc.exitCode}`);
  }
}

runCompressionTest();

const raw = await Bun.file(DATA_PATH).text();
const entries = JSON.parse(raw) as SpikeOutputEntry[];

const tokenizationStart = performance.now();
const measurements: Measurement[] = entries.map((entry) => {
  const a_pre_tokens = estimateTokens(entry.original_text);
  const a_post_tokens = estimateTokens(entry.compressed_text);
  const a_saved_tokens = a_pre_tokens - a_post_tokens;
  const b35_pre_tokens = ratioTokens(entry.original_text, 3.5);
  const b35_post_tokens = ratioTokens(entry.compressed_text, 3.5);
  const b35_saved_tokens = b35_pre_tokens - b35_post_tokens;
  const b40_pre_tokens = ratioTokens(entry.original_text, 4.0);
  const b40_post_tokens = ratioTokens(entry.compressed_text, 4.0);
  const b40_saved_tokens = b40_pre_tokens - b40_post_tokens;
  return {
    ...entry,
    a_pre_tokens,
    a_post_tokens,
    a_saved_tokens,
    b35_pre_tokens,
    b35_post_tokens,
    b35_saved_tokens,
    b35_drift_pct: driftPct(b35_saved_tokens, a_saved_tokens),
    b40_pre_tokens,
    b40_post_tokens,
    b40_saved_tokens,
    b40_drift_pct: driftPct(b40_saved_tokens, a_saved_tokens),
  };
});
const tokenizationMs = performance.now() - tokenizationStart;

const displayRows = measurements.map((row) => ({
  fixture: row.file,
  tier: row.tier,
  bytes: `${row.original_bytes}->${row.compressed_bytes}`,
  a_saved: row.a_saved_tokens,
  b35_saved: fmt(row.b35_saved_tokens),
  b35_drift: fmtDrift(row.b35_drift_pct),
  b40_saved: fmt(row.b40_saved_tokens),
  b40_drift: fmtDrift(row.b40_drift_pct),
}));
console.table(displayRows);

const tiers = [...new Set(measurements.map((row) => row.tier))];
const tierRows35 = tiers.map((tier) => {
  const stats = aggregate(
    measurements.filter((row) => row.tier === tier),
    3.5,
  );
  return [
    tier,
    String(stats.count),
    fmt(stats.preciseSaved, 0),
    fmt(stats.approxSaved),
    fmt(stats.drift),
    fmt(stats.meanDrift),
    fmt(stats.medianDrift),
    fmt(stats.p95AbsDrift),
    fmt(stats.maxAbsDrift),
    fmt(stats.calibratedBytesPerSavedToken, 2),
  ];
});
const tierRows40 = tiers.map((tier) => {
  const stats = aggregate(
    measurements.filter((row) => row.tier === tier),
    4.0,
  );
  return [tier, String(stats.count), fmt(stats.approxSaved), fmt(stats.drift)];
});
const overall35 = aggregate(measurements, 3.5);
const overall40 = aggregate(measurements, 4.0);

console.log("\nPer-tier aggregate (bytes/token=3.5):");
console.log(
  markdownTable(
    ["tier", "n", "A saved", "B saved", "overall drift %", "mean drift %", "median drift %", "p95 abs drift %", "max abs drift %", "calibrated B/token"],
    tierRows35,
  ),
);
console.log("\nOverall aggregate:");
console.log(
  markdownTable(
    ["A saved", "B saved 3.5", "B drift 3.5 %", "B saved 4.0", "B drift 4.0 %", "tokenization ms"],
    [[fmt(overall35.preciseSaved, 0), fmt(overall35.approxSaved), fmt(overall35.drift), fmt(overall40.approxSaved), fmt(overall40.drift), fmt(tokenizationMs, 2)]],
  ),
);

const anyTierAbove15 = tiers.some(
  (tier) => Math.abs(aggregate(measurements.filter((row) => row.tier === tier), 3.5).drift) > 15,
);
const recommendation =
  Math.abs(overall35.drift) < 5 && tokenizationMs > 50
    ? "B"
    : anyTierAbove15
      ? "A"
      : "hybrid";
const recommendationText =
  recommendation === "B"
    ? "Ship Option B (approximate byte-ratio counting), using calibrated bytes/token values per compressor tier."
    : recommendation === "A"
      ? "Ship Option A (precise ai-tokenizer counts), with a size cap/fallback for very large blobs."
      : "Ship a hybrid: precise tokenization for short outputs and byte-ratio approximation for long outputs.";

const perFixtureMarkdownRows = measurements.map((row) => [
  row.file,
  row.tier,
  row.command.replaceAll("|", "\\|"),
  String(row.original_bytes),
  String(row.compressed_bytes),
  String(row.a_pre_tokens),
  String(row.a_post_tokens),
  String(row.a_saved_tokens),
  fmt(row.b35_saved_tokens),
  fmtDrift(row.b35_drift_pct),
  fmt(row.b40_saved_tokens),
  fmtDrift(row.b40_drift_pct),
]);

const report = `# Compression Token Counting Spike

## Methodology

This spike measures AFT bash output compression fixtures through the real Rust compression dispatch (\`compress_with_registry\`) using built-in TOML filters and Rust compressors. A Cargo integration test writes \`data/spike-output.json\`; this Bun script tokenizes each original/compressed pair with \`ai-tokenizer\` Claude encoding and compares it with byte-ratio estimates.

- Fixtures: ${measurements.length} realistic bash outputs across git, build/test, lint, filesystem, deploy/container, plus one generic fallback sample.
- Option A: precise Claude token counts using \`ai-tokenizer@^1.0.6\`.
- Option B: byte approximation using both 3.5 bytes/token and code-leaning 4.0 bytes/token.
- IPC-cost proxy: elapsed time to tokenize all original and compressed fixture texts in-process with \`ai-tokenizer\`.

## Overall Aggregate

${markdownTable(
  ["fixtures", "A saved", "B saved 3.5", "B drift 3.5 %", "B saved 4.0", "B drift 4.0 %", "tokenization ms"],
  [[String(measurements.length), fmt(overall35.preciseSaved, 0), fmt(overall35.approxSaved), fmt(overall35.drift), fmt(overall40.approxSaved), fmt(overall40.drift), fmt(tokenizationMs, 2)]],
)}

## Per-tier Breakdown (3.5 bytes/token)

${markdownTable(
  ["tier", "n", "A saved", "B saved", "overall drift %", "mean fixture drift %", "median fixture drift %", "p95 abs drift %", "max abs drift %", "calibrated bytes/saved-token"],
  tierRows35,
)}

## Per-tier 4.0 Variant

${markdownTable(["tier", "n", "B saved 4.0", "overall drift 4.0 %"], tierRows40)}

## Per-fixture Measurements

${markdownTable(
  ["fixture", "tier", "command", "orig bytes", "compressed bytes", "A pre", "A post", "A saved", "B3.5 saved", "B3.5 drift", "B4.0 saved", "B4.0 drift"],
  perFixtureMarkdownRows,
)}

## Recommendation

**Recommendation: ${recommendation}.** ${recommendationText}

Decision rule evaluation:

- B aggregate drift at 3.5 bytes/token: ${fmt(overall35.drift)}% (${Math.abs(overall35.drift) < 5 ? "passes" : "fails"} the <5% aggregate criterion).
- Total tokenization time for ${measurements.length} fixtures: ${fmt(tokenizationMs, 2)}ms (${tokenizationMs > 50 ? "passes" : "does not pass"} the >50ms IPC-cost criterion).
- Any tier over 15% aggregate drift: ${anyTierAbove15 ? "yes" : "no"}.

Calibrated byte ratios from this fixture set (saved bytes / precise saved tokens):

${markdownTable(
  ["tier", "calibrated bytes/saved-token"],
  tiers.map((tier) => {
    const stats = aggregate(measurements.filter((row) => row.tier === tier), 3.5);
    return [tier, fmt(stats.calibratedBytesPerSavedToken, 2)];
  }),
)}
`;

await Bun.write(REPORT_PATH, report);
console.log(`\nWrote ${REPORT_PATH}`);
