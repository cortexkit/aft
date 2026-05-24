import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callBridge } from "./_shared.js";
import {
  askEditPermission,
  assertExternalDirectoryPermission,
  permissionDeniedResponse,
  resolveAbsolutePath,
  resolveRelativePattern,
} from "./permissions.js";

const z = tool.schema;

/**
 * Tool definitions for scope-aware structure commands:
 * add_member, add_derive, wrap_try_catch, add_decorator, add_struct_tags.
 */
export function structureTools(ctx: PluginContext): Record<string, ToolDefinition> {
  return {
    aft_transform: {
      description:
        "Scope-aware structural code transformations with correct indentation.\n\n" +
        "Language-specific: add_derive (Rust), add_struct_tags (Go), add_decorator (Python), wrap_try_catch (TS/JS), add_member (any language).\n\n" +
        "Ops:\n" +
        "- 'add_member': Insert method/field into class, struct, or impl block. Requires 'container' (container name) and 'code'. Optional 'position'.\n" +
        "- 'add_derive': Add Rust derive macros to a struct/enum. Requires 'target' and 'derives' array. Deduplicates existing derives.\n" +
        "- 'wrap_try_catch': Wrap a TS/JS function body in try/catch. Requires 'target' (function name). Optional 'catchBody'.\n" +
        "- 'add_decorator': Add Python decorator to function/class. Requires 'target' and 'decorator' (without @). Optional 'position'.\n" +
        "- 'add_struct_tags': Add/update Go struct field tags. Requires 'target' (struct name), 'field', 'tag', 'value'.\n\n" +
        "Each op requires specific parameters — see parameter descriptions for requirements. Use aft_safety checkpoint/undo before risky transforms.\n\n" +
        "Returns:\n" +
        "- add_member: { file, scope, position, formatted, syntax_valid?, format_skipped_reason?, validation_errors?, validate_skipped_reason?, backup_id?, lsp_diagnostics? }\n" +
        "- add_derive: { file, target, derives, formatted, syntax_valid?, format_skipped_reason?, validation_errors?, validate_skipped_reason?, backup_id?, lsp_diagnostics? }\n" +
        "- wrap_try_catch: { file, target, formatted, syntax_valid?, format_skipped_reason?, validation_errors?, validate_skipped_reason?, backup_id?, lsp_diagnostics? }\n" +
        "- add_decorator: { file, target, decorator, formatted, syntax_valid?, format_skipped_reason?, validation_errors?, validate_skipped_reason?, backup_id?, lsp_diagnostics? }\n" +
        "- add_struct_tags: { file, target, field, tag_string, formatted, syntax_valid?, format_skipped_reason?, validation_errors?, validate_skipped_reason?, backup_id?, lsp_diagnostics? }",
      // Parameters are Zod-optional because different ops need different subsets.
      // Runtime guards below validate per-op requirements and give clear errors.
      args: {
        op: z
          .enum(["add_member", "add_derive", "wrap_try_catch", "add_decorator", "add_struct_tags"])
          .describe("Transformation operation"),
        filePath: z
          .string()
          .describe("Path to the source file (absolute or relative to project root)"),
        // add_member
        container: z
          .string()
          .optional()
          .describe(
            "Container name for add_member — the class, struct, or impl block to insert into. Appears as 'scope' in the response.",
          ),
        code: z.string().optional().describe("Member code to insert (add_member)"),
        position: z
          .string()
          .optional()
          .describe(
            "For add_member: 'first', 'last' (default), 'before:name', 'after:name'. For add_decorator: 'first' (default) or 'last' only.",
          ),
        // add_derive, wrap_try_catch, add_decorator, add_struct_tags
        target: z
          .string()
          .optional()
          .describe(
            "Target symbol name (add_derive: struct/enum, wrap_try_catch: function, add_decorator: function/class, add_struct_tags: struct)",
          ),
        derives: z
          .array(z.string())
          .optional()
          .describe("Derive macro names (add_derive — e.g. ['Clone', 'Debug'])"),
        catchBody: z
          .string()
          .optional()
          .describe("Catch block body (wrap_try_catch — default: 'throw error;')"),
        decorator: z
          .string()
          .optional()
          .describe("Decorator text without @ (add_decorator — e.g. 'staticmethod')"),
        // add_struct_tags
        field: z.string().optional().describe("Struct field name (add_struct_tags)"),
        tag: z.string().optional().describe("Tag key (add_struct_tags — e.g. 'json')"),
        value: z
          .string()
          .optional()
          .describe("Tag value (add_struct_tags — e.g. 'user_name,omitempty')"),
        // common
        validate: z
          .enum(["syntax", "full"])
          .optional()
          .describe(
            "Validation level: 'syntax' (default) or 'full'. Syntax = tree-sitter parse check only. Full = also runs LSP type-checking (slower, catches more errors)",
          ),
      },
      execute: async (args, context): Promise<string> => {
        const op = args.op as string;

        if (op === "add_member") {
          if (typeof args.container !== "string")
            throw new Error("'container' is required for 'add_member' op");
          if (typeof args.code !== "string")
            throw new Error("'code' is required for 'add_member' op");
        }
        if (
          op === "add_derive" ||
          op === "wrap_try_catch" ||
          op === "add_decorator" ||
          op === "add_struct_tags"
        ) {
          if (typeof args.target !== "string")
            throw new Error(`'target' is required for '${op}' op`);
        }
        if (op === "add_derive" && !Array.isArray(args.derives)) {
          throw new Error("'derives' array is required for 'add_derive' op");
        }
        if (op === "add_decorator" && typeof args.decorator !== "string") {
          throw new Error("'decorator' is required for 'add_decorator' op");
        }
        if (op === "add_struct_tags") {
          if (typeof args.field !== "string")
            throw new Error("'field' is required for 'add_struct_tags' op");
          if (typeof args.tag !== "string")
            throw new Error("'tag' is required for 'add_struct_tags' op");
          if (typeof args.value !== "string")
            throw new Error("'value' is required for 'add_struct_tags' op");
        }

        const filePath = resolveAbsolutePath(context, args.filePath as string);

        // External-directory check first (mirrors opencode-native edit.ts:68).
        {
          const denial = await assertExternalDirectoryPermission(context, filePath);
          if (denial) return permissionDeniedResponse(denial);
        }

        const permissionError = await askEditPermission(
          context,
          [resolveRelativePattern(context, args.filePath as string)],
          { filepath: filePath },
        );
        if (permissionError) return permissionDeniedResponse(permissionError);

        const params: Record<string, unknown> = { file: args.filePath };
        if (args.validate !== undefined) params.validate = args.validate;

        switch (op) {
          case "add_member":
            params.scope = args.container;
            params.code = args.code;
            if (args.position !== undefined) params.position = args.position;
            break;
          case "add_derive":
            params.target = args.target;
            params.derives = args.derives;
            break;
          case "wrap_try_catch":
            params.target = args.target;
            if (args.catchBody !== undefined) params.catch_body = args.catchBody;
            break;
          case "add_decorator":
            params.target = args.target;
            params.decorator = args.decorator;
            if (args.position !== undefined) params.position = args.position;
            break;
          case "add_struct_tags":
            params.target = args.target;
            params.field = args.field;
            params.tag = args.tag;
            params.value = args.value;
            break;
        }

        const response = await callBridge(ctx, context, op, params);
        if (response.success === false) {
          throw new Error((response.message as string) || `${op} failed`);
        }
        return JSON.stringify(response);
      },
    },
  };
}
