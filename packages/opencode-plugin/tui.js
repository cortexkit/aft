// Root TUI entrypoint for directory-path plugin loading.
//
// OpenCode 2 resolves the TUI feature at `<dir>/tui` when the plugin target is
// a directory, the same literal join it uses for `server` and `index`, and
// with the same silent skip when the file is absent — the sidebar simply never
// appears. See index.js for why the exports map does not cover this shape.
export { default } from "./src/entry/tui.mjs";
