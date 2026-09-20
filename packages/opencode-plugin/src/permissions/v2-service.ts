import { OpenCode } from "@opencode/client";
import { discover, headers } from "@opencode/client/service";

import type { V2PermissionClient, V2PermissionCreateInput } from "./v2.js";

/**
 * The outcome of looking for the OpenCode service that can show a prompt.
 *
 * The failure branch carries what was actually observed rather than a verdict,
 * because it ends up in the sentence the user reads when a prompt could not be
 * raised.
 */
export type V2PromptChannelResult =
  | { readonly client: V2PermissionClient; readonly unavailable?: undefined }
  | { readonly client?: undefined; readonly unavailable: string };

export interface V2PromptChannel {
  /** The client for the local OpenCode service, or why none could be reached. */
  client(): Promise<V2PromptChannelResult>;
}

type ServiceEndpoint = NonNullable<Awaited<ReturnType<typeof discover>>>;

function detail(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * Adapt the generated OpenCode client to the two calls a permission prompt needs.
 *
 * `event.subscribe` is wrapped rather than used directly so the prompt code keeps
 * taking a subscription object it can hold open across the `permission.create`
 * call; the generated client hands back the async iterable immediately.
 */
function promptClient(endpoint: ServiceEndpoint): V2PermissionClient {
  const client = OpenCode.make({ baseUrl: endpoint.url, headers: headers(endpoint) });
  return {
    permission: {
      // The generated input types metadata as JSON values. AFT's request
      // metadata is assembled by the tools and is JSON-serialisable by
      // construction, so it is handed over as-is.
      create: (input: V2PermissionCreateInput) =>
        client.permission.create(input as Parameters<typeof client.permission.create>[0]),
    },
    event: {
      subscribe: async () => ({ stream: client.event.subscribe() }),
    },
  };
}

/**
 * Find the OpenCode service already serving this machine.
 *
 * `discover` never starts one: it reads the service registration, checks that
 * the process behind it is healthy and version-compatible, and returns its URL
 * together with the credentials it requires. Nothing about it comes from the
 * plugin context, which is why a plugin can use it at all.
 */
async function discoverPromptClient(): Promise<V2PromptChannelResult> {
  let endpoint: ServiceEndpoint | undefined;
  try {
    endpoint = await discover();
  } catch (error) {
    return { unavailable: `looking up the local OpenCode service failed: ${detail(error)}` };
  }
  if (!endpoint) {
    return { unavailable: "no healthy, compatible local OpenCode service is registered" };
  }
  return { client: promptClient(endpoint) };
}

/**
 * One prompt channel for one OpenCode Location.
 *
 * The discovered client is kept so a session that answers many prompts pays for
 * discovery once, and it is held in this closure rather than in module state so
 * it is dropped with the Location's consumers when that scope is torn down.
 * Only a successful discovery is remembered: a host whose service was not up
 * yet must be able to raise the next prompt.
 */
export function createV2PromptChannel(
  discovery: () => Promise<V2PromptChannelResult> = discoverPromptClient,
): V2PromptChannel {
  let cached: V2PermissionClient | undefined;
  return {
    async client() {
      if (cached) return { client: cached };
      const result = await discovery();
      if (result.client) cached = result.client;
      return result;
    },
  };
}
