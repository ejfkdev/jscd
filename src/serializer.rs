//! payload 反序列化器：把 V8 通用快照流解析为对象树（格式详见 docs/stream-format.md）。
//!
//! 设计要点（静态解析，不重建堆）：
//! - 流在"槽"粒度自描述：raw chunk（0x60..0x7F / 0x12）与引用编码的首字节空间
//!   不相交，因此对象体无需逐字段布局表即可线性消费——每个 kNewObject 声明
//!   自己的 size（对齐字数），body 消费到量即止。
//! - 语义解释（哪个槽是 name、哪个 chunk 里是 bytecode）按对象类型 + 家族
//!   常量在 `interpret` 层做；家族常量由真机 golden 校准。
//! - backref 按分配序（0-based）；hot 环 8 深度（引用时入环，Root 亦入环）。

use crate::tables::VersionTable;
use serde::Serialize;

pub type ObjId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Ref {
    /// 分配序对象
    Object(ObjId),
    /// roots 表项（索引 = roots.h 顺序）
    Root(usize),
    /// 只读堆 (chunk_index, chunk_offset)
    RoRef(u32, u32),
    /// attached reference（0 = 源码字符串占位）
    Attached(usize),
}

#[derive(Debug, Clone, Serialize)]
pub struct Object {
    /// 语义类型名（由 map 引用解析，如 "SharedFunctionInfo"；未知为 "RoRef#n/n"）
    pub type_name: String,
    /// payload 内的起始偏移（诊断用）
    pub start_offset: usize,
    /// 反序列化后的对象字节数
    pub byte_size: usize,
    /// 槽序列：index 0 = map，其后按对象内存布局
    pub slots: Vec<Slot>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Slot {
    /// 对象内 tagged 槽下标（0 = map）
    pub index: usize,
    pub value: SlotValue,
}

#[derive(Debug, Clone, Serialize)]
pub enum SlotValue {
    Ref(Ref),
    /// 连续相同引用（FixedRepeat/VariableRepeat）
    Repeat(usize, Box<SlotValue>),
    /// raw chunk（覆盖 n 个 tagged 槽：Smi 串/字段域/字节码本体）
    Raw(Vec<u8>),
    ClearedWeak,
    /// kWeakPrefix 后随的引用
    WeakRef(Box<SlotValue>),
    /// 尚未 resolve 的 pending forward ref（resolve 后改写为 Ref::Object）
    PendingRef(u32),
}

impl SlotValue {
    pub fn as_ref(&self) -> Option<Ref> {
        match self {
            SlotValue::Ref(r) => Some(*r),
            SlotValue::WeakRef(inner) => inner.as_ref().as_ref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HotEntry {
    Object(ObjId),
    Root(usize),
}

/// HotObjectsList：固定 8 槽环形数组，写入即前进（与 serializer.h 语义一致，
/// **不是** FIFO——位置不随淘汰移动）。
#[derive(Debug, Clone, Default)]
struct HotRing {
    slots: [Option<HotEntry>; 8],
    index: usize,
}

impl HotRing {
    fn add(&mut self, e: HotEntry) {
        self.slots[self.index] = Some(e);
        self.index = (self.index + 1) & 7;
    }
    fn get(&self, i: usize) -> Option<HotEntry> {
        self.slots.get(i).copied().flatten()
    }
}

pub struct CodeCache {
    pub objects: Vec<Object>,
    /// 顶层（第一个序列化的）SFI
    pub top_sfi: ObjId,
    /// 解析到的表信息引用（interpret 层用）
    pub roots: Vec<String>,
}

struct Walker<'a> {
    data: &'a [u8],
    pos: usize,
    tagged_size: usize,
    tags: &'a std::collections::HashMap<String, u8>,
    roots: &'a [String],
    objects: Vec<Object>,
    hot: HotRing,
    /// pending forward ref id → 待改写槽（对象 id, slots 向量下标）
    pending: std::collections::HashMap<u32, Vec<(ObjId, usize)>>,
    /// 最近操作面包屑（错误诊断用）
    trace: std::collections::VecDeque<String>,
}

type R<T> = Result<T, String>;

impl<'a> Walker<'a> {
    fn tag(&self, name: &str) -> R<u8> {
        self.tags
            .get(name)
            .copied()
            .ok_or_else(|| format!("tag {name} missing from table"))
    }

    /// 可选 tag（版本间增删；缺失时返回 None 而不是报错）。
    fn opt_tag(&self, name: &str) -> Option<u8> {
        self.tags.get(name).copied()
    }

    /// 别名解析（如 kVariableRepeat / kVariableRepeatRoot，kFixedRepeat / kFixedRepeatRoot）。
    fn tag_any(&self, names: &[&str]) -> Option<u8> {
        names.iter().find_map(|n| self.opt_tag(n))
    }

    fn byte(&mut self) -> R<u8> {
        let b = *self.data.get(self.pos).ok_or("unexpected end of payload")?;
        self.pos += 1;
        Ok(b)
    }

    fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    /// PutInt 变体：首字节低 2 位 = 字节数-1，小端，值 = raw >> 2。
    fn putint(&mut self) -> R<u32> {
        let b0 = self.byte()?;
        let n = (b0 & 3) as usize + 1;
        let mut raw = b0 as u32;
        for i in 1..n {
            raw |= (self.byte()? as u32) << (8 * i);
        }
        Ok(raw >> 2)
    }

    fn raw(&mut self, n: usize) -> R<Vec<u8>> {
        if self.pos + n > self.data.len() {
            return Err(format!("raw read {n} bytes overruns payload"));
        }
        let d = self.data[self.pos..self.pos + n].to_vec();
        self.pos += n;
        Ok(d)
    }

    fn push_slot(&mut self, id: ObjId, index: usize, value: SlotValue) {
        self.objects[id].slots.push(Slot { index, value });
    }

    fn hot_push(&mut self, e: HotEntry) {
        self.hot.add(e);
    }

    /// 解析一个引用槽（含 kNewObject 递归、repeat 展开）。
    fn parse_ref(&mut self, depth: usize) -> R<SlotValue> {
        if depth > 64 {
            return Err("ref recursion too deep".into());
        }
        let b = self.byte()?;
        let t_new = self.tag("kNewObject")?;
        let t_backref = self.tag("kBackref")?;
        let t_hot = self.tag("kHotObject")?;
        let t_root_const = self.tag("kRootArrayConstants")?;
        let t_fixed_raw = self.tag("kFixedRawData")?;
        // repeat 在 13.x 拆成 *Root 变体（编码为 [count][1 字节 root 索引]）
        let t_fixed_repeat = self.tag_any(&["kFixedRepeat", "kFixedRepeatRoot"]);
        let t_fixed_repeat_root_only = self.opt_tag("kFixedRepeat").is_none()
            && self.opt_tag("kFixedRepeatRoot").is_some();

        if self.trace.len() >= 10 {
            self.trace.pop_front();
        }
        self.trace
            .push_back(format!("@{} 0x{b:02x}", self.pos - 1));
        if (t_new..t_backref).contains(&b) {
            let id = self.parse_new_object(depth)?;
            Ok(SlotValue::Ref(Ref::Object(id)))
        } else if b == t_backref {
            let idx = self.putint()? as usize;
            if idx >= self.objects.len() {
                return Err(format!("backref {idx} out of range ({})", self.objects.len()));
            }
            // PutBackReference 会把该对象加入 hot 环
            self.hot_push(HotEntry::Object(idx));
            Ok(SlotValue::Ref(Ref::Object(idx)))
        } else if (t_hot..t_hot + 8).contains(&b) {
            let i = (b - t_hot) as usize;
            match self.hot.get(i) {
                Some(HotEntry::Object(id)) => Ok(SlotValue::Ref(Ref::Object(id))),
                Some(HotEntry::Root(r)) => Ok(SlotValue::Ref(Ref::Root(r))),
                None => Err(format!("hot object index {i} out of ring")),
            }
        } else if b == self.tag("kRootArray")? {
            let idx = self.putint()? as usize;
            self.hot_push(HotEntry::Root(idx));
            Ok(SlotValue::Ref(Ref::Root(idx)))
        } else if (t_root_const..t_root_const + 32).contains(&b) {
            Ok(SlotValue::Ref(Ref::Root((b - t_root_const) as usize)))
        } else if b == self.tag("kReadOnlyHeapRef")? {
            let c = self.putint()?;
            let o = self.putint()?;
            Ok(SlotValue::Ref(Ref::RoRef(c, o)))
        } else if b == self.tag("kAttachedReference")? {
            Ok(SlotValue::Ref(Ref::Attached(self.putint()? as usize)))
        } else if Some(b) == self.tag_any(&["kStartupObjectCache"])
            || Some(b) == self.tag_any(&["kReadOnlyObjectCache", "kSharedHeapObjectCache"])
        {
            let i = self.putint()? as usize;
            Ok(SlotValue::Ref(Ref::RoRef(u32::MAX, i as u32)))
        } else if Some(b) == self.opt_tag("kRegisterPendingForwardRef") {
            Ok(SlotValue::PendingRef(self.putint()?))
        } else if Some(b) == self.opt_tag("kClearedWeakReference") {
            Ok(SlotValue::ClearedWeak)
        } else if Some(b) == self.opt_tag("kWeakPrefix")
            || Some(b) == self.opt_tag("kIndirectPointerPrefix")
            || Some(b) == self.opt_tag("kProtectedPointerPrefix")
        {
            // 前缀类 tag（weak / 间接指针 / 受保护指针）：后随一个引用
            let inner = self.parse_ref(depth + 1)?;
            Ok(SlotValue::WeakRef(Box::new(inner)))
        } else if Some(b) == self.opt_tag("kInitializeSelfIndirectPointer") {
            Ok(SlotValue::ClearedWeak) // 仅初始化 host 字段，无流数据
        } else if Some(b) == self.opt_tag("kAllocateJSDispatchEntry")
            || Some(b) == self.opt_tag("kJSDispatchEntry")
        {
            let _ = self.putint()?; // parameter_count / entry_id
            Ok(SlotValue::ClearedWeak)
        } else if (t_fixed_raw..t_fixed_raw + 32).contains(&b) {
            let n = (b - t_fixed_raw + 1) as usize;
            Ok(SlotValue::Raw(self.raw(n * self.tagged_size)?))
        } else if Some(b) == self.opt_tag("kVariableRawData") {
            let n = self.putint()? as usize;
            Ok(SlotValue::Raw(self.raw(n * self.tagged_size)?))
        } else if t_fixed_repeat.map_or(false, |t| (t..t + 16).contains(&b)) {
            let base = t_fixed_repeat.unwrap();
            let n = (b - base + 2) as usize;
            if t_fixed_repeat_root_only {
                // FixedRepeatRoot：count + 1 字节 root 索引
                let root = self.byte()? as usize;
                Ok(SlotValue::Repeat(n, Box::new(SlotValue::Ref(Ref::Root(root)))))
            } else {
                let inner = self.parse_ref(depth + 1)?;
                Ok(SlotValue::Repeat(n, Box::new(inner)))
            }
        } else if Some(b) == self.tag_any(&["kVariableRepeat"]) {
            let n = self.putint()? as usize + 18;
            let inner = self.parse_ref(depth + 1)?;
            Ok(SlotValue::Repeat(n, Box::new(inner)))
        } else if Some(b) == self.opt_tag("kVariableRepeatRoot") {
            let n = self.putint()? as usize + 18;
            let root = self.byte()? as usize;
            Ok(SlotValue::Repeat(n, Box::new(SlotValue::Ref(Ref::Root(root)))))
        } else if Some(b) == self.opt_tag("kNewMetaMap")
            || Some(b) == self.opt_tag("kNewContextlessMetaMap")
            || Some(b) == self.opt_tag("kNewContextfulMetaMap")
        {
            // meta map：无 size/map 字段，body 直接跟在后面（长度取 Map 的固定大小）
            Err(format!(
                "meta map tag 0x{b:02x} at {} not supported (rare in code caches)",
                self.pos - 1
            ))
        } else {
            Err(format!(
                "unknown serialization tag 0x{b:02x} at {} (recent: {:?})",
                self.pos - 1,
                self.trace.iter().rev().take(8).collect::<Vec<_>>()
            ))
        }
    }

    /// kNewObject：size + map + resolve 段 + body（消费到 size 词数为止）。
    fn parse_new_object(&mut self, depth: usize) -> R<ObjId> {
        let tag_offset = self.pos.saturating_sub(1); // kNewObject tag 字节位置
        let size_words = self.putint()? as usize;
        let byte_size = size_words * 8;
        let id = self.objects.len();
        if std::env::var("JSCD_TRACE").is_ok() {
            eprintln!(
                "[obj #{id}] tag=0x{:02x} @{} size={size_words}w depth={depth}",
                self.data[tag_offset], tag_offset
            );
        }
        self.objects.push(Object {
            type_name: "?unclassified".into(),
            start_offset: tag_offset,
            byte_size,
            slots: Vec::new(),
        });
        // 槽 0 = map（递归）
        let map = self.parse_ref(depth + 1)?;
        self.push_slot(id, 0, map);
        // map 之后：kResolvePendingForwardRef*（把挂起引用绑定到本对象）
        let t_resolve = self.tag("kResolvePendingForwardRef")?;
        loop {
            match self.peek() {
                Some(b) if b == t_resolve => {
                    self.byte()?;
                    let pid = self.putint()?;
                    if let Some(list) = self.pending.get_mut(&pid) {
                        for (oid, vec_pos) in list.drain(..) {
                            let old = std::mem::replace(
                                &mut self.objects[oid].slots[vec_pos].value,
                                SlotValue::ClearedWeak,
                            );
                            let _ = old;
                            self.objects[oid].slots[vec_pos].value =
                                SlotValue::Ref(Ref::Object(id));
                        }
                    }
                    self.pending.remove(&pid);
                }
                _ => break,
            }
        }
        // body：消费到 size 个 tagged 槽
        let mut consumed = 1usize; // map 槽
        let mut slot_index = 1usize;
        while consumed < size_words {
            let before = self.pos;
            let v = self.parse_ref(depth + 1)?;
            let n_slots = match &v {
                SlotValue::Raw(d) => d.len() / self.tagged_size,
                SlotValue::Repeat(n, _) => *n,
                _ => 1,
            };
            if let SlotValue::PendingRef(pid) = &v {
                let vec_pos = self.objects[id].slots.len();
                self.pending.entry(*pid).or_default().push((id, vec_pos));
            }
            if std::env::var("JSCD_TRACE").is_ok() {
                eprintln!("[obj #{id}] unit slot={slot_index} n={n_slots} consumed={consumed}+{n_slots} @{} -> {}", before, self.pos);
            }
            self.push_slot(id, slot_index, v);
            consumed += n_slots;
            slot_index += n_slots;
            let _ = before;
        }
        if std::env::var("JSCD_TRACE").is_ok() {
            eprintln!("[obj #{id}] done @{} consumed={consumed}", self.pos);
        }
        if consumed != size_words {
            let units: Vec<String> = self.objects[id]
                .slots
                .iter()
                .map(|s| format!("{}:{:?}", s.index, unit_kind(&s.value)))
                .collect();
            return Err(format!(
                "object {id} (at payload +{} tag=0x{:02x} size={}) size mismatch: consumed {consumed} of {size_words} words; units=[{}]",
                self.objects[id].start_offset,
                self.data.get(self.objects[id].start_offset).copied().unwrap_or(0),
                size_words,
                units.join(" ")
            ));
        }
        Ok(id)
    }
}

/// 解析整个 payload（已解压、已去头）。
pub fn parse(payload: &[u8], table: &VersionTable) -> R<CodeCache> {
    // 最小必需集：跨版本恒存在（可选项按需 opt_tag 取）
    let need = [
        "kNewObject",
        "kBackref",
        "kRootArray",
        "kReadOnlyHeapRef",
        "kAttachedReference",
        "kSynchronize",
        "kNop",
        "kHotObject",
        "kRootArrayConstants",
        "kFixedRawData",
    ];
    for n in need {
        if !table.serialization.tags.contains_key(n) {
            return Err(format!("table missing serialization tag {n}"));
        }
    }
    let mut w = Walker {
        data: payload,
        pos: 0,
        tagged_size: table.tagged_size as usize,
        tags: &table.serialization.tags,
        roots: &table.roots,
        objects: Vec::new(),
        hot: HotRing::default(),
        pending: std::collections::HashMap::new(),
        trace: std::collections::VecDeque::new(),
    };
    // 顶层对象必须是 kNewObject（tag 已在此消费，parse_new_object 从 size 开始读）
    let b = w.byte()?;
    let t_new = w.tag("kNewObject")?;
    if !(t_new..w.tag("kBackref")?).contains(&b) {
        return Err(format!(
            "payload does not start with kNewObject (got 0x{b:02x})"
        ));
    }
    let top = w.parse_new_object(0)?;
    // deferred 段：直到 kSynchronize
    let t_sync = w.tag("kSynchronize")?;
    loop {
        match w.peek() {
            None => return Err("payload ended before kSynchronize".into()),
            Some(b) if b == t_sync => {
                w.byte()?;
                break;
            }
            Some(_) => {
                let before = w.pos;
                w.parse_ref(0)
                    .map_err(|e| format!("deferred section: {e}"))?;
                if w.pos == before {
                    return Err("deferred section made no progress".into());
                }
            }
        }
    }
    // 尾部 kNop 填充
    let t_nop = w.tag("kNop")?;
    while let Some(b) = w.peek() {
        if b != t_nop {
            return Err(format!(
                "unexpected byte 0x{b:02x} after kSynchronize at {}",
                w.pos
            ));
        }
        w.byte()?;
    }
    // 分类：map 引用 → 类型名；roots 表缺失段（torque 生成段）走结构指纹兜底
    let roots: Vec<String> = table.roots.to_vec();
    for id in 0..w.objects.len() {
        let by_map = w.objects[id]
            .slots
            .first()
            .and_then(|s| s.value.as_ref())
            .and_then(|r| match r {
                Ref::Root(i) => roots.get(i).map(|s| map_type_name(s)),
                Ref::Object(m) => Some(format!(
                    "Map(instance_type={})",
                    map_instance_type(&w.objects[m]).unwrap_or(0)
                )),
                Ref::RoRef(c, o) => Some(format!("?ro:{c}/{o}")),
                _ => None,
            })
            .filter(|n| !n.starts_with('?') && !n.starts_with("String:") && !n.starts_with("Symbol:") && !n.starts_with("AccessorInfo:"));
        let type_name = by_map.unwrap_or_else(|| structural_type(&w.objects[id]).to_string());
        w.objects[id].type_name = type_name;
    }
    Ok(CodeCache {
        objects: w.objects,
        top_sfi: top,
        roots,
    })
}

/// 单元形态摘要（诊断用）。
fn unit_kind(v: &SlotValue) -> String {
    match v {
        SlotValue::Ref(Ref::Object(i)) => format!("obj#{i}"),
        SlotValue::Ref(Ref::Root(i)) => format!("root#{i}"),
        SlotValue::Ref(Ref::RoRef(c, o)) => format!("ro{c}/{o}"),
        SlotValue::Ref(Ref::Attached(i)) => format!("attached#{i}"),
        SlotValue::Repeat(n, inner) => format!("repeat×{n}({})", unit_kind(inner)),
        SlotValue::Raw(d) => format!("raw{}B", d.len()),
        SlotValue::ClearedWeak => "cleared".into(),
        SlotValue::WeakRef(inner) => format!("weak({})", unit_kind(inner)),
        SlotValue::PendingRef(id) => format!("pending#{id}"),
    }
}

fn map_type_name(root_name: &str) -> String {
    root_name
        .strip_suffix("Map")
        .unwrap_or(root_name)
        .to_string()
}

/// Map 对象 body 的第一个 raw chunk 里含 instance_type（@12..14，对象内偏移）。
fn map_instance_type(map: &Object) -> Option<u16> {
    for s in &map.slots {
        if let SlotValue::Raw(d) = &s.value {
            // chunk 覆盖对象内 [s.index*8, +d.len())；instance_type 在 @12
            let start = s.index * 8;
            if start <= 12 && start + d.len() >= 14 {
                let off = 12 - start;
                return Some(u16::from_le_bytes([d[off], d[off + 1]]));
            }
        }
    }
    None
}

/// 结构指纹分类：roots 表解析不到 map 名时的兜底（9–11 家族，无指针压缩）。
/// 指纹 = (byte_size, 槽形态)；随家族扩展在 layout 模块维护。
fn structural_type(o: &Object) -> &'static str {
    // Script：source 槽恒为 attached ref 0（源码占位），定长对象
    if o.byte_size >= 96 {
        if matches!(
            o.slots.get(1),
            Some(Slot {
                value: SlotValue::Ref(Ref::Attached(0)),
                ..
            })
        ) {
            return "Script";
        }
    }
    // UncompiledDataWithoutPreparseData：map + inferred_name(ptr) + start/end(i32×2)
    if o.byte_size == 24 {
        let s1_ref = matches!(
            o.slots.get(1),
            Some(Slot {
                value: SlotValue::Ref(_),
                ..
            })
        );
        let s2_raw8 = matches!(
            o.slots.get(2),
            Some(Slot {
                value: SlotValue::Raw(d),
                ..
            }) if d.len() == 8
        );
        if s1_ref && s2_raw8 {
            return "UncompiledDataWithoutPreparseData";
        }
    }
    // UncompiledDataWithPreparseData：map + name(ptr) + raw8 + preparse_data(ptr)
    if o.byte_size == 40 {
        let s1_ref = matches!(
            o.slots.get(1),
            Some(Slot {
                value: SlotValue::Ref(_),
                ..
            })
        );
        let s2_raw8 = matches!(
            o.slots.get(2),
            Some(Slot {
                value: SlotValue::Raw(d),
                ..
            }) if d.len() == 8
        );
        let s3_ref = matches!(
            o.slots.get(3),
            Some(Slot {
                value: SlotValue::Ref(_),
                ..
            })
        );
        if s1_ref && s2_raw8 && s3_ref {
            return "UncompiledDataWithPreparseData";
        }
    }
    "?unclassified"
}

impl CodeCache {
    pub fn obj(&self, id: ObjId) -> &Object {
        &self.objects[id]
    }

