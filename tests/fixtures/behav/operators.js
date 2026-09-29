function target(a) {
  let s = "";
  s += typeof a;
  s += a instanceof Object ? "o" : "-";
  s += "x" in { x: 1 } ? "i" : "-";
  s += a ? "t" : "f";
  let i = 0;
  do {
    i++;
  } while (i < 3);
  s += i;
  const o = { k: 1 };
  delete o.k;
  s += Object.keys(o).length;
  s += (1, 2);
  s += a > 10 ? "big" : a > 5 ? "mid" : "small";
  switch (a) {
    case 1:
    case 2:
      s += "low";
    case 3:
      s += "three";
      break;
    default:
      s += "other";
  }
  return s;
}
