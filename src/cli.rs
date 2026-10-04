//! 子命令实现与统一输出（stdout=数据，stderr=诊断，退出码由 main 收口）。

use crate::args::{Cmd, Common};
use crate::{bi, bif};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

pub fn run_cmd(cli: crate::args::Cli) -> Result<(), String> {
    let json = cli.json;
    match cli.cmd {
        Cmd::Info { file, common } => info_cmd(&file, &common, json),
        Cmd::Strings { file, common } => strings_cmd(&file, &common, json),
        Cmd::Functions { file, common } => functions_cmd(&file, &common, json),
        Cmd::Disasm {
            file,
            common,
            filter,
        } => disasm_cmd(&file, &common, filter.as_deref(), json),
        Cmd::Decompile {
            file,
            output,
            common,
            ro_map,
            verify,
            runtime,
        } => {
            let dest = match (output, common.output) {
                (Some(a), Some(b)) if a != b => {
                    return Err(bif!(
                        "-o {0} conflicts with the OUTPUT argument {1}",
                        "-o {0} 与位置参数 OUTPUT {1} 冲突";
                        b.display(),
                        a.display()
                    ));
                }
                (Some(a), _) => Some(a),
                (None, b) => b,
            };
            decompile_path(
                &file,
                dest.as_deref(),
                common.quiet,
                ro_map.as_deref(),
                verify,
                json,
                runtime,
            )
        }
        Cmd::RoMap { file, common } => ro_map_cmd(&file, &common, json),
        Cmd::DebugParse { file } => debug_parse_cmd(&file),
    }
}

/// (dev) 流解析观察器：表加载 → 反序列化 → 对象清单。
fn debug_parse_cmd(file: &Path) -> Result<(), String> {
    use std::fmt::Write as _;
    let data = load_input(file)?;
    let (h, table, cache) = parse_cache(&data)?;
    let layout = crate::bytecode::FamilyLayout::from_table(&table);
    let d = crate::disasm::Disassembler::new(&cache, &table, layout)
        .with_source_len(h.source_length())
        .with_ro_map(embedded_ro_map(&table.v8));

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
    crate::out::stdout(&s).map_err(|e| e.to_string())
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
    let mut text = header.render_text(file, &ident);
    // 只读堆名表是 macOS 探针建的：本平台不套用时把话说明白（info 是给人看的诊断命令，
    // 这里多一行不会破坏 stdout 模式"只有产物"的约定）。
    if crate::ro_embed::RO_MAPS.iter().any(|(k, _)| *k == ident.v8)
        && !crate::decompile::embedded_ro_map_applies()
    {
        text.push_str(crate::bif!(
            "ro_map:        embedded table exists but was built on macOS — not applied here\n               (build one for this platform: `jscd ro-map <probe.jsc> -o names.json`)\n",
            "ro_map:        有内嵌名表，但在 macOS 上提取的 —— 本平台不套用\n               （要真名就在本平台建一张：`jscd ro-map <探针.jsc> -o names.json`）\n"
        ));
    }
    emit(common, &text, &header.render_json(file, &ident), json)
}

