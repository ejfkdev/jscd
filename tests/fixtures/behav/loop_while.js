function target(n) {
  let a = n, b = 0;
  while (a > 0) { b += a; a--; }
  do { b -= 1; } while (b > 100);
  return b;
}
