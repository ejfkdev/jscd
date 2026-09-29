function target(a, ...rest) {
  const arr = [0, ...rest, a];
  const obj = { a, ...{ b: 2 }, c: 3 };
  const merged = { ...obj, d: arr.length };
  return Math.max(...arr) + Object.keys(merged).length + rest.length;
}
