import type { Named, Box as ShapeBox } from "./shapes";
import { type Pair, pairOf, Direction as Heading } from "./shapes";
import fs = require("node:fs");
import Planar = Geo.Planar;
import { Rectangle } from "./geometry";

export type { Named } from "./shapes";
export type { Pair as Duo };
export { Rectangle as Rect } from "./geometry";

export type LabelledBox = ShapeBox<string> & Named;

export function headingPair(): Pair<Heading, string> {
  return pairOf(Heading.East, "east");
}

export function readSize(path: string): number {
  return fs.statSync(path).size;
}

export function rulerFor(length: number): Planar.Metric.Ruler {
  return new Planar.Metric.Ruler(length);
}

export const square = new Rectangle(2, 2);

declare namespace Geo {
  export namespace Planar {
    export namespace Metric {
      class Ruler {
        constructor(length: number);
      }
    }
  }
}
