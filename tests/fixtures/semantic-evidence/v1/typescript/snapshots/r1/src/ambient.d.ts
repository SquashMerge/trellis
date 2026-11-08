declare module "legacy-clock" {
  export interface Tick {
    at: number;
  }

  export function now(): Tick;

  export class Clock {
    constructor(label: string);
    tick(): Tick;
  }
}

declare global {
  interface Window {
    trellisVersion: string;
  }

  var trellisBuild: number;
}

declare function currentUser(): string;

declare class Registry {
  register(name: string): void;
  static instance: Registry;
}

declare const environment: "development" | "production";

declare namespace Telemetry {
  function emit(event: string): void;
}

export {};