/// 常量池字符串提取：遍历对象图取全部字符串字面量（去重保序）。
fn strings_cmd(file: &Path, common: &Common, json_flag: bool) -> Result<(), String> {
    let data = load_input(file)?;
    let (h, table, cache) = parse_cache(&data)?;
    let layout = crate::bytecode::FamilyLayout::from_table(&table);
    let d = crate::disasm::Disassembler::new(&cache, &table, layout)
        .with_source_len(h.source_length())
        .with_ro_map(embedded_ro_map(&table.v8));

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
    let d = crate::disasm::Disassembler::new(&cache, &table, layout)
        .with_source_len(h.source_length())
        .with_ro_map(embedded_ro_map(&table.v8));
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

/// 按表的头布局切出 payload（老族还要跳过预留表与桩键）。
fn t_payload<'a>(data: &'a [u8], table: &crate::tables::VersionTable) -> &'a [u8] {
    let legacy_family = table.serialization.legacy.contains_key("kSpaceMask");
    if !legacy_family {
        return data;
    }
    let hl = &table.header;
    let rd = |off: Option<usize>| -> usize {
        off.and_then(|o| data.get(o..o + 4))
            .and_then(|b| b.try_into().ok())
            .map(u32::from_le_bytes)
            .unwrap_or(0) as usize
    };
    let num_res = rd(hl.num_reservations);
    let num_keys = rd(hl.num_stub_keys);
    let start = (hl.header_size + 4 * (num_res + num_keys) + 7) & !7;
    data.get(start..).unwrap_or(&[])
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
    // 调试：`JSCD_TABLE=6.2.414.78` 强制只用某张表（老族多版本候选混在一起时定位用）
    if let Ok(v) = std::env::var("JSCD_TABLE") {
        if let Some(t) = crate::tables::table_for(&v) {
            return crate::serializer::parse_with(t_payload(data, &t), &t, &[]).map(|c| (h0, t, c));
        }
    }
    // 候选表：精确识别到的排第一；识别不到（老族哈希算法不同）时逐个试
    let mut candidates: Vec<crate::tables::VersionTable> = Vec::new();
    if let Some(t) = crate::tables::table_for(ident.v8) {
        candidates.push(t);
    }
    if candidates.is_empty() {
        candidates = crate::tables::all_tables();
    }
    let mut errors = Vec::new();
    for table in candidates {
        let Ok(h) = crate::header::Header::parse_with(data, &table.header) else {
            continue;
        };
        // 老族（≤8.4）：payload 之前还有 reservation 表 + stub keys，得先跳过；
        // 表本身不带这个字段，按该族的头布局直接读（code-serializer.h：
        // magic/version/source/flag/num_reservations/payload_length/checksum → 对齐到 8）
        // 老族（payload 之前还有 reservation 表 + stub keys）：V8 ≤ 8。
        // **不能只看 `kSpaceMask` 是否存在** —— 9.0–9.3 的 serializer 源码里那些常量还在
        // （我们的提取器照样收进表里），但它们的 .jsc 已经没有预留表了：实测 16.3(9.0) 与
        // 16.20(9.4) 的 payload 起点字节逐位相同。按存在性判会把 9.0–9.3 多跳一段，
        // 报 "unexpected end of payload"。
        let legacy_family = table
            .v8
            .split('.')
            .next()
            .and_then(|m| m.parse::<u32>().ok())
            .map(|m| m <= 8)
            .unwrap_or(false);
        let (payload, reservations) = if legacy_family {
            // 预留表位置按版本取头字段偏移：7.8/8.4 是 num_res@16、表从 32 起；
            // 6.2/6.8 是 num_res@20、num_stub_keys@24、表从 40 起，且表后还有
            // `num_stub_keys` 个 4 字节桩键（早先按 7.8 的固定 16/32 读 6.x，
            // num_res 读成 gpu features 之类的垃圾 → payload 起点远离真身）。
            let hl = &table.header;
            let rd = |off: Option<usize>| -> usize {
                off.and_then(|o| data.get(o..o + 4))
                    .and_then(|b| b.try_into().ok())
                    .map(u32::from_le_bytes)
                    .unwrap_or(0) as usize
            };
            let num_res = match hl.num_reservations {
                Some(o) => rd(Some(o)),
                None => rd(Some(16)),
            };
            let num_keys = rd(hl.num_stub_keys);
            let mut res = Vec::with_capacity(num_res);
            for i in 0..num_res {
                let at = hl.header_size + i * 4;
                let v = data.get(at..at + 4).and_then(|b| b.try_into().ok()).unwrap_or([0; 4]);
                res.push(u32::from_le_bytes(v));
            }
            let unaligned = hl.header_size + 4 * (num_res + num_keys);
            let start = (unaligned + 7) & !7;
            (data.get(start..).unwrap_or(&[]), res)
        } else {
            (h.payload(data), Vec::new())
        };
        // tagged_size 是构建属性（压缩/非压缩），表里给默认值；解析失败时回退另一种
        // 调试：JSCD_TS=4/8 强制某个 tagged size（老族压缩/非压缩构建的判定用）
        let forced = std::env::var("JSCD_TS").ok().and_then(|v| v.parse::<u8>().ok());
        let order: Vec<u8> = match forced {
            Some(v) => vec![v],
            None => vec![table.tagged_size, if table.tagged_size == 8 { 4 } else { 8 }],
        };
        for ts in order {
            let mut t = table.clone();
            t.tagged_size = ts;
            match crate::serializer::parse_with(payload, &t, &reservations) {
                // 解析"读通"不等于读对：版本不匹配时（例如 V8 14.6 的 payload 撞上 13.6
                // 的表）反序列化可能一路不报错，却读成没有函数、没有字节码的空壳 ——
                // 输出只剩运行时前导，看着成功其实全是垃圾。这里必须过结构门禁。
                Ok(c) if cache_has_code(&c, &t) => return Ok((h, t, c)),
                Ok(_) => errors.push(format!(
                    "{} tagged_size={ts}: parsed but contains no function",
                    t.v8
                )),
                Err(e) => {
                    if errors.len() < 40 {
                        errors.push(format!("{} tagged_size={ts}: {e}", t.v8))
                    }
                }
            }
        }
    }
    Err(unsupported_msg(&ident, h0.version_hash, &errors))
}

