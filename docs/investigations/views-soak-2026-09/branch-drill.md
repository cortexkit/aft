# opencode views branch-switch drill

Observed at `2026-09-12T09:02:03Z` against `5716f8ba60e7`.

- A: first-parent commit `a085bf62a459` (300 changed files)
- B: branch `refs/heads/dev` (`def7220bfc65`, 5735 changed files)
- Standalone measurement PIDs: views-on `22933`, views-off `57515`
- Running subc daemon PID (sampled separately in JSON): `62424`

`cpu_s` and `rss_delta_mb` measure the placed standalone AFT process that owns each drill watcher; the running subc daemon deltas are retained in the JSON rows.

| switch | on publication_ms | on puts | on embeds | on cpu_s | on rss_delta_mb | on correct_ms | off publication_ms | off puts | off embeds | off cpu_s | off rss_delta_mb | off correct_ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| HEAD→a085bf62a459 | — | 0 | 3 | 103.91 | -288.0 | timeout | — | — | 0 | 242.74 | 13.562 | timeout |
| a085bf62a459→HEAD | 111284 | 0 | 4 | 111.97 | 2905.906 | 4637 | — | — | 0 | 224.99 | -4.797 | timeout |
| HEAD→refs/heads/dev | — | 0 | 0 | 81.07 | -3030.219 | timeout | — | — | 0 | 214.63 | -6.328 | timeout |
| refs/heads/dev→HEAD | 212902 | 0 | 0 | 156.48 | 2407.453 | timeout | — | — | 0 | 3.11 | 10.719 | 7504 |

## Defects

- views-on HEAD→a085bf62a459 did not return both correct probes within 300 seconds
- HEAD→a085bf62a459 did not publish a new pointer generation
- a085bf62a459→HEAD reused HEAD with puts=0 and embeds=4
- views-on HEAD→refs/heads/dev did not return both correct probes within 300 seconds
- HEAD→refs/heads/dev did not publish a new pointer generation
- views-on refs/heads/dev→HEAD did not return both correct probes within 300 seconds
- views-off HEAD→a085bf62a459 did not return both correct probes within 300 seconds
- views-off a085bf62a459→HEAD did not return both correct probes within 300 seconds
- views-off HEAD→refs/heads/dev did not return both correct probes within 300 seconds