    pub fn ref_object(&self, r: Ref) -> Option<ObjId> {
        match r {
            Ref::Object(id) => Some(id),
            _ => None,
        }
    }

    /// 引用槽按对象内槽号取值（repeat 视为一个槽）。
    pub fn slot_at(&self, id: ObjId, index: usize) -> Option<&SlotValue> {
        self.objects[id]
            .slots
            .iter()
            .find(|s| s.index == index)
            .map(|s| &s.value)
    }

    /// 引用槽按对象内字节偏移取值（repeat/chunk 展开到偏移粒度）。
    pub fn unit_at(&self, id: ObjId, byte_off: usize) -> Option<&SlotValue> {
        let tagged = 8; // 家族常量：9-11 无压缩
        let want = byte_off / tagged;
        self.slot_at(id, want)
    }

    /// 解引用（穿透 WeakRef；Pending 已在解析期改写）。
    pub fn deref(&self, v: &SlotValue) -> Option<Ref> {
        v.as_ref()
    }

    /// 无压缩 Smi 解码（8 字节：值在高 32 位）。
    pub fn smi64(d: &[u8]) -> Option<i64> {
        if d.len() < 8 {
            return None;
        }
        Some(i64::from_le_bytes(d[..8].try_into().unwrap()) >> 32)
    }

