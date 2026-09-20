// Root V2 server entrypoint for directory-path plugin loading.
//
// OpenCode 2 tries `<dir>/server` before `<dir>/index` when the plugin target
// is a directory, so this is the file that makes a checkout load as a V2
// plugin rather than falling back to the V1 default export. See the note in
// index.js for why the exports map does not cover this case.
export * from "./dist/entry/server.js";
export { default } from "./dist/entry/server.js";
