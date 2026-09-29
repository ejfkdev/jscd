function target(a, b) {
  const name = `v${a}-${b}`;
  const raw = String.raw`a\nb`;
  const upper = ((s) => s.toUpperCase())`tagged${a}`;
  return `${name}|${raw.length}|${upper}|${a > b ? "gt" : "le"}`;
}
