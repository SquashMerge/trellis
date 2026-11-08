export interface Entity {
  id: Id;
}

export interface Named extends Entity {
  name: string;
}

export type Id = string;

export enum Status {
  Active,
  Inactive = 2,
}

function sealed(): ClassDecorator {
  return () => {};
}

function log(target: object, key: string): void {}

export abstract class Base<T> implements Entity {
  id: Id = "";
  abstract describe(): T;
}

@sealed()
export class Model extends Base<Id> implements Named {
  constructor(
    private readonly id: Id,
    public name: string,
  ) {
    super();
  }

  describe(): Id {
    return this.id;
  }

  @log
  rename(next: string): void {
    this.name = next;
  }
}

export function parse(x: string): Model;
export function parse(x: number): Model;
export function parse(x: any) {
  return new Model(String(x), "m");
}

export const made = make<Model>();

declare function make<T>(): T;