/// 结构门禁：真实缓存里至少有一个挂着 `BytecodeArray` 的 SFI。
///
/// 空源码也会编出 `StackCheck; Return` 的顶层字节码，所以"一个都没有"只可能意味着
/// 这张表跟这份 payload 不是一路货。
fn cache_has_code(cache: &crate::serializer::CodeCache, table: &crate::tables::VersionTable) -> bool {
    use crate::serializer::{Ref, SlotValue};
    let layout = crate::bytecode::FamilyLayout::from_table(table);
    let d = crate::disasm::Disassembler::new(cache, table, layout)
        .with_ro_map(embedded_ro_map(&table.v8));
    let slots = d.sfi_function_data_slots();
    (0..cache.objects.len()).any(|id| {
        cache.obj(id).ty.is(table, "SharedFunctionInfo")
            && slots.iter().any(|s| {
                matches!(
                    cache.slot_at(id, *s),
                    Some(SlotValue::Ref(Ref::Object(b))) if cache.obj(*b).ty.is(table, "BytecodeArray")
                )
            })
    })
}

/// 候选表全灭时的最终报错：把"版本对不对"和"payload 坏没坏"分开说。
fn unsupported_msg(ident: &crate::tables::Identified, hash: u32, errors: &[String]) -> String {
    let has_table = crate::tables::table_for(ident.v8).is_some();
    // 版本本来就不在支持范围时，逐个候选表的失败是**预期**的，不必倒给用户看；
    // 版本对得上却读不通才值得列几条候选错误，方便判断是截断还是损坏。
    if ident.confidence == "exact" || has_table {
        let head = bif!(
            "cannot parse this .jsc: V8 {0} is supported but the payload did not survive — \
             the file is likely truncated or corrupted",
            "解析失败：V8 {0} 在支持范围内，但 payload 读不通 —— 文件很可能被截断或损坏";
            ident.v8
        );
        let show = &errors[..errors.len().min(4)];
        if show.is_empty() {
            head
        } else {
            format!("{head} ({})", show.join(" | "))
        }
    } else {
        bif!(
            "unsupported .jsc: built by V8 {0} (version hash 0x{1:08x}); this build covers \
             Node 8.0.0–26.10.0 (V8 5.8–14.6) — run `jscd info <file>` for the full header",
            "不支持的 .jsc：由 V8 {0} 生成（version hash 0x{1:08x}）；当前覆盖 \
             Node 8.0.0–26.10.0（V8 5.8–14.6）—— 可先跑 `jscd info <文件>` 看完整头信息";
            ident.v8,
            hash
        )
    }
}

/// 内嵌 ro-map（按精确 V8 版本）包成 `Rc` —— `Disassembler` 用这个形态。
///
/// 注意：这里**不说话**。stdout 模式只输出产物、不许有诊断（tests/cli.rs 的约定）；
/// 平台不符这件事在 `jscd info` 里说明（那本来就是给人看的诊断命令）。
fn embedded_ro_map(v8: &str) -> Option<std::rc::Rc<crate::decompile::RoMap>> {
    crate::decompile::RoMap::embedded(v8).map(std::rc::Rc::new)
}

