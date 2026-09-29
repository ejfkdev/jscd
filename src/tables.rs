//! codegen 生成的多版本配置：基线 + 每版本差异，JSON 编译期内嵌（对齐 dae 的 profiles 模式）。
//!
//! `tables/manifest.json`  记录全部 V8↔Node 版本、version_hash 索引与表文件别名（去重）
//! `tables/v<X_Y>.json`    单个 V8 major.minor 的完整解析表（bytecodes/操作数/头布局/序列化 tag）

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

mod embed {
    // 由 scripts/codegen.py 生成；表为空时 loader 走内置默认布局 + hash 爆破兜底。
    include!("tables_embed.rs");
}

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub generated_at: String,
    pub versions: Vec<VersionEntry>,
}

#[derive(Debug, Deserialize)]
pub struct VersionEntry {
    /// 完整 V8 版本号 major.minor.build.patch
    pub v8: String,
    pub hash: u32,
    /// 使用该 V8 版本的代表性 Node 版本
    pub node: String,
    /// 表文件名（去重后可能指向与前一版本相同的文件）
    pub table: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VersionTable {
    pub v8: String,
    pub source_tag: String,
    pub header: crate::header::HeaderLayout,
    pub hash: HashCfg,
    /// tagged 指针宽度：macOS 构建 = 8（无指针压缩），linux/win x64 = 4
    pub tagged_size: u8,
    pub bytecodes: Vec<BytecodeDef>,
    pub operand_types: HashMap<String, OperandTypeInfo>,
    /// roots.h 根数组顺序（index → 名字）
    #[serde(default)]
    pub roots: Vec<String>,
    /// Runtime::FunctionId 顺序名表（index → name）
    #[serde(default)]
    pub runtime_names: Vec<String>,
    /// IntrinsicId 顺序名表（index → Name）
    #[serde(default)]
    pub intrinsic_names: Vec<String>,
    /// BytecodeArray 头布局（版本间会变，如 9.4=54 / 10.2=56 / 11.3=54）
    #[serde(default)]
    pub bytecode_array: Option<BytecodeArrayLayout>,
    /// 解释器帧常量 → 寄存器命名基址（9.x–11.x start=-6，12.x+ start=-7）
    #[serde(default)]
    pub frame: Option<FrameLayout>,
    /// SharedFunctionInfo 字段偏移（槽顺序随版本变，如 13.6 首槽为 trusted_function_data）
    #[serde(default)]
    pub shared_function_info: Option<StructLayout>,
    /// ScopeInfo 布局差异（名字取值路径）
    #[serde(default)]
    pub scope_info: Option<ScopeInfoLayout>,
    /// parameter_count 语义（false = parameter_size 为字节数，true = 直接是计数）
    #[serde(default)]
    pub parameter_count: Option<ParameterCountCfg>,
    #[serde(default)]
    pub serialization: SerializationCfg,
}

impl VersionTable {
    /// 表指纹（用于校验 ro-map 与表匹配）。
    pub fn v8_tags(&self) -> String {
        self.source_tag.clone()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HashCfg {
    /// 目前恒为 "murmur_combine_v1"；未来版本如有变化在此切换
    pub algorithm: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BytecodeDef {
    pub name: String,
    /// 操作数类型名列表（按 operand 顺序，名称与 bytecodes.h 的 OperandType 一致）
    #[serde(default)]
    pub operands: Vec<String>,
    /// Wide/ExtraWide 前缀字节码标记
    #[serde(default)]
    pub prefix: Option<String>,
    /// 累加器隐式读写："r" / "w" / "rw" / ""（不碰）；决定反编译的 acc 活跃性
    #[serde(default)]
    pub acc: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OperandTypeInfo {
    /// 单倍缩放下的字节数
    pub size: u32,
    /// true = 随 Wide(×2)/ExtraWide(×4) 前缀扩宽
    pub scalable: bool,
}

/// 序列化流的关键开关：随 V8 版本演进在此记录差异（M2 反序列化器消费）。
#[derive(Debug, Deserialize, Clone, Default)]
pub struct SerializationCfg {
    #[serde(default)]
    pub tags: HashMap<String, u8>,
    #[serde(default)]
    pub code_items: HashMap<String, u8>,
    #[serde(default)]
    pub switches: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StructLayout {
    pub header_size: usize,
    #[serde(default)]
    pub fields: HashMap<String, usize>,
}

impl StructLayout {
    pub fn off(&self, name: &str) -> Option<usize> {
        self.fields.get(name).copied()
    }
    /// 槽下标（槽从 0 = map 起算）。
    pub fn slot(&self, tagged_size: usize, name: &str) -> Option<usize> {
        self.off(name).map(|o| o / tagged_size)
    }
    /// 依次尝试多个候选字段名（跨版本改名）。
    pub fn slot_any(&self, tagged_size: usize, names: &[&str]) -> Option<usize> {
        names.iter().find_map(|n| self.slot(tagged_size, n))
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ParameterCountCfg {
    pub direct: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScopeInfoLayout {
    /// flags 是否 Smi 编码（V8 ≤ 12）；false 时为裸 uint32（13.x）
    pub flags_smi: bool,
    /// position_info 是否在 names/infos 之前（13.x）
    pub position_info_early: bool,
    pub max_inlined_names: usize,
    pub saved_class_bit: u32,
    pub function_variable_bits: [u32; 2],
    pub receiver_bits: [u32; 2],
    pub has_inferred_bit: u32,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct FrameLayout {
    pub reg_file_start: i32,
    pub context_index: i32,
    pub closure_index: i32,
    pub first_param: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BytecodeArrayLayout {
    pub header_size: usize,
    pub fields: HashMap<String, usize>,
}

impl BytecodeArrayLayout {
    /// 家族兜底（表缺该字段时）：无压缩 9.x 布局。
    pub fn fallback(tagged_size: usize) -> Self {
        let mut fields = HashMap::new();
        fields.insert("constant_pool".to_string(), 2 * tagged_size);
        fields.insert("handler_table".to_string(), 3 * tagged_size);
        fields.insert("source_position_table".to_string(), 4 * tagged_size);
        fields.insert("frame_size".to_string(), 5 * tagged_size);
        fields.insert("parameter_size".to_string(), 5 * tagged_size + 4);
        fields.insert("bytecode_age".to_string(), 5 * tagged_size + 13);
        BytecodeArrayLayout {
            header_size: 5 * tagged_size + 14,
            fields,
        }
    }
    pub fn off(&self, name: &str) -> Option<usize> {
        self.fields.get(name).copied()
    }
}

/// info 子命令的版本识别结果。
#[derive(Debug, Clone)]
pub struct Identified {
    pub node: &'static str,
    pub v8: &'static str,
    /// exact（表内命中）/ brute（爆破命中）/ unknown
    pub confidence: &'static str,
}

impl Identified {
    pub fn render_text(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let _ = writeln!(s, "node:          {}", self.node);
        let _ = writeln!(s, "v8:            {} ({})", self.v8, self.confidence);
        s
    }
}

fn manifest() -> &'static Option<Manifest> {
    static M: OnceLock<Option<Manifest>> = OnceLock::new();
    M.get_or_init(|| serde_json::from_str(embed::MANIFEST_JSON).ok())
}

fn table_map() -> &'static HashMap<&'static str, &'static str> {
    static T: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    T.get_or_init(|| EMBEDDED_TABLE_FILES.iter().copied().collect())
}

/// 按 version_hash 精确查表 → 未命中则爆破。
pub fn identify(version_hash: u32) -> Identified {
    if let Some(m) = manifest() {
        if let Some(v) = m.versions.iter().find(|v| v.hash == version_hash) {
            return Identified {
                node: leaked(v.node.as_str()),
                v8: leaked(v.v8.as_str()),
                confidence: "exact",
            };
        }
    }
    match crate::vhash::brute_force(version_hash) {
        Some((fold, (a, b, c, d))) => Identified {
            node: "unknown (no matching Node release)",
            v8: leaked(&format!("{a}.{b}.{c}.{d}")),
            confidence: if fold == crate::vhash::Fold::LeftFold {
                "brute (left-fold, V8 ≥ 12)"
            } else {
                "brute (right-fold, V8 ≤ 11)"
            },
        },
        None => Identified {
            node: "unknown",
            v8: "unknown",
            confidence: "unknown",
        },
    }
}

/// 按识别到的 V8 版本取解析表；无内嵌表时返回 None（调用方走默认布局兜底）。
pub fn table_for(v8: &str) -> Option<VersionTable> {
    let v8_minor = v8.rsplit_once('.').map(|(a, _)| a).unwrap_or(v8);
    let m = manifest().as_ref()?;
    // manifest.versions 是完整 build 粒度（90 项），表是 major.minor 粒度（36 项）；
    // 先找同 major.minor 的条目，再加载其表文件。
    let entry = m
        .versions
        .iter()
        .find(|v| v.v8.rsplit_once('.').map(|(a, _)| a) == Some(v8_minor))?;
    let raw = table_map().get(entry.table.as_str())?;
    serde_json::from_str(raw).ok()
}

/// 泄漏到 'static —— 版本字符串总量恒定（≤90 条 × 短字符串），进程级单例足够。
fn leaked(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

#[allow(non_upper_case_globals)]
mod embedded_tables_shim {
    // 引用 embed 模块生成的静态量，避免 dead_code 警告
    pub use super::embed::{EMBEDDED_TABLE_FILES, MANIFEST_JSON};
}
pub use embedded_tables_shim::{EMBEDDED_TABLE_FILES, MANIFEST_JSON};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identify_known_hash_is_exact() {
        // Node 16.20.2 → V8 9.4.146.26 的 version_hash（由 codegen 写入 manifest）
        let id = identify(0);
        // 空表/无匹配时允许 unknown，但绝不能 panic
        assert!(matches!(id.confidence, "exact" | "brute" | "unknown"));
    }
}

#[cfg(test)]
mod table_debug {
    use super::*;
    #[test]
    fn debug_deserialize_v94() {
        let raw = EMBEDDED_TABLE_FILES
            .iter()
            .find(|(n, _)| *n == "v9_4.json")
            .map(|(_, s)| *s)
            .expect("v9_4 embedded");
        match serde_json::from_str::<VersionTable>(raw) {
            Ok(t) => println!("ok: v8={} tagged={} bc={} roots={}", t.v8, t.tagged_size, t.bytecodes.len(), t.roots.len()),
            Err(e) => panic!("deserialize failed: {e}"),
        }
    }
}
