// Test stand-in for the host's `herdr` module: shared state, theme, props and files.
import { readFile } from "node:fs/promises";
// One state for the bundled copy and the test that imports this file directly.
export const state = (globalThis.__herdrStub ??= { shared: {}, props: {}, appHome: "/tmp", home: "/tmp" });
export const props = () => state.props;
export const shared = (key) => state.shared[key];
export const setShared = (key, value) => { state.shared[key] = value; };
export const appHome = () => state.appHome;
export const home = () => state.home;
export const readTextFile = async (path) => { try { return await readFile(path, "utf8"); } catch { return null; } };
export const theme = () => ({ background: "#101114", foreground: "#e6e6e6", muted: "#a0a0a8", input: "#6a6a78", dim: "#777777", surface: "#17181c",
  raised: "#202128", border: "#2c2d35", hover: "#25262c", selected: "#2a2f3a", accent: "#7aa2f7", ansi0: "#000000", ansi1: "#e06c75", ansi2: "#98c379",
  ansi3: "#e5c07b", ansi9: "#ff7a5c", ansi11: "#ffd75f" });
export const focused = () => state.focused ?? null;
export const spaces = () => state.spaces ?? [];
export const layout = () => state.layout ?? null;
// exec here, as Herdr runs it for this computer (`on` names another machine in the app).
import { spawn } from "node:child_process";
export const exec = (command, args = [], options = {}) => new Promise((resolve, reject) => {
  state.execs = (state.execs || 0) + 1;
  const child = spawn(command, args, { cwd: options.cwd });
  let stdout = "", stderr = "";
  child.stdout.on("data", (d) => { stdout += d; });
  child.stderr.on("data", (d) => { stderr += d; });
  child.on("error", reject);
  child.on("close", (code) => resolve({ code, stdout, stderr }));
});
export const machines = () => state.machines ?? [{ name: null, layout: "ready" }];
