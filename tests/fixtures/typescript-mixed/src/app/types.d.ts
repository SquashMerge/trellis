declare module "ambient-mod" {
  export function f(): void;
}

declare global {
  interface Window {
    app: string;
  }
}

declare function helperDecl(a: number): void;

declare class Shape {
  area(): number;
}

export interface Config {
  x: number;
}