/// 同上，不过给 `Decompiler`（它收 `Option<RoMap>`）。
fn embedded_ro_map_plain(v8: &str) -> Option<crate::decompile::RoMap> {
    crate::decompile::RoMap::embedded(v8)
}

fn json_out(text: &str) -> String {
    serde_json::json!({ "disassembly": text }).to_string()
}

/// 反编译单个 `.jsc` → 文本（单文件模式与目录模式共用）。
pub fn decompile_text(
    file: &Path,
    ro_map: Option<&Path>,
    runtime: bool,
) -> Result<String, String> {
    let data = load_input(file)?;
    let (_h, table, cache) = parse_cache(&data)?;
    let rmap = match ro_map {
        Some(p) => Some(crate::decompile::RoMap::load(p)?),
        // 按**文件里识别到的** V8 版本查内嵌表（表的代表版本未必等于文件的版本）；
        // 走 embedded_ro_map 是为了在平台不符时也给出一次提示。
        None => embedded_ro_map_plain(crate::tables::identify(_h.version_hash).v8),
    };
    let d = crate::decompile::Decompiler::new(&cache, &table)
        .with_ro_map(rmap)
        .with_runtime(runtime);
    let mut text = String::new();
    d.render_all(&mut text, None)?;
    // 摊平后的函数是文件级定义 → 把 `__uncompiled.<name>` 占位换成真名字
    let text = crate::decompile::link_flat_functions(text);
    // 优化分两层：
    //   ① 文本级（`opt`）：删字节码特有的簿记与搬运、把"空声明 + 单次赋值"并成声明器
    //      —— 便宜、对形态敏感；
    //   ② AST 级（`opt_js`，借 swc）：复制传播 / 无用变量删除 / 常量折叠 —— 需要
    //      控制流与作用域信息，才能真正消掉"只读一次的变量"这类冗余。
    //      产物不是合法 JS 时它返回 None，这里就保持文本级结果。
    //   ② 只跑在"纯代码"输出上：`--runtime` 的前导是手写基础设施，压缩器只会帮倒忙。
    let text = crate::opt::optimize(&text);
    let text = if runtime {
        text
    } else {
        crate::opt_js::optimize(&text).unwrap_or(text)
    };
    // 内置门禁：括号/花括号配平（保证不会输出结构性语法错误）
    syntax_check(&text)?;
    Ok(text)
}

/// `--verify`：产物过一遍 `node --check`。
fn verify_text(src: &str) -> Result<(), String> {
    match node_check(src) {
        Ok(()) => {
            eprintln!("{}", bi!("jscd: syntax check passed (node --check)", "jscd: 语法校验通过（node --check）"));
            Ok(())
        }
        Err(e) => Err(bif!("syntax check failed (node --check): {0}", "语法校验失败（node --check）：{0}"; e)),
    }
}

