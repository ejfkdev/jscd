let calls = 0;
function tick(x) {
  calls += 1;
  return x * 2 + calls;
}
function target(n) {
  const list = [tick(n), tick(n + 1), tick(n + 2)];
  const obj = { a: tick(n), b: tick(n + 3), ["c" + n]: tick(n + 4) };
  const sum = tick(n) + tick(n + 5);
  const nested = [sum, [tick(n + 6)]];
  return calls + "|" + list.join(",") + "|" + obj.a + "," + obj.b + "," + obj["c" + n] + "|" + sum + "|" + nested[1][0];
}
