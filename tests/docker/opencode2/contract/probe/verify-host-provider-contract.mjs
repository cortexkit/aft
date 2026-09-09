import { access, readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const scenarios = process.argv.slice(2);
if (scenarios.length === 0) scenarios.push("read/T1", "write/T1");
const probeDir = dirname(fileURLToPath(import.meta.url));
const contractPath = join(probeDir, "..", "host-provider-config.json");

function reportFailure(reason) {
  for (const scenario of scenarios) console.error(`FAIL ${scenario} ${reason}`);
  process.exit(1);
}

for (const scenario of scenarios) {
  if (!/^[^/]+\/T1$/.test(scenario)) reportFailure("host_provider_config_invalid:t1_scenario");
}

let contract;
try {
  contract = JSON.parse(await readFile(contractPath, "utf8"));
} catch (error) {
  if (error?.code === "ENOENT") reportFailure("contract_uncaptured:host_provider_config");
  reportFailure(`host_provider_config_invalid:${error.message}`);
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
    reportFailure(`host_provider_config_invalid:${field}`);
  }
}

if (contract.run_id !== contract.observed_run_id) {
  reportFailure("host_provider_config_invalid:observed_run_id");
}
if (contract.beta_version !== contract.host_version) {
  reportFailure("host_provider_config_invalid:host_version");
}
if (!/^[0-9a-f]{40}$/.test(contract.observed_sha)) {
  reportFailure("host_provider_config_invalid:observed_sha");
}

const expectedProvider = {
  name: "Deterministic aimock",
  package: "@opencode-ai/ai/providers/openai-compatible",
  settings: {
    baseURL: "http://127.0.0.1:4010/v1",
    apiKey: "{env:OPENAI_API_KEY}",
  },
  models: {
    "mock-model": {
      name: "Mock Model",
    },
  },
};
if (JSON.stringify(contract.opencode_json?.providers?.openai) !== JSON.stringify(expectedProvider)) {
  reportFailure("host_provider_config_invalid:opencode_json.providers.openai");
}
if (!Array.isArray(contract.run_command) || !contract.run_command.includes("openai/mock-model")) {
  reportFailure("host_provider_config_invalid:run_command");
}
if (contract.observed_session?.model?.providerID !== "openai") {
  reportFailure("host_provider_config_invalid:observed_session.model.providerID");
}
if (contract.missing_contract_code !== "contract_uncaptured:host_provider_config") {
  reportFailure("host_provider_config_invalid:missing_contract_code");
}

for (const transcript of [contract.transcript, contract.guard_transcript]) {
  try {
    await access(join(probeDir, "..", transcript));
  } catch {
    reportFailure(`host_provider_config_invalid:${transcript}`);
  }
}

for (const scenario of scenarios) {
  console.log(`PASS ${scenario} host_provider_config ${contract.beta_version} ${contract.run_id}`);
}
