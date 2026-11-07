# Search Benchmarks

With `search_index: true`, AFT builds a trigram index in the background and serves
grep queries from memory. Here's how it compares to ripgrep on real codebases.

## opencode-aft (253 files)

| Query | ripgrep | AFT | Speedup |
|-------|---------|-----|---------|
| `validate_path` | 31.4ms | 1.48ms | **21x** |
| `BinaryBridge` | 31.0ms | 1.3ms | **24x** |
| `fn handle_grep` | 31.3ms | 0.2ms | **136x** |
| `search_index` | 31.5ms | 0.4ms | **71x** |

## reth (1,878 Rust files)

| Query | ripgrep | AFT | Speedup |
|-------|---------|-----|---------|
| `impl Display for` | 98.9ms | 1.10ms | **90x** |
| `BlockNumber` | 61.6ms | 2.19ms | **28x** |
| `EthApiError` | 32.7ms | 1.31ms | **25x** |
| `fn execute` | 36.6ms | 2.19ms | **17x** |

## Chromium/base (3,953 C++ files)

| Query | ripgrep | AFT | Speedup |
|-------|---------|-----|---------|
| `WebContents` | 69.5ms | 0.29ms | **236x** |
| `StringPiece` | 51.8ms | 0.78ms | **66x** |
| `NOTREACHED` | 51.6ms | 2.16ms | **24x** |
| `base::Value` | 54.4ms | 1.13ms | **48x** |

Rare queries see the biggest gains — the trigram index narrows candidates to a few files instantly.
High-match queries still benefit from `memchr` SIMD scanning and early termination.

Index builds in ~2s for most projects (under 2K files). Larger codebases like Chromium/base
(~4K files) take ~2 minutes for the initial build. Once built, the index persists to disk for
instant cold starts and stays fresh via file watcher and mtime verification.

<!-- merge-queue permission probe; this draft PR is closed unmerged -->
