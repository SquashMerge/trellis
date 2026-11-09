// What every drawing shares: one headless Pele, its text measure, and the small DOM.
import makePele, { widthTable } from "trellis-web:pele";
import { createDocument } from "./dom.js";

// Pele's headless text measure (its Helvetica table), so the adapter wraps and fits labels
// with the same widths Pele lays them out with. Mirrors Pele's `_1.width(text, size, 0)`.
const WIDTHS = Array.from({ length: 95 }, (_, i) => parseInt(widthTable.substr(i * 2, 2), 36));
const zeroWidth = (c) => c === 173 || (c >= 768 && c <= 879) || (c >= 8203 && c <= 8207) || (c >= 56320 && c <= 57343) || (c >= 65024 && c <= 65039) || c === 65279;
const wide = (c) => (c >= 4352 && c <= 4447) || (c >= 11904 && c <= 42191) || (c >= 44032 && c <= 55203) || (c >= 63744 && c <= 64255) ||
  (c >= 65072 && c <= 65103) || (c >= 65280 && c <= 65376) || (c >= 65504 && c <= 65510) || (c >= 55296 && c <= 56319);
export function measure(text, size = 14) {
  let units = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    if (c < 32) continue;
    if (c < 127) units += WIDTHS[c - 32];
    else if (!zeroWidth(c)) units += wide(c) ? 1000 : 580;
  }
  return (units * size) / 1000;
}
// The family the drawn text is set in: the metrics above are Helvetica's.
export const FONT_FAMILY = "Helvetica, Arial, sans-serif";

export const document = createDocument(measure);
// Pele with a shadowed globalThis: no canvas, so it lays out with its built-in width table.
export const pele = makePele({});
