import { access, readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const scenario = process.argv[2] ?? "read/T2/invalid_arguments";
const probeDir = dirname(fileURLToPath(import.meta.url));
const contractPath = join(probeDir, "..", "host-schema-rejection.json");

function fail(reason) {
  console.error(`FAIL ${scenario} ${reason}`);
  process.exit(1);
}

let contract;
try {
  contract = JSON.parse(await readFile(contractPath, "utf8"));
} catch (error) {
  if (error?.code === "ENOENT") fail("contract_uncaptured:host_schema_rejection");
  fail(`host_schema_rejection_invalid:${error.message}`);
}

const requiredStrings = [
  "run_id",
  "observed_run_id",
  "observed_sha",
  "beta_version",
  "host_version",
  "transcript",
  "guard_transcript",
];
for (const field of requiredStrings) {
  if (typeof contract[field] !== "string" || contract[field].length === 0) {
    fail(`host_schema_rejection_invalid:${field}`);
  }
}

if (contract.run_id !== contract.observed_run_id) {
  fail("host_schema_rejection_invalid:observed_run_id");
}
if (contract.beta_version !== contract.host_version) {
  fail("host_schema_rejection_invalid:host_version");
}
if (!/^[0-9a-f]{40}$/.test(contract.observed_sha)) {
  fail("host_schema_rejection_invalid:observed_sha");
}
if (contract.probe?.t2_subcase !== scenario || contract.probe?.error_origin !== "host") {
  fail("host_schema_rejection_invalid:t2_subcase");
}
if (contract.agent_visible_contract?.json_event?.type !== "tool_use") {
  fail("host_schema_rejection_invalid:json_event.type");
}
if (contract.agent_visible_contract?.json_event?.state_status !== "error") {
  fail("host_schema_rejection_invalid:json_event.state_status");
}
if (contract.agent_visible_contract?.model_handoff?.error_type !== "tool.execution") {
  fail("host_schema_rejection_invalid:model_handoff.error_type");
}
if (contract.agent_visible_contract?.message?.suffix !== "Update the arguments and call the tool again.") {
  fail("host_schema_rejection_invalid:message.suffix");
}

for (const transcript of [contract.transcript, contract.guard_transcript]) {
  try {
    await access(join(probeDir, "..", transcript));
  } catch {
    fail(`host_schema_rejection_invalid:${transcript}`);
  }
}

console.log(`PASS ${scenario} host_schema_rejection ${contract.beta_version} ${contract.run_id}`);
