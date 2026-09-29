function target(x) {
  try {
    if (x < 0) throw new RangeError("negative");
    return "ok:" + x;
  } catch (e) {
    return "caught:" + e.message;
  } finally {
    x = x + 100;
  }
}
