export function combine(left: string, right: string): string;
export function combine(left: number, right: number): number;
export function combine(left: unknown, right: unknown): unknown {
  if (typeof left === "string" && typeof right === "string") {
    return left + right;
  }
  return (left as number) + (right as number);
}

export function firstOf<T>(items: readonly T[]): T | undefined;
export function firstOf<T>(items: readonly T[], fallback: T): T;
export function firstOf<T>(items: readonly T[], fallback?: T): T | undefined {
  return items.length > 0 ? items[0] : fallback;
}

export class Parser {
  parse(input: string): number;
  parse(input: string, radix: number): number;
  parse(input: string, radix?: number): number {
    return radix === undefined ? Number(input) : Number.parseInt(input, radix);
  }

  format(value: number): string;
  format(value: Date): string;
  format(value: number | Date): string {
    return value instanceof Date ? value.toISOString() : value.toFixed(1);
  }
}

export abstract class Codec<T> {
  abstract encode(value: T): string;
  abstract decode(text: string): T;

  roundTrip(value: T): T {
    return this.decode(this.encode(value));
  }
}

export const parser = new Parser();
export const total = combine(parser.parse("7"), parser.parse("ff", 16));
