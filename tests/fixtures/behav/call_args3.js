class C {
  constructor(a, b, c) {
    this.v = a + b + c;
  }
  m(a, b, c, d) {
    return this.v - a - b - c - d;
  }
}

function target(x) {
  const f = (a, b, c) => a * 100 + b * 10 + c;
  const bare = () => 7;
  const c = new C(1, 2, 3);
  const empty = new C();
  const arr = [1, 2, 3];
  const spliced = arr.splice(0, 1, x, x + 1);
  return (
    f(x, x + 1, x + 2) +
    bare() +
    c.v +
    c.m(1, 2, 3, x) +
    arr.length +
    spliced.length +
    String(empty.v)
  );
}