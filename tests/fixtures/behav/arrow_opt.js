function target(a, b = 2) {
  const add = (x, y) => x + y;
  const nest = (x) => (y) => x * y;
  const obj = { v: a, get raw() { return this.v; } };
  const val = obj?.raw ?? 0;
  const missing = obj?.nope?.deep;
  return add(a, b) + nest(2)(3) + val + (missing === undefined ? 1 : 0);
}
