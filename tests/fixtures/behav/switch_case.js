function target(n) {
  switch (n) {
    case 0: return "zero";
    case 1: case 2: return "small";
    default: return n > 0 ? "pos" : "neg";
  }
}
