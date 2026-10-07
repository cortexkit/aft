# Reference policy vectors

Each JSON case pins a policy, a complete command line, PTY/stdin requirements,
and the independent expected `matches` decision. `.jcs` holds RFC 8785 bytes
(no newline); `.sha256` is their lowercase SHA-256 (no newline).

Generated and checked by the reference matcher in
`crates/prefrontal-core-module/src/remote_exec_policy.rs`. Regenerate from the
worktree root with:

```
cargo nextest run -p prefrontal-core-module --lib -E 'test(remote_exec_policy::tests::generate_policy_vectors)' --run-ignored all
```

The generator first checks every independently specified expected result;
it cannot silently publish a changed matcher decision. Normal unit tests
verify every vector against those expectations, the matcher and its digest.
Unsupported shell syntax keeps the whole line local. These vectors do not
split or execute commands; routing and local fallback belong to AFT.
