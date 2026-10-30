export function caller(items) {
  const seen = new Set(items);
  if (!seen.size) throw Error("empty");
  return seen;
}
