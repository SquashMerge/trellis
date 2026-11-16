export async function apiFetch<T>(path: string): Promise<T> {
  return (await fetch(path)).json() as Promise<T>;
}
export class Client {
  async get(path: string): Promise<string> { return path; }
}
