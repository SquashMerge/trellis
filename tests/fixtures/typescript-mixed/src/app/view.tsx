import { Model } from "./model";

declare function render(model: Model): void;
declare const Panel: (props: { onClick: () => void }) => unknown;

export class View {
  model = new Model("v", "view");

  draw() {
    return <Panel onClick={() => render(this.model)} />;
  }
}

export const identity = <T,>(x: T) => x;
