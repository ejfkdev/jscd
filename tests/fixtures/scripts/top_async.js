async function main() {
  const v = await Promise.resolve(6 * 7);
  return "answer " + v;
}
main().then((s) => console.log(s));
