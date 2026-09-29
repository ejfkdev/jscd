function target(a, b) {
  const [first, second = 9, ...rest] = [a, undefined, b, a + b];
  const { x, y: yy = 2, ...others } = { x: first, z: 5 };
  const swap = [second, first];
  const [s1, s2] = swap;
  return [s1, s2, rest.length, yy, Object.keys(others).length].join(",");
}
