export const TRAJECTORIES = ["T1", "T2", "T3", "T4", "T5", "T6", "T7"] as const;
export type Trajectory = (typeof TRAJECTORIES)[number];

export type ScenarioExecutionMode = "shared-server" | "standalone";
export type DiskEffectPhase = "result" | "post_result" | "cancellation" | "any";

export interface DiskEffectDeclaration {
  path: string;
  phase?: DiskEffectPhase;
}

export interface NonMutatingEvidence {
  reason: string;
}

export interface ProjectionTypeMap {
  [group: string]: "boolean" | "number" | "string";
}

export interface FieldProjectionRule {
  kind: "field";
  field: string;
  pattern: string;
  min_lines?: number;
  max_lines?: number;
  types?: ProjectionTypeMap;
}

export interface IgnoreProjectionRule {
  kind: "ignore";
  pattern: string;
  min_lines?: number;
  max_lines?: number;
}

export interface TrailerProjectionRule {
  kind: "trailer";
  field?: string;
}

export type ProjectionRule = FieldProjectionRule | IgnoreProjectionRule | TrailerProjectionRule;

export interface ExactComparison {
  mode: "exact";
  expected: string;
}

export interface ShapeComparison {
  mode: "shape";
  rules: ProjectionRule[];
  expected: Record<string, unknown>;
}

export type ScenarioComparison = ExactComparison | ShapeComparison;

export interface ToolCallPlan {
  id: string;
  name: string;
  arguments: Record<string, unknown>;
  disk_effects?: Array<string | DiskEffectDeclaration>;
  non_mutating_evidence?: NonMutatingEvidence;
  outlives_result?: boolean;
  task_ids?: string[];
  process_ids?: number[];
}

export interface ToolTurnResponse {
  kind: "tool_calls";
  calls: ToolCallPlan[];
}

export interface TextTurnResponse {
  kind: "text";
  content: string;
}

export interface ScriptedTurn {
  label: string;
  delay_ms?: number;
  response: ToolTurnResponse | TextTurnResponse;
}

export interface ApiControlPlan {
  id: string;
  after_turn: string;
  delay_ms?: number;
  method: string;
  path: string;
  body?: unknown;
  expected_status?: number;
  expected_stdout_pattern?: string;
  forbidden_stdout_pattern?: string;
  purpose: "abort" | "permission" | "smoke";
}

export interface ThreeStatePathExpectation {
  path: string;
  transition:
    | "create"
    | "delete"
    | "edit"
    | "move_destination"
    | "move_overwrite_destination"
    | "move_source";
  paired_path?: string;
  expected_intermediate_sha256?: string;
}

export interface RestoreEvidencePlan {
  operation: "checkpoint_restore" | "undo";
  identifier: string;
  calls: Array<{ tool: string; arguments: Record<string, unknown> }>;
  paths: ThreeStatePathExpectation[];
}

export interface ScenarioDefinition {
  schema_version: 1;
  id: string;
  tool: string;
  trajectory: Trajectory;
  execution: ScenarioExecutionMode;
  prompt: string;
  model?: string;
  auto?: boolean;
  fixture?: string;
  turns: ScriptedTurn[];
  expected_turns?: string[];
  comparison?: ScenarioComparison;
  compare_call_id?: string;
  error_origin?: "host" | "product";
  subcase?: "invalid_arguments" | "missing_target" | string;
  controls?: ApiControlPlan[];
  transport_dead?: { from_turn: string; to_turn: string };
  restore_evidence?: RestoreEvidencePlan;
  quiescence_timeout_ms?: number;
  expected_fail_issue?: string;
  project_config?: Record<string, unknown>;
  metadata?: Record<string, unknown>;
  /** Added by the loader so fixture paths can stay registration-relative. */
  registration_path?: string;
}

export interface ScenarioRegistration {
  schema_version: 1;
  tool: string;
  scenarios: ScenarioDefinition[];
}

export interface RecordedMockExchange {
  index: number;
  label: string;
  request: unknown;
  response: unknown;
  observed_at: string;
}

export interface ScenarioResult {
  id: string;
  status: "failed" | "passed";
  issue?: string;
  failure?: { code: string; message: string; details?: Record<string, unknown> };
  forensic_dir: string;
}

export interface HarnessValidationContext {
  repo_root: string;
  platform: NodeJS.Platform;
  scenarios: readonly ScenarioDefinition[];
  matrix: unknown;
  pinned_host_version: string;
}

export interface ScenarioLifecycleContext {
  scenario: ScenarioDefinition;
  run_root: string;
  project_root: string;
  forensic_dir: string;
  host_generation: "v1" | "v2";
}

export type HarnessRuntimeEvent =
  | { kind: "tool_call_accepted"; call: ToolCallPlan; turn: string; at: number }
  | { kind: "tool_result_observed"; call: ToolCallPlan; before_turn: string; at: number }
  | { kind: "mock_exchange"; exchange: RecordedMockExchange }
  | { kind: "control_started"; control: ApiControlPlan; at: number }
  | { kind: "control_completed"; control: ApiControlPlan; at: number; output: unknown }
  | { kind: "host_exit"; at: number; output: unknown }
  | { kind: "termination"; at: number; evidence: unknown }
  | { kind: "disk_checkpoints"; at: number; checkpoints: unknown[] };

export interface HarnessExtension {
  name: string;
  validate?: (context: HarnessValidationContext) => void | Promise<void>;
  beforeScenario?: (context: ScenarioLifecycleContext) => void | Promise<void>;
  observe?: (context: ScenarioLifecycleContext, event: HarnessRuntimeEvent) => void | Promise<void>;
  afterScenario?: (
    context: ScenarioLifecycleContext,
    result: ScenarioResult,
  ) => void | Promise<void>;
}
