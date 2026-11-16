import { apiFetch, Client } from "./lib";
export async function plain(path: string) {
  return apiFetch<string>(path);
}
export async function awaited(path: string) {
  const a = await apiFetch<string>(path);
  await apiFetch(path);
  return a;
}
export async function returned(path: string) {
  return await apiFetch(path);
}
export async function member(c: Client) {
  return await c.get("x");
}
