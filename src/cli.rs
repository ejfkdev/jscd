//! 子命令实现与统一输出（stdout=数据，stderr=诊断，退出码由 main 收口）。

use crate::args::{Cmd, Common};
use std::fs;
use std::io::Write;
use std::path::Path;

pub fn run_cmd(cli: crate::args::Cli) -> Result<(), String> {
    let json = cli.json;
    match cli.cmd {
        Cmd::Version => {
            println!("jscd {}", env!("JSCD_VERSION"));
            Ok(())
        }
        Cmd::Info { file, common } => info_cmd(&file, &common, json),
        Cmd::Strings { file, common } => strings_cmd(&file, &common, json),
        Cmd::Functions { file, common } => functions_cmd(&file, &common, json),
        Cmd::Disasm {
            file,
            common,
            filter,
        } => disasm_cmd(&file, &common, filter.as_deref(), json),
        Cmd::Decompile { file, common } => decompile_cmd(&file, &common, json),
        Cmd::DebugParse { file } => debug_parse_cmd(&file),
    }
}

/// (dev) 流解析观察器：表加载 → 反序列化 → 对象清单。
fn debug_parse_cmd(file: &Path) -> Result<(), String> {
    use std::fmt::Write as _;
    let data = load_input(file)?;
    let (h, table, cache) = parse_cache(&data)?;
    let layout = crate::bytecode::FamilyLayout::from_table(&table);
    let d = crate::disasm::Disassembler::new(&cache, &table, layout).with_source_len(h.source_length());

    let mut s = String::new();
    let _ = writeln!(
        s,
        "v8 {} ({} objects, top=#{})",
        table.v8,
        cache.objects.len(),
        cache.top_sfi
    );
    let dump_str = std::env::var("JSCD_DUMP_STR").is_ok();
    for (id, o) in cache.objects.iter().enumerate() {
        let map_ref = o
            .slots
            .first()
            .and_then(|sl| sl.value.as_ref())
            .map(|r| format!("{r:?}"))
            .unwrap_or_else(|| "-".into());
        let _ = writeln!(
            s,
            "#{id:<4} {:<28} size={:<5} slots={:<3} map={}",
            o.ty.name(&table),
            o.byte_size,
            o.slots.len(),
            map_ref
        );
        if dump_str && o.ty.is_string(&table) {
            if let Some(v) = d.string_value(id) {
                let _ = writeln!(s, "      text = {v:?}");
            }
        }
    }
    if std::env::var("JSCD_LIST_FN").is_ok() {
        for id in 0..cache.objects.len() {
            if !cache.obj(id).ty.is(&table, "SharedFunctionInfo") {
                continue;
            }
            let _ = writeln!(s, "SFI#{id} name={:?}", d.sfi_name(id));
        }
    }
    print!("{s}");
    Ok(())
}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

fn info_cmd(file: &Path, common: &Common, json: bool) -> Result<(), String> {
    let data = load_input(file)?;
    // 两遍解析：默认布局读出 version_hash（两种布局该字段都在 @4）→ 识别 V8 版本
    // → 换用该版本的表布局重解析（12.x+ 头部多出 ro checksum，布局不同）。
    let h0 = crate::header::Header::parse(&data)?;
    let ident = crate::tables::identify(h0.version_hash);
    let header = crate::tables::table_for(ident.v8)
        .and_then(|t| crate::header::Header::parse_with(&data, &t.header).ok())
        .unwrap_or(h0);
    let text = header.render_text(file, &ident);
    emit(common, &text, &header.render_json(file, &ident), json)
}

/// 常量池字符串提取：遍历对象图取全部字符串字面量（去重保序）。
fn strings_cmd(file: &Path, common: &Common, json_flag: bool) -> Result<(), String> {
    let data = load_input(file)?;
    let (h, table, cache) = parse_cache(&data)?;
    let layout = crate::bytecode::FamilyLayout::from_table(&table);
    let d = crate::disasm::Disassembler::new(&cache, &table, layout).with_source_len(h.source_length());

    let mut seen = std::collections::HashSet::new();
    let mut values: Vec<String> = Vec::new();
    for id in 0..cache.objects.len() {
        let obj = cache.obj(id);
        if !obj.ty.is_string(&table) {
            continue;
        }
        if let Some(v) = d.string_value(id) {
            if seen.insert(v.clone()) {
                values.push(v);
            }
        }
    }
    // 根表里的 internalized 字符串（脚本未序列化时也能给出线索）
    let mut roots: Vec<String> = Vec::new();
    for (i, name) in table.roots.iter().enumerate() {
        if let Some(lit) = name.strip_prefix("String:") {
            if !lit.is_empty() && seen.insert(lit.to_string()) {
                values.push(lit.to_string());
                roots.push(format!("root#{i}"));
            }
        }
    }
    let mut text = String::new();
    for v in &values {
        text.push_str(v);
        text.push('\n');
    }
    let json = serde_json::json!({ "count": values.len(), "strings": values, "from_roots": roots });
    emit(common, &text, &json.to_string(), json_flag)
}

