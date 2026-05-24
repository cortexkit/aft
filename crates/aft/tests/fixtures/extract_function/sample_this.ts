export class UserService {
  private users: Map<string, string> = new Map();

  getUser(id: string): string | undefined {
    const key = id.toLowerCase();
    const user = this.users.get(key);
    return user;
  }

  addUser(id: string, name: string): void {
    this.users.set(id, name);
  }
}
