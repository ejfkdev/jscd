function target(name, count) {
  const parts = ["a", "b"];
  return name + ":" + count + "|" + parts.join("-") + "|" + parts.length;
}
