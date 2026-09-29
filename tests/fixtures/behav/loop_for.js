function target(n) {
  let acc = 0;
  for (let i = 0; i < n; i++) { if (i % 2 === 0) acc += i; else acc -= i; }
  return acc;
}
