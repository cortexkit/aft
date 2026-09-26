// Run with EXEC_COUNT_LOG=/tmp/aft-exec.jsonl bun test --preload /absolute/path/to/this/file.
// Count newly created executable files, including copies that preserve executable mode.
const fs = require("node:fs");
const path = require("node:path");
const { appendFileSync, lstatSync, statSync } = fs;
const output = process.env.EXEC_COUNT_LOG;
if (!output) throw new Error("Set EXEC_COUNT_LOG to an absolute JSONL output path");
const created = new Set();
const reported = new Set();
const key = (file) => String(file);
const exists = (file) => {
  try { lstatSync(file); return true; } catch { return false; }
};
const isExecutable = (file) => {
  try {
    const stat = lstatSync(file);
    // Shared-library fixtures are loaded as data, not launched as programs.
    return stat.isFile() && !/\.so(?:\.|$)/.test(key(file)) && (stat.mode & 0o111) !== 0;
  } catch { return false; }
};
function record(file, operation) {
  file = key(file);
  if (!reported.has(file) && isExecutable(file)) {
    reported.add(file);
    appendFileSync(output, JSON.stringify({ operation, path: file }) + "\n");
  }
}
function recordTree(file, operation) {
  if (isExecutable(file)) record(file, operation);
  else if (exists(file) && lstatSync(file).isDirectory()) {
    for (const child of fs.readdirSync(file)) recordTree(path.join(key(file), child), operation);
  }
}
for (const name of ["writeFileSync", "copyFileSync", "cpSync"]) {
  const original = fs[name];
  fs[name] = function (...args) {
    const dest = name === "writeFileSync" ? args[0] : args[1];
    const fresh = !exists(dest);
    const result = original.apply(this, args);
    if (fresh) {
      created.add(key(dest));
      recordTree(dest, name);
    }
    return result;
  };
}
const chmod = fs.chmodSync;
fs.chmodSync = function (file, mode, ...rest) {
  const result = chmod.call(this, file, mode, ...rest);
  if (created.has(key(file))) record(file, "chmodSync");
  return result;
};
for (const name of ["copyFile", "cp"]) {
  const original = fs[name];
  fs[name] = function (src, dest, ...rest) {
    const fresh = !exists(dest);
    const callback = rest.pop();
    return original.call(this, src, dest, ...rest, (error) => {
      if (!error && fresh) recordTree(dest, name);
      callback(error);
    });
  };
}
for (const name of ["writeFile", "copyFile", "cp"]) {
  const original = fs.promises[name];
  fs.promises[name] = async function (...args) {
    const dest = name === "writeFile" ? args[0] : args[1];
    const fresh = !exists(dest);
    const result = await original.apply(this, args);
    if (fresh) {
      if (name === "writeFile") created.add(key(dest));
      recordTree(dest, `promises.${name}`);
    }
    return result;
  };
}
const chmodAsync = fs.promises.chmod;
fs.promises.chmod = async function (file, mode) {
  const result = await chmodAsync.call(this, file, mode);
  if (created.has(key(file))) record(file, "promises.chmod");
  return result;
};