/// `jscd <输入> [输出]`：输入是文件 → 反编译该文件；是目录 → 递归反编译并保持层级。
///
/// 输出缺省：文件 → stdout；目录 → 与输入同级的 `<输入名>-out/`。
pub fn decompile_path(
    input: &Path,
    output: Option<&Path>,
    quiet: bool,
    ro_map: Option<&Path>,
    verify: bool,
    json: bool,
    runtime: bool,
) -> Result<(), String> {
    let meta = fs::metadata(input).map_err(|e| {
        bif!("cannot read {0}: {1}", "读不了 {0}：{1}"; input.display(), e)
    })?;
    if meta.is_dir() {
        return decompile_dir(input, output, quiet, ro_map, verify, json, runtime);
    }
    // 单文件：`-` 或缺省 → stdout；给了现成目录 → 目录/<同名>.js；否则当文件路径写
    let text = decompile_text(input, ro_map, runtime)?;
    if verify {
        verify_text(&text)?;
    }
    let dest: Option<PathBuf> = match output {
        None => None,
        Some(p) if p == Path::new("-") => None,
        Some(p) if p.is_dir() => Some(p.join(js_name(input))),
        Some(p) => Some(p.to_path_buf()),
    };
    match dest {
        Some(p) => {
            write_js(&p, &text)?;
            if !quiet {
                eprintln!("{}", bif!("wrote {0}", "已写入 {0}"; p.display()));
            }
            if json {
                let line = serde_json::json!({ "source": p.to_string_lossy() }).to_string();
                crate::out::stdout(&format!("{line}\n")).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
        None if json => {
            let line = serde_json::json!({ "source": text }).to_string();
            crate::out::stdout(&format!("{line}\n")).map_err(|e| e.to_string())
        }
        None => {
            let mut text = text;
            if !text.ends_with('\n') {
                text.push('\n');
            }
            crate::out::stdout(&text).map_err(|e| e.to_string())
        }
    }
}

/// 目录模式：递归找 `*.jsc`，保持相对层级写到输出根目录。
fn decompile_dir(
    input: &Path,
    output: Option<&Path>,
    quiet: bool,
    ro_map: Option<&Path>,
    verify: bool,
    json: bool,
    runtime: bool,
) -> Result<(), String> {
    let out_root = match output {
        Some(p) if p == Path::new("-") => {
            return Err(bi!(
                "a directory input needs a directory OUTPUT ('-' = stdout only fits a single file)",
                "目录输入要一个目录作为输出（'-' 只在单文件时表示 stdout）"
            )
            .to_string())
        }
        Some(p) => p.to_path_buf(),
        None => default_out_dir(input),
    };
    let files = collect_jsc(input);
    if files.is_empty() {
        return Err(bif!(
            "no .jsc files under {0}",
            "{0} 下没有找到 .jsc 文件";
            input.display()
        ));
    }
    fs::create_dir_all(&out_root).map_err(|e| {
        bif!("cannot create {0}: {1}", "建不了目录 {0}：{1}"; out_root.display(), e)
    })?;

    let total = files.len();
    let tty = std::io::stderr().is_terminal();
    let mut ok = 0usize;
    let mut written: Vec<(String, String)> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    for (i, f) in files.iter().enumerate() {
        let rel = f.strip_prefix(input).unwrap_or(f.as_path());
        let mut dest = out_root.join(rel);
        dest.set_extension("js");
        let r = decompile_text(f, ro_map, runtime)
            .and_then(|t| {
                if verify {
                    verify_text(&t)?;
                }
                Ok(t)
            })
            .and_then(|t| write_js(&dest, &t));
        match r {
            Ok(()) => {
                ok += 1;
                written.push((f.display().to_string(), dest.display().to_string()));
                if !quiet && tty {
                    eprintln!("[{}/{total}] {}", i + 1, dest.display());
                }
            }
            Err(e) => {
                eprintln!("jscd: {}: {e}", f.display());
                failed.push((f.display().to_string(), e));
            }
        }
    }

    if json {
        let line = serde_json::json!({
            "outDir": out_root.to_string_lossy(),
            "ok": ok,
            "total": total,
            "files": written
                .iter()
                .map(|(a, b)| serde_json::json!({ "input": a, "output": b }))
                .collect::<Vec<_>>(),
            "failed": failed
                .iter()
                .map(|(a, b)| serde_json::json!({ "input": a, "error": b }))
                .collect::<Vec<_>>(),
        })
        .to_string();
        crate::out::stdout(&format!("{line}\n")).map_err(|e| e.to_string())?;
    } else {
        eprintln!(
            "{}",
            bif!(
                "{0}/{1} .jsc -> {2}/",
                "{0}/{1} 个 .jsc -> {2}/";
                ok,
                total,
                out_root.display()
            )
        );
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(bif!("{0} file(s) failed", "{0} 个文件失败"; failed.len()))
    }
}

/// 递归收集 `*.jsc`（排序，保证输出顺序稳定）。
fn collect_jsc(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "jsc") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// 目录输入的默认输出：与输入同级的 `<输入名>-out`（`.` 之类先规范化成真实目录名）。
fn default_out_dir(input: &Path) -> PathBuf {
    let abs = fs::canonicalize(input).unwrap_or_else(|_| input.to_path_buf());
    let name = abs
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "jscd".to_string());
    let parent = abs.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    parent.join(format!("{name}-out"))
}

/// `<stem>.js`（`app.jsc` → `app.js`）。
fn js_name(input: &Path) -> String {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "out".to_string());
    format!("{stem}.js")
}

