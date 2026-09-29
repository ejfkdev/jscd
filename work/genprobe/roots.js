function probe(arr, o, p, m) {
  const a = arr.length;
  const b = arr.join(",");
  const c = arr.push(1);
  const d = o.constructor;
  const e = p.then;
  const f = p.resolve;
  const g = m.get;
  const h = m.set;
  const i = m.has;
  const j = stringsOnly();
  const k = arr["next"];
  const l = arr.forEach;
  const n = arr.indexOf;
  return [a,b,c,d,e,f,g,h,i,j,k,l,n];
}
function stringsOnly() { return 1; }
