import { boxOf, type Box } from "./shapes";

interface ViewProps<T> {
  box: Box<T>;
  render?: (value: T) => string;
}

export const identity = <T,>(value: T): T => value;

export function Label<T>({ box, render }: ViewProps<T>) {
  const text = render ? render(box.value) : String(box.value);
  return <span title={box.describe()}>{text}</span>;
}

export function Panel(props: { items: string[] }) {
  const boxes = props.items.map((item) => boxOf<string>(item, item));
  return (
    <section>
      {boxes.map((box) => (
        <Label<string> key={box.name} box={box} />
      ))}
      <p>{identity<number>(boxes.length)}</p>
    </section>
  );
}
