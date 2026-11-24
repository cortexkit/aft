// Regular function
export function greet(name: string): string {
  return `Hello, ${name}!`;
}

// Arrow function assigned to const
export const add = (a: number, b: number): number => {
  return a + b;
};

// Class with methods
export class UserService {
  private users: Map<string, string> = new Map();

  getUser(id: string): string | undefined {
    return this.users.get(id);
  }

  addUser(id: string, name: string): void {
    this.users.set(id, name);
  }
}

// Interface
export interface Config {
  host: string;
  port: number;
  debug?: boolean;
}

// Enum
export enum Status {
  Active = "active",
  Inactive = "inactive",
  Pending = "pending",
}

// Type alias
export type UserId = string;

// Non-exported function
function internalHelper(): void {
  console.log("internal");
}
