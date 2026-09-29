function target(s) {
  const re = /(\d+)-(\d+)/;
  const m = re.exec(s);
  const parts = s.split("-");
  const replaced = s.replace(/\d+/g, (d) => "#" + d.length);
  const padded = String(parts[0]).padStart(4, "0");
  const joined = [padded, parts.length, replaced.includes("x"), m ? m[2] : "none"].join("/");
  return joined.toUpperCase().slice(0, 24);
}
