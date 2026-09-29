function target(x) {
  const arr = [1, 2, 3];
  arr[0] = x;
  arr.push(x + 1);
  const last = arr[arr.length - 1];
  return arr.length + last + arr[1];
}