    /// 找对象 id 对应的 SlotValue（Raw 剥离取第 n 个 tagged 词）。
    pub fn raw_at(&self, id: ObjId, byte_off: usize, len: usize) -> Option<&[u8]> {
        let tagged = 8;
        for s in &self.objects[id].slots {
            if let SlotValue::Raw(d) = &s.value {
                let start = s.index * tagged;
                if byte_off >= start && byte_off + len <= start + d.len() {
                    return Some(&d[byte_off - start..byte_off - start + len]);
                }
            }
        }
        None
    }
}

// ---------------------------------------------------------------- 解释层

/// 对象类型判定的家族常量与帮助函数（9.x–11.x 家族，无指针压缩）。
pub mod layout {
    use super::{CodeCache, Object, ObjId, SlotValue};

    /// BytecodeArray::kHeaderSize（V8 9.4–11.3，无压缩）：
    /// FixedArrayBase(16) + 3 指针(24) + frame/param/incoming(12) + osr/age(2) → 对齐 56
    pub const BCA_HEADER: usize = 56;
    /// SFI 大小（同上家族）= 7 词
    pub const SFI_SIZE: usize = 56;

    pub fn is_sfi(o: &Object) -> bool {
        o.type_name == "SharedFunctionInfo" && o.byte_size == SFI_SIZE
    }

