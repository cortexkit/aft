import { Schema } from "effect";

/**
 * The V2 host's Rpc.define returns its input after checking reserved custom
 * error names. AFT defines no custom RPC errors, so this local identity has the
 * same result and also loads on V1, where @opencode-ai/schema is unavailable.
 */
const Rpc = {
  define<const Definition>(definition: Definition): Definition {
    return definition;
  },
};

export const RpcSession = Schema.Struct({
  sessionID: Schema.optional(Schema.String),
});

export const IndexProgress = Schema.Struct({
  index: Schema.Literals(["search", "semantic"]),
  status: Schema.String,
  sessionID: Schema.optional(Schema.String),
  stage: Schema.optional(Schema.NullOr(Schema.String)),
  completed: Schema.optional(Schema.Number),
  total: Schema.optional(Schema.Number),
});

export type AftRpcSession = typeof RpcSession.Type;
export type AftIndexProgress = typeof IndexProgress.Type;

export const AftRpc = Rpc.define({
  id: "aft",
  methods: {
    getStatus: {
      input: RpcSession,
      output: Schema.Record(Schema.String, Schema.Unknown),
    },
  },
  events: {
    statusInvalidated: { schema: RpcSession },
    showStatusDialog: { schema: RpcSession },
    indexProgress: { schema: IndexProgress },
  },
});
