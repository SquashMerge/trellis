export interface Named {
  readonly name: string;
  describe(): string;
}

export interface Box<T> extends Named {
  value: T;
  map<U>(transform: (value: T) => U): Box<U>;
  (input: T): Box<T>;
  [key: string]: unknown;
}

export type Pair<A, B = A> = { first: A; second: B };

export type Mapper<T, U> = (value: T) => U;

export enum Direction {
  North,
  East = 2,
  South = East + 1,
  West = "west".length,
}

export const enum Flag {
  None = 0,
  Visible = 1 << 0,
}

export function boxOf<T>(value: T, name: string): Box<T> {
  const box = Object.assign(((input: T) => boxOf<T>(input, name)) as Box<T>, {
    name,
    value,
    describe: () => `${name}:${String(value)}`,
    map: <U>(transform: Mapper<T, U>) => boxOf<U>(transform(value), name),
  });
  return box;
}

export function pairOf<A, B = A>(first: A, second: B): Pair<A, B> {
  return { first, second };
}

export function turn(direction: Direction): Direction {
  return direction === Direction.West ? Direction.North : direction + 1;
}
