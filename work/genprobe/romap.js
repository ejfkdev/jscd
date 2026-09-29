function p_hex_2f(o) { return o["/"]; }
function p_hex_2f_sep(o) { return [o, o].join("/"); }
function p_hex_2f_arr(o) { return [o, "/"]; }
function p_hex_30(o) { return o["0"]; }
function p_hex_30_pad(o) { return o.padStart(4, "0"); }
function p_none(o) { return o.none; }
function p_hex_none_lit(o) { return o ? "none" : o; }
function __runAll(o) {
  p_hex_2f(o); p_hex_2f_sep(o); p_hex_2f_arr(o); p_hex_30(o); p_hex_30_pad("ab"); p_none(o); p_hex_none_lit(o);
}
__runAll({});
