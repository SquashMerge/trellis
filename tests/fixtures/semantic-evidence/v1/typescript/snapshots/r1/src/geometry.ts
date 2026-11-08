function sealed<T extends abstract new (...args: never[]) => object>(constructor: T): T {
  Object.freeze(constructor);
  return constructor;
}

function logged(_target: object, key: string, descriptor: PropertyDescriptor): void {
  const original = descriptor.value as (...args: unknown[]) => unknown;
  descriptor.value = function (...args: unknown[]) {
    console.log("call", key);
    return original.apply(this, args);
  };
}

function tagged(_target: object, _key: string | symbol, _index: number): void {}

export interface Measurable {
  area(): number;
  perimeter(): number;
}

export abstract class Shape implements Measurable {
  abstract area(): number;
  abstract perimeter(): number;

  describe(): string {
    return `${this.constructor.name} ${this.area().toFixed(2)}`;
  }
}

@sealed
export class Rectangle extends Shape {
  constructor(
    public readonly width: number,
    protected height: number,
    private label?: string,
  ) {
    super();
  }

  area(): number {
    return this.width * this.height;
  }

  perimeter(): number {
    return 2 * (this.width + this.height);
  }

  @logged
  scale(@tagged factor: number): Rectangle {
    return new Rectangle(this.width * factor, this.height * factor, this.label);
  }
}

export class Square extends Rectangle implements Measurable {
  public static readonly sides = 4;

  constructor(side: number) {
    super(side, side, "square");
  }

  override area(): number {
    return super.area();
  }

  get side(): number {
    return this.width;
  }

  set side(value: number) {
    Object.assign(this, { width: value, height: value });
  }
}

@sealed()
export class Circle extends Shape {
  constructor(private readonly radius: number) {
    super();
  }

  area(): number {
    return Math.PI * this.radius ** 2;
  }

  perimeter(): number {
    return 2 * Math.PI * this.radius;
  }
}