/// 写 `.js`（必要时建父目录）。
fn write_js(dest: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| {
                bif!("cannot create {0}: {1}", "建不了目录 {0}：{1}"; parent.display(), e)
            })?;
        }
    }
    fs::write(dest, text).map_err(|e| {
        bif!("cannot write {0}: {1}", "写不了 {0}：{1}"; dest.display(), e)
    })
}

/// 外部语法门禁：把产物交给 `node --check` 实编译校验（仅在用户显式要求时执行）。
fn node_check(src: &str) -> Result<(), String> {
    let mut path = std::env::temp_dir();
    path.push(format!("jscd-verify-{}.js", std::process::id()));
    std::fs::write(&path, src).map_err(|e| e.to_string())?;
    let out = std::process::Command::new("node")
        .arg("--check")
        .arg(&path)
        .output()
        .map_err(|e| bif!("cannot run node: {0}", "无法执行 node：{0}"; e))?;
    let _ = std::fs::remove_file(&path);
    if out.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr);
        Err(msg.lines().take(3).collect::<Vec<_>>().join(" | "))
    }
}

/// (probe) 探针 jsc → 只读堆名表：
/// 探针里的每个函数形如 `function p_<name>(o) { return o.<name>; }`，
/// 其常量池中的 ro 引用即该属性名对应的 (chunk, offset)。
fn ro_map_cmd(file: &Path, common: &Common, json_flag: bool) -> Result<(), String> {
    let data = load_input(file)?;
    let (_, table, cache) = parse_cache(&data)?;
    let ts = table.tagged_size as usize;
    let d = crate::decompile::Decompiler::new(&cache, &table);
    let mut entries = serde_json::Map::new();
    for id in 0..cache.objects.len() {
        if !cache.obj(id).ty.is(&table, "SharedFunctionInfo") {
            continue;
        }
        let name = d.sfi_name(id);
        // 名字编码：p_<ident> 或 p_hex_<hex(utf8)>
        let prop: String = if let Some(hex) = name.strip_prefix("p_hex_") {
            let bytes: Option<Vec<u8>> = (0..hex.len() / 2)
                .map(|i| u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok())
                .collect();
            match bytes {
                Some(b) => String::from_utf8_lossy(&b).into_owned(),
                None => continue,
            }
        } else if let Some(p) = name.strip_prefix("p_") {
            if p.is_empty() {
                continue;
            }
            p.to_string()
        } else {
            continue;
        };
        let Some(bca) = d.sfi_function_data_slots().iter().find_map(|slot| {
            match cache.slot_at(id, *slot) {
                Some(crate::serializer::SlotValue::Ref(crate::serializer::Ref::Object(b)))
                    if cache.obj(*b).ty.is(&table, "BytecodeArray") =>
                {
                    Some(*b)
                }
                _ => None,
            }
        }) else {
            continue;
        };
        let Some(pool) = d
            .bca_constant_pool_slot()
            .to_owned()
            .pipe(|s| cache.slot_at(bca, s))
            .and_then(|v| v.as_ref())
            .and_then(|r| cache.ref_object(r))
        else {
            continue;
        };
        let _ = ts;
        for i in 0..cache.array_len(pool) {
            if let Some(crate::serializer::Elem::Ref(crate::serializer::Ref::RoRef(c, o))) =
                cache.array_elem(pool, i)
            {
                entries.insert(format!("{c}/{o}"), serde_json::Value::String(prop.clone()));
            }
        }
    }
    let out = serde_json::json!({
        "schema": "jscd.ro-map/v1",
        "v8": table.v8,
        "tags": table.v8_tags(),
        "entries": entries,
    })
    .to_string();
    if json_flag {
        return emit(common, &out, &out, true);
    }
    emit(common, &out, "", false)
}

trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}
impl<T> Pipe for T {}

