import { helper } from "./legacy.js";
import "./esm.mjs";
import "./cjs.cjs";
import "./types";
import "../../pkg/";

export function bridge(): void {
  helper();
}
