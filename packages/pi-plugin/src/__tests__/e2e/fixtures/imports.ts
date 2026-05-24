import { parse } from "jsonc-parser";
import { z } from "zod";
import { funcB } from "./sample";

export const schema = z.string();
export const message = funcB("json") + String(parse('{"value":1}')?.value ?? 0);
