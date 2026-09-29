function target(start) {
  let n = start;
  const inc = function () { return ++n; };
  return inc() + inc();
}
