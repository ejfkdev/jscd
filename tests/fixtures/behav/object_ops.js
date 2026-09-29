function target(k) {
  const o = { a: 1, b: 2 };
  o.c = k;
  return o.a + o.b + o.c + (o.missing === undefined ? 1 : 0);
}
