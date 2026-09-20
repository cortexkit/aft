// Root entrypoint for directory-path plugin loading.
//
// A host given a package NAME resolves through the exports map in
// package.json. A host given a DIRECTORY path does not: OpenCode 2 resolves
// `<dir>/server` then `<dir>/index` literally (see `resolve()` in
// @opencode/plugin dist/host.js), and swallows every resolution error, so a
// package without these files is skipped silently — no load attempt, no
// warning, nothing in the log to explain it.
//
// Local development points at a checkout rather than an installed package, so
// these shims are what make `"plugin": ["/path/to/packages/opencode-plugin"]`
// work. They are one line each and change nothing for installed consumers,
// whose resolution goes through the exports map either way.
export { default } from "./dist/index.js";
