import { access } from "node:fs/promises";
import { join } from "node:path";

import { fail } from "./errors.js";
import { asRecord, readJson } from "./util.js";

export interface HandoffContract {
  kind: "env" | "flag" | "header";
  name: string;
}

export interface HostCliContract {
  schema_version: 1;
  host_version: string;
  observed_run_id: string;
  endpoint_handoff: { run: HandoffContract; api: HandoffContract };
  password_handoff: { run: HandoffContract; api: HandoffContract };
  session_start: Record<string, unknown>;
  idle_retention: Record<string, unknown>;
  /** The flag `opencode api` takes a request body on. */
  request_body_flag: string;
  shared_server_smoke: {
    method: string;
    path: string;
    expected_status?: number;
  };
}

export interface HostProviderConfigContract {
  schema_version: 1;
  host_version: string;
  observed_run_id: string;
  provider_config: Record<string, unknown>;
  /** The opencode.json key the observed provider object was accepted under. */
  config_key: string;
  model: string;
}


export interface HostSchemaRejectionContract {
  schema_version: 1;
  host_version: string;
  observed_run_id: string;
  agent_visible_text: string;
  json_event: Record<string, unknown>;
}

function handoff(value: unknown, label: string): HandoffContract {
  const record = asRecord(value);
  if (
    !record ||
    !["env", "flag", "header"].includes(String(record.kind)) ||
    typeof record.name !== "string" ||
    record.name.length === 0
  ) {
    fail("contract_uncaptured", `${label} is invalid`, { contract: label }, true);
  }
  return record as unknown as HandoffContract;
}

function contractIdentity(
  record: Record<string, unknown>,
  expectedVersion: string,
  label: string,
): { hostVersion: string; runId: string } {
  const hostVersion = record.host_version ?? record.beta_version;
  const runId = record.observed_run_id ?? record.run_id;
  if (hostVersion !== expectedVersion || typeof runId !== "string" || runId.length === 0) {
    fail(
      "contract_uncaptured",
      `${label} must carry the pinned host version and observed run id`,
      { expected_version: expectedVersion, observed_version: hostVersion, observed_run_id: runId },
      true,
    );
  }
  return { hostVersion, runId } as { hostVersion: string; runId: string };
}

export async function loadHostCliContract(
  contractRoot: string,
  expectedVersion: string,
): Promise<HostCliContract> {
  const path = join(contractRoot, "host-cli-contract.json");
  await access(path).catch(() => {
    fail("contract_uncaptured", "host_cli_contract", { path }, true);
  });
  const record = asRecord(await readJson(path));
  if (record?.schema_version !== 1)
    fail("contract_uncaptured", "host_cli_contract schema", { path }, true);
  const identity = contractIdentity(record, expectedVersion, "host_cli_contract");
  const endpoint = asRecord(record.endpoint_handoff);
  const password = asRecord(record.password_handoff);
  const smoke = asRecord(record.shared_server_smoke);
  const bodyHandoff = asRecord(record.request_body_handoff);
  const missingFields = [
    ...(!endpoint ? ["endpoint_handoff"] : []),
    ...(!asRecord(endpoint?.run) ? ["endpoint_handoff.run"] : []),
    ...(!asRecord(endpoint?.api) ? ["endpoint_handoff.api"] : []),
    ...(!password ? ["password_handoff"] : []),
    ...(!asRecord(password?.run) ? ["password_handoff.run"] : []),
    ...(!asRecord(password?.api) ? ["password_handoff.api"] : []),
    ...(!asRecord(record.session_start) ? ["session_start"] : []),
    ...(!asRecord(record.idle_retention) ? ["idle_retention"] : []),
    ...(typeof bodyHandoff?.api !== "string" ? ["request_body_handoff.api"] : []),
    ...(typeof smoke?.method !== "string" ? ["shared_server_smoke.method"] : []),
    ...(typeof smoke?.path !== "string" ? ["shared_server_smoke.path"] : []),
  ];
  if (missingFields.length > 0) {
    fail(
      "contract_uncaptured",
      "host_cli_contract fields",
      { path, missing_fields: missingFields },
      true,
    );
  }
  return {
    schema_version: 1,
    host_version: identity.hostVersion,
    observed_run_id: identity.runId,
    endpoint_handoff: {
      run: handoff(endpoint?.run, "endpoint_handoff.run"),
      api: handoff(endpoint?.api, "endpoint_handoff.api"),
    },
    password_handoff: {
      run: handoff(password?.run, "password_handoff.run"),
      api: handoff(password?.api, "password_handoff.api"),
    },
    session_start: record.session_start as Record<string, unknown>,
    idle_retention: record.idle_retention as Record<string, unknown>,
    request_body_flag: bodyHandoff?.api as string,
    shared_server_smoke: {
      method: smoke?.method as string,
      path: smoke?.path as string,
      expected_status:
        typeof smoke?.expected_status === "number" ? smoke.expected_status : undefined,
    },
  };
}