    pub fn is_bytecode_array(o: &Object) -> bool {
        o.type_name == "BytecodeArray"
    }

    /// SFI 字段读取：槽 1..4 = function_data / name_or_scope_info /
    /// outer_scope_info_or_feedback_metadata / script_or_debug_info；
    /// raw tail @40..56 = [length i16][formal_parameter_count u16]
    /// [function_token_offset u16][expected_nof_properties u8][flags2 u8]
    /// [flags u32][function_literal_id i32]。
    pub fn sfi_u16(c: &CodeCache, id: ObjId, off: usize) -> Option<u16> {
        let d = c.raw_at(id, off, 2)?;
        Some(u16::from_le_bytes(d.try_into().unwrap()))
    }
    pub fn sfi_u32(c: &CodeCache, id: ObjId, off: usize) -> Option<u32> {
        let d = c.raw_at(id, off, 4)?;
        Some(u32::from_le_bytes(d.try_into().unwrap()))
    }

    /// 无用告警占位（Raw chunk 直接给 SlotValue）
    pub fn raw_value(o: &Object, index: usize) -> Option<&Vec<u8>> {
        o.slots.iter().find(|s| s.index == index).and_then(|s| match &s.value {
            SlotValue::Raw(d) => Some(d),
            _ => None,
        })
    }
}
