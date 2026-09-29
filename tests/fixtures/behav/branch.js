function target(n) {
  if (n < 0) return "neg";
  else if (n === 0) return "zero";
  else if (n < 10) return "small";
  return "big";
}