/**
 * The provider object a host generation was observed to accept, and where.
 *
 * Both the shape and the key it goes under are host-generation specific, so
 * each generation has its own captured file rather than one shape reused by
 * assumption. `config_key` is read back off the captured `opencode_json` so
 * the harness writes the key the transcript actually shows working.
 */
async function readProviderConfigContract(
  path: string,
  pinnedVersion: string,
  label: string,
): Promise<HostProviderConfigContract> {
  await access(path).catch(() => {
    fail("contract_uncaptured", label, { path }, true);
  });
  const record = asRecord(await readJson(path));
  if (record?.schema_version !== 1) {
    fail("contract_uncaptured", `${label} schema`, { path }, true);
  }
  contractIdentity(record, pinnedVersion, label);
  if (!asRecord(record.provider_config)) {
    fail("contract_uncaptured", `${path}: provider_config observation is missing`);
  }
  const opencodeJson = asRecord(record.opencode_json);
  const configKeys = Object.keys(opencodeJson ?? {}).filter(
    (key) => key !== "$schema" && key !== "plugin",
  );
  if (configKeys.length !== 1) {
    fail(
      "contract_uncaptured",
      `${path}: opencode_json must show exactly one provider key`,
      { observed_keys: configKeys },
      true,
    );
  }
  const runCommand = record.run_command;
  const modelFlag = Array.isArray(runCommand) ? runCommand.indexOf("--model") : -1;
  const model = Array.isArray(runCommand) ? runCommand[modelFlag + 1] : undefined;
  if (modelFlag === -1 || typeof model !== "string" || model.length === 0) {
    fail("contract_uncaptured", `${path}: run_command model observation is missing`);
  }
  return { ...record, config_key: configKeys[0], model } as unknown as HostProviderConfigContract;
}

export async function loadHostProviderConfigContract(
  contractRoot: string,
  pinnedVersion: string,
): Promise<HostProviderConfigContract> {
  return readProviderConfigContract(
    join(contractRoot, "host-provider-config.json"),
    pinnedVersion,
    "host_provider_config",
  );
}

/** The same observation for the V1 host, which the T7 parity trajectory also runs. */
export async function loadV1HostProviderConfigContract(
  contractRoot: string,
  pinnedVersion: string,
): Promise<HostProviderConfigContract> {
  return readProviderConfigContract(
    join(contractRoot, "host1-provider-config.json"),
    pinnedVersion,
    "host1_provider_config",
  );
}


export async function loadHostSchemaRejectionContract(
  contractRoot: string,
  expectedVersion: string,
): Promise<HostSchemaRejectionContract> {
  const path = join(contractRoot, "host-schema-rejection.json");
  await access(path).catch(() => {
    fail(
      "contract_uncaptured",
      "host_schema_rejection",
      { path, validation: "contract_uncaptured:host_schema_rejection" },
      true,
    );
  });
  const record = asRecord(await readJson(path));
  if (record?.schema_version !== 1) {
    fail("contract_uncaptured", "host_schema_rejection schema", { path }, true);
  }
  const identity = contractIdentity(record, expectedVersion, "host_schema_rejection");
  const observation = asRecord(record.observation);
  const observedInstance = asRecord(record.observed_instance);
  const agentVisibleContract = asRecord(record.agent_visible_contract);
  const agentVisibleText =
    record.agent_visible_text ??
    observation?.agent_visible_text ??
    observedInstance?.json_event_error;
  const jsonEvent = asRecord(
    record.json_event ?? observation?.json_event ?? agentVisibleContract?.json_event,
  );
  if (typeof agentVisibleText !== "string" || agentVisibleText.length === 0 || !jsonEvent) {
    fail("contract_uncaptured", "host_schema_rejection observations", { path }, true);
  }
  return {
    schema_version: 1,
    host_version: identity.hostVersion,
    observed_run_id: identity.runId,
    agent_visible_text: agentVisibleText,
    json_event: jsonEvent,
  };
}

export function applyHandoff(
  handoffContract: HandoffContract,
  value: string,
  args: string[],
  env: NodeJS.ProcessEnv,
): void {
  if (handoffContract.kind === "flag") args.push(handoffContract.name, value);
  else if (handoffContract.kind === "env") env[handoffContract.name] = value;
  else {
    const current = env.OPENCODE_API_HEADERS ? JSON.parse(env.OPENCODE_API_HEADERS) : {};
    env.OPENCODE_API_HEADERS = JSON.stringify({ ...current, [handoffContract.name]: value });
  }
}
