declare module "bun:test" {
  export const afterAll: (...args: any[]) => any;
  export const afterEach: (...args: any[]) => any;
  export const beforeAll: (...args: any[]) => any;
  export const beforeEach: (...args: any[]) => any;
  export const describe: any;
  export const expect: (...args: any[]) => any;
  export const mock: (...args: any[]) => any;
  export const test: any;
}

interface ImportMeta {
  readonly dir: string;
}
