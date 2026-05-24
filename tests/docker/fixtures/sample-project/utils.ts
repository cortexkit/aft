export function parseConfig(raw: string): Record<string, unknown> {
  return JSON.parse(raw);
}

export const MAX_RETRIES = 3;

export interface Config {
  host: string;
  port: number;
  debug: boolean;
}
