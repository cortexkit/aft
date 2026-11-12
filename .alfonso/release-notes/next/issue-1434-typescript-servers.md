TypeScript packages in a repository now share one language server per resolved
SDK version, including copied TypeScript 7 installations. Packages pinned to a
different SDK retain their own diagnostics. A new `lsp.max_servers` setting
(default 8, project config may only lower it) limits live clients and starts in
progress. AFT evicts the least recently used idle client first; when all clients
are busy, inspection reports partial coverage and retries on the next request.
