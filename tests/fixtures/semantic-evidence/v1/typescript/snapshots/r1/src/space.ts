export namespace Geo.Planar.Metric {
  export const unit = 1;

  export function scale(value: number): number {
    return value * unit;
  }

  export class Ruler {
    constructor(readonly length: number) {}

    measure(value: number): number {
      return scale(value) / this.length;
    }
  }
}

namespace Tools {
  export interface Wrench {
    size: number;
  }

  export function tighten(wrench: Wrench): number {
    return wrench.size * 2;
  }
}

module Legacy {
  export function describe(): string {
    return "legacy " + Tools.tighten({ size: 3 });
  }
}

export function measureAll(values: number[]): number[] {
  const ruler = new Geo.Planar.Metric.Ruler(2);
  return values.map((value) => ruler.measure(value));
}

export const legacyText = Legacy.describe();
