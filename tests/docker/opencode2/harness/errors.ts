export type HarnessFailureCode =
  | "contract_uncaptured"
  | "disk_effect_observation_incomplete"
  | "executable_provenance"
  | "fixture_invalid"
  | "host_failed"
  | "matrix_invalid"
  | "no_effect_observed"
  | "plugin_source_invalid"
  | "projection_unparsed"
  | "scenario_invalid"
  | "setup_timeout"
  | "delivery_timeout"
  | "duplicate_delivery"
  | "turn_log_incomplete"
  | "undeclared_disk_effect";

export class HarnessError extends Error {
  readonly code: HarnessFailureCode;
  readonly details: Readonly<Record<string, unknown>>;
  readonly unsuppressible: boolean;

  constructor(
    code: HarnessFailureCode,
    message: string,
    details: Readonly<Record<string, unknown>> = {},
    unsuppressible = false,
  ) {
    super(`${code}:${message}`);
    this.name = "HarnessError";
    this.code = code;
    this.details = details;
    this.unsuppressible = unsuppressible;
  }
}

export function fail(
  code: HarnessFailureCode,
  message: string,
  details: Readonly<Record<string, unknown>> = {},
  unsuppressible = false,
): never {
  throw new HarnessError(code, message, details, unsuppressible);
}
