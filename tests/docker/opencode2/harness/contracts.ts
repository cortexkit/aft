import { access } from "node:fs/promises";
import { join } from "node:path";

import { fail } from "./errors.js";
import { asRecord, fileContainsText, readJson } from "./util.js";

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

/**
 * The V1 host's own code for choosing between `apply_patch` and `edit`/`write`.
 *
 * The pinned OpenCode 1 build picks one edit family or the other from the
 * model id, before any plugin is consulted, so a harness whose model is not a
 * GPT-5 name cannot be offered `apply_patch` at all. A row that leaves its V1
 * leg out of the verdict for that reason cites this capture, and the run holds
 * the capture against the installed executable so the citation cannot quietly
 * rot into a claim about a host that no longer behaves that way.
 */
export interface HostEditFamilyGateContract {
  schema_version: 1;
  host_version: string;
  observed_run_id: string;
  /** Where inside the installed package the fragments were read. */
  executable: string;
  /** The selector that reads the model id and picks the family. */
  selector_source: string;
  /** The bundle's own name for each tool the selector switches on. */
  tool_selectors: Array<{ tool: string; source: string }>;
}

export async function loadV1HostEditFamilyGateContract(
  contractRoot: string,
  pinnedVersion: string,
): Promise<HostEditFamilyGateContract> {
  const path = join(contractRoot, "host1-edit-family-gate.json");
  await access(path).catch(() => {
    fail("contract_uncaptured", "host1_edit_family_gate", { path }, true);
  });
  const record = asRecord(await readJson(path));
  if (record?.schema_version !== 1) {
    fail("contract_uncaptured", "host1_edit_family_gate schema", { path }, true);
  }
  const identity = contractIdentity(record, pinnedVersion, "host1_edit_family_gate");
  const selectors = Array.isArray(record.tool_selectors) ? record.tool_selectors : [];
  const parsedSelectors = selectors.flatMap((value) => {
    const entry = asRecord(value);
    return typeof entry?.tool === "string" && typeof entry.source === "string"
      ? [{ tool: entry.tool, source: entry.source }]
      : [];
  });
  if (
    typeof record.executable !== "string" ||
    typeof record.selector_source !== "string" ||
    record.selector_source.length === 0 ||
    parsedSelectors.length !== selectors.length ||
    parsedSelectors.length === 0
  ) {
    fail(
      "contract_uncaptured",
      "host1_edit_family_gate observations",
      { path, observed_selectors: parsedSelectors.length },
      true,
    );
  }
  return {
    schema_version: 1,
    host_version: identity.hostVersion,
    observed_run_id: identity.runId,
    executable: record.executable,
    selector_source: record.selector_source,
    tool_selectors: parsedSelectors,
  };
}

/**
 * Hold the captured gate against the executable this run would have used.
 *
 * A capture that is no longer in the binary means the reason a row gives for
 * leaving its V1 leg out is a claim about some other build. That is worth
 * ending the run over: every other outcome would be judged against a host the
 * exclusion was never written for.
 */
export async function assertV1HostEditFamilyGate(
  contract: HostEditFamilyGateContract,
  executable: string | undefined,
): Promise<void> {
  if (!executable) {
    fail(
      "contract_uncaptured",
      "host1_edit_family_gate cannot be checked without the pinned V1 executable",
      { executable_env: "OPENCODE1_BIN" },
      true,
    );
  }
  const fragments = [
    { label: "selector_source", text: contract.selector_source },
    ...contract.tool_selectors.map((selector) => ({
      label: `tool_selectors.${selector.tool}`,
      text: selector.source,
    })),
  ];
  for (const fragment of fragments) {
    if (await fileContainsText(executable, fragment.text)) continue;
    fail(
      "contract_uncaptured",
      `the pinned V1 host no longer contains its captured ${fragment.label}; re-observe the edit-family gate before trusting the exclusion that rests on it`,
      { executable, host_version: contract.host_version, fragment: fragment.text },
      true,
    );
  }
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
