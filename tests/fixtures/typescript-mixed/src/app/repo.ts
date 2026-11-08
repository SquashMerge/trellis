import type { Entity } from "./model";
import { Model, type Named } from "./model.js";
import _ from "lodash";

export class Repo<T extends Entity> {
  private items: T[] = [];

  add(item: T): void {
    this.items.push(item);
  }

  names(models: Named[]): string[] {
    return _.map(models, (m) => m.name);
  }

  create(): Model {
    return new Model("1", "one");
  }
}