/// 语法门禁：括号/花括号配平 + （可选）`node --check` 实编译校验。
///
/// 手写的配平检查对**正则字面量**（`/^(get|set)_…$/` 这种，运行时前言里就有）
/// 会误判：它把 `/` 当除法/注释处理，于是引号与括号的配对整体错位 → 明明 `node --check`
/// 通过的产物被判"unbalanced bracket"而不输出（node10 的 path.js 就栽在这）。
/// 所以配平只是**快速通道**；不通过时用真引擎复核，两边都不过才报错。
pub fn syntax_check(src: &str) -> Result<(), String> {
    if std::env::var("JSCD_NO_GATE").is_ok() {
        return Ok(());
    }
    match bracket_balance(src) {
        Ok(()) => Ok(()),
        Err(e) => match node_check(src) {
            Ok(()) => Ok(()),
            Err(_) => Err(e),
        },
    }
}

/// 括号/花括号/方括号配平（启发式：跳过字符串与注释，不识别正则字面量）。
fn bracket_balance(src: &str) -> Result<(), String> {
    let mut depth = (0i32, 0i32, 0i32); // (), {}, []
    let mut in_str: Option<char> = None;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut prev = '\0';
    for c in src.chars() {
        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
            }
            prev = c;
            continue;
        }
        if in_block_comment {
            if prev == '*' && c == '/' {
                in_block_comment = false;
            }
            prev = c;
            continue;
        }
        if let Some(q) = in_str {
            if c == q && prev != '\\' {
                in_str = None;
            }
            prev = c;
            continue;
        }
        match c {
            '"' | '\'' | '`' => in_str = Some(c),
            '/' if prev == '/' => in_line_comment = true,
            '*' if prev == '/' => in_block_comment = true,
            '(' => depth.0 += 1,
            ')' => depth.0 -= 1,
            '{' => depth.1 += 1,
            '}' => depth.1 -= 1,
            '[' => depth.2 += 1,
            ']' => depth.2 -= 1,
            _ => {}
        }
        if depth.0 < 0 || depth.1 < 0 || depth.2 < 0 {
            return Err(format!("unbalanced bracket near: ...{}", tail(src, c)));
        }
        prev = c;
    }
    if depth != (0, 0, 0) {
        return Err(format!("unbalanced brackets at EOF: {:?}", depth));
    }
    Ok(())
}

/// 失败点的上下文窗口（帮助定位不平衡之处）。
fn tail(src: &str, c: char) -> String {
    let idx = src.find(c).unwrap_or(0);
    // 按 char 边界取窗口（中文注释会让按字节切分 panic）
    let lo = src[..idx].char_indices().rev().nth(59).map(|(i, _)| i).unwrap_or(0);
    let hi = src[idx..]
        .char_indices()
        .nth(60)
        .map(|(i, _)| idx + i)
        .unwrap_or(src.len());
    format!("[@{idx}] {}", &src[lo..hi])
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
            let mut payload = payload.to_string();
            if !payload.ends_with('\n') {
                payload.push('\n');
            }
            crate::out::stdout(&payload).map_err(|e| e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::Identified;

    fn ident(v8: &'static str, conf: &'static str) -> Identified {
        Identified {
            node: "unknown (no matching Node release)",
            v8,
            confidence: conf,
        }
    }

    #[test]
    fn message_names_version_and_range_when_family_has_no_table() {
        // V8 14.6（Node 26）能被爆破认出来，但没有表 → 该说"不支持 + 支持范围"，而不是
        // 把 18 张候选表的失败记录甩给用户。
        let m = unsupported_msg(&ident("15.2.1.7", "brute (left-fold, V8 ≥ 12)"), 0x1234_5678, &[]);
        assert!(m.contains("15.2.1.7"), "{m}");
        assert!(m.contains("Node 8.0.0") && m.contains("V8 5.8"), "{m}");
        assert!(!m.contains("tagged_size"), "支持范围外不该泄漏候选表细节：{m}");
    }

    #[test]
    fn message_keeps_candidate_errors_when_version_is_supported() {
        // 版本对得上却读不通 = 截断/损坏，这时候选表错误才有诊断价值。
        let m = unsupported_msg(
            &ident("13.6.233.17", "brute (left-fold, V8 ≥ 12)"),
            0xfa8c_33dc,
            &["13.6.233.17 tagged_size=8: unexpected end of payload".to_string()],
        );
        assert!(m.contains("13.6.233.17"), "{m}");
        assert!(m.contains("truncated or corrupted"), "{m}");
        assert!(m.contains("unexpected end of payload"), "{m}");
    }
}
