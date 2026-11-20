# Occipital notice-record samples

These two app replies are copied byte for byte from
`crates/occipital-app-reader/test-vectors/phone-reader-notice-records-v1.1/`
in Occipital at tag `phone-reader-notice-records-v1.1` (commit
`7ee5a144c1a4b2c1ed3c9fa74e357b9ffe28e08c`). The SHA-256 values are from
the tag's `SHA256SUMS` and are checked by AFT's notice sample test.

```text
f6a3c1c6df70a10a79b1abdbc2c4f59e17b8e6fc6edb3cf1c86719b7b233c0ae  02-wake-records-with-notices.json
fc698d032e5b7f8e3ba8d2abf10c62335c5861b5f77b15fcdc3fcc50fb330ee6  05-notice-views.json
```

The sample set defines its own view names, data schemas, and template content.
AFT uses the sample to pin the shared wire shapes while keeping its own card
facts and templates. `05-notice-views.json` is the display server's reply, not
the provider's bucket value.
