import { afterAll } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { delimiter, join, resolve } from "node:path";
import { isolatedAftEnvironment } from "../../test-child-environment.js";
import {
  assertAftDatabaseUnchanged,
  liveAftDatabasePath,
  snapshotAftDatabase,
} from "./storage-canary.js";

// target/ is disposable but not the OS temp namespace: permission E2E tests
// must not receive the temp-directory exemption for their project fixtures.
const scratch = resolve(import.meta.dir, "../../../../../target");
mkdirSync(scratch, { recursive: true });
const root = mkdtempSync(join(scratch, "bun-test-home-"));
const database = liveAftDatabasePath();
const before = snapshotAftDatabase(database);
Object.assign(process.env, isolatedAftEnvironment(root));
process.env.AFT_TEST_ISOLATION_ROOT = root;
// Both the suite home and an outer test-driver home can live under target/.
// Stop discovery there without hiding repositories created inside fixtures.
const gitCeilings = process.env.GIT_CEILING_DIRECTORIES;
process.env.GIT_CEILING_DIRECTORIES = gitCeilings ? `${scratch}${delimiter}${gitCeilings}` : scratch;
delete process.env.AFT_ALLOW_PRODUCTION_MIGRATION;

// Register at preload time, before any test can spawn a bridge. Missing stores
// are snapshots too: creating the operator's first aft.db must turn this red.
afterAll(() => assertAftDatabaseUnchanged(database, before));
process.once("exit", () => rmSync(root, { recursive: true, force: true }));
