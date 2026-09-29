function target(n) {
  let sum = 0;
  const arr = [1, 2, 3, n];
  for (const v of arr) {
    if (v === 2) continue;
    sum += v;
  }
  const obj = { a: 1, b: 2 };
  const keys = [];
  for (const k in obj) {
    keys.push(k);
  }
  outer: for (let i = 0; i < 3; i++) {
    for (let j = 0; j < 3; j++) {
      if (i * j > 2) break outer;
      sum += 1;
    }
  }
  return sum + keys.join("") ;
}