/// 函数树：列出全部 SharedFunctionInfo（名字/参数/字节码长度/嵌套层级）。
fn functions_cmd(file: &Path, common: &Common, json_flag: bool) -> Result<(), String> {
    use std::fmt::Write as _;
    let data = load_input(file)?;
    let (h, table, cache) = parse_cache(&data)?;
    let layout = crate::bytecode::FamilyLayout::from_table(&table);
    let d = crate::disasm::Disassembler::new(&cache, &table, layout).with_source_len(h.source_length());
    let ts = table.tagged_size as usize;
    let ba = table
        .bytecode_array
        .clone()
        .unwrap_or_else(|| crate::tables::BytecodeArrayLayout::fallback(ts));

    // 嵌套关系：SFI 的常量池里引用到的子 SFI（常量池元素恒为指针槽，直接读槽即可）
    let mut children: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
    let mut all_sfis: Vec<usize> = Vec::new();
    let pool_slot = ba.fields.get("constant_pool").map(|o| o / ts).unwrap_or(2);
    for id in 0..cache.objects.len() {
        if !cache.obj(id).ty.is(&table, "SharedFunctionInfo") {
            continue;
        }
        all_sfis.push(id);
        let Some(crate::serializer::SlotValue::Ref(crate::serializer::Ref::Object(b))) =
            d.sfi_function_data_slots()
                .iter()
                .find_map(|s| match cache.slot_at(id, *s) {
                    Some(v @ crate::serializer::SlotValue::Ref(crate::serializer::Ref::Object(b)))
                        if cache.obj(*b).ty.is(&table, "BytecodeArray") =>
                    {
                        Some(v)
                    }
                    _ => None,
                })
        else {
            continue;
        };
        let Some(crate::serializer::SlotValue::Ref(crate::serializer::Ref::Object(pool))) =
            cache.slot_at(*b, pool_slot)
        else {
            continue;
        };
        for slot in &cache.obj(*pool).slots {
            if slot.index < 2 {
                continue; // map / length
            }
            if let crate::serializer::SlotValue::Ref(crate::serializer::Ref::Object(c)) =
                &slot.value
            {
                if cache.obj(*c).ty.is(&table, "SharedFunctionInfo") {
                    children.entry(id).or_default().push(*c);
                }
            }
        }
    }
    let mut roots: Vec<usize> = all_sfis
        .iter()
        .copied()
        .filter(|id| !children.values().any(|v| v.contains(id)))
        .collect();
    roots.sort_unstable();

    let mut text = String::new();
    let mut json_items = Vec::new();
    let mut visited = std::collections::HashSet::new();
    // 迭代式遍历（避免嵌套 fn 捕获环境），栈序保证父先于子
    let mut stack: Vec<(usize, usize)> = roots.iter().rev().map(|r| (*r, 0)).collect();
    while let Some((id, depth)) = stack.pop() {
        if !visited.insert(id) {
            continue;
        }
        let name = d.sfi_name(id);
        let bca = d.sfi_function_data_slots().iter().find_map(|slot| {
            match cache.slot_at(id, *slot) {
                Some(crate::serializer::SlotValue::Ref(crate::serializer::Ref::Object(b)))
                    if cache.obj(*b).ty.is(&table, "BytecodeArray") =>
                {
                    Some(*b)
                }
                _ => None,
            }
        });
        let (bc_len, params, frame) = match bca {
            Some(b) => {
                let len = cache
                    .raw_at(b, ts, ts)
                    .and_then(crate::serializer::decode_smi_bytes)
                    .unwrap_or(0);
                let params = ba
                    .off("parameter_size")
                    .and_then(|o| cache.raw_at(b, o, 4))
                    .map(|x| u32::from_le_bytes(x.try_into().unwrap()))
                    .unwrap_or(0);
                let params = if d.parameter_count_direct() {
                    params & 0xFFFF
                } else {
                    params / 8
                };
                let frame = ba
                    .off("frame_size")
                    .and_then(|o| cache.raw_at(b, o, 4))
                    .map(|x| u32::from_le_bytes(x.try_into().unwrap()))
                    .unwrap_or(0);
                (len as usize, params, frame)
            }
            None => (0, 0, 0),
        };
        let marker = if bca.is_some() { "" } else { "  (uncompiled)" };
        let _ = writeln!(
            text,
            "{:indent$}{}  [{}] params={} frame={} bytecode={}{}",
            "",
            if name.is_empty() { "<anonymous>" } else { &name },
            id,
            params,
            frame,
            bc_len,
            marker,
            indent = depth * 2
        );
        json_items.push(serde_json::json!({
            "id": id, "name": name, "depth": depth, "compiled": bca.is_some(),
            "params": params, "frame_size": frame, "bytecode_length": bc_len,
        }));
        if let Some(kids) = children.get(&id) {
            for c in kids.iter().rev() {
                stack.push((*c, depth + 1));
            }
        }
    }
    let json = serde_json::json!({ "count": json_items.len(), "functions": json_items });
    emit(common, &text, &json.to_string(), json_flag)
}

