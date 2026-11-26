import { expect, test } from "bun:test";
import { AftConfigSchema } from "../config";

test("nixfmt is accepted only under the nix formatter key", () => {
  const ok = AftConfigSchema.safeParse({ formatter: { nix: "nixfmt" } });
  expect(ok.success).toBe(true);

  // Refused under every other language key: the pair would silently disable
  // that language's own formatter detection.
  for (const language of ["rust", "typescript", "python", "go", "nixt", "Nix"]) {
    const bad = AftConfigSchema.safeParse({ formatter: { [language]: "nixfmt" } });
    expect(bad.success).toBe(false);
    if (!bad.success) {
      const issue = bad.error.issues.find((i) => i.path[0] === "formatter");
      expect(issue?.path).toEqual(["formatter", language]);
      expect(issue?.message).toContain("nixfmt formats Nix files only");
    }
  }

  // Other tools and "none" stay unrestricted; a mixed map fails only on the
  // mismatched nixfmt pair.
  expect(AftConfigSchema.safeParse({ formatter: { rust: "rustfmt", nix: "nixfmt" } }).success).toBe(
    true,
  );
  expect(AftConfigSchema.safeParse({ formatter: { rust: "none" } }).success).toBe(true);
  const mixed = AftConfigSchema.safeParse({ formatter: { rust: "nixfmt", nix: "nixfmt" } });
  expect(mixed.success).toBe(false);
});