fn disasm_cmd(file: &Path, common: &Common, filter: Option<&str>, json_flag: bool) -> Result<(), String> {
    let data = load_input(file)?;
    let (h, table, cache) = parse_cache(&data)?;
    let layout = crate::bytecode::FamilyLayout::from_table(&table);
    let d = crate::disasm::Disassembler::new(&cache, &table, layout)
        .with_source_len(h.source_length());
    let text = d.render_all(filter)?;
    emit(common, &text, &json_out(&text), json_flag)
}

/// 解压 → 头 → 识别 → 表 → payload 反序列化（tagged_size 自动回退）。
fn parse_cache<'a>(
    data: &'a [u8],
) -> Result<
    (
        crate::header::Header,
        crate::tables::VersionTable,
        crate::serializer::CodeCache<'a>,
    ),
    String,
> {
    let h0 = crate::header::Header::parse(data)?;
    let ident = crate::tables::identify(h0.version_hash);
    let table = crate::tables::table_for(ident.v8)
        .ok_or_else(|| format!("no embedded table for v8 {} (run codegen)", ident.v8))?;
    let h = crate::header::Header::parse_with(data, &table.header)?;
    let payload = h.payload(data);
    // tagged_size 是构建属性（压缩/非压缩），表里给默认值；解析失败时回退另一种
    let mut errors = Vec::new();
    for ts in [table.tagged_size, if table.tagged_size == 8 { 4 } else { 8 }] {
        let mut t = table.clone();
        t.tagged_size = ts;
        match crate::serializer::parse(payload, &t) {
            Ok(c) => return Ok((h, t, c)),
            Err(e) => errors.push(format!("tagged_size={ts}: {e}")),
        }
    }
    Err(format!("payload parse failed ({})", errors.join(" | ")))
}

fn json_out(text: &str) -> String {
    serde_json::json!({ "disassembly": text }).to_string()
}

fn decompile_cmd(_file: &Path, _common: &Common, _json: bool) -> Result<(), String> {
    Err("decompile: control-flow reconstruction lands after disasm (M2)".into())
}

/// 读入并解压（bytenode --compress 的 Brotli 包裹对上层透明）。
pub fn load_input(file: &Path) -> Result<Vec<u8>, String> {
    let raw = fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(crate::header::maybe_brotli_decompress(raw))
}

/// 统一落盘/出 stdout：-o 指定文件，`-o -` 或缺省走 stdout。
fn emit(common: &Common, text: &str, json: &str, want_json: bool) -> Result<(), String> {
    let payload = if want_json { json } else { text };
    match &common.output {
        Some(p) if p != Path::new("-") => fs::write(p, payload)
            .map_err(|e| format!("write {}: {e}", p.display())),
        _ => {
            let mut out = std::io::stdout().lock();
            out.write_all(payload.as_bytes()).map_err(|e| e.to_string())?;
            if !payload.ends_with('\n') {
                out.write_all(b"\n").map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }
}
