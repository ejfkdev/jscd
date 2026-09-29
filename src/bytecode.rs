//! 字节码解码：表驱动（opcode 表 + 操作数宽度/缩放 + 寄存器命名）。
//!
//! BytecodeArray 内存布局（无压缩 ts=8 / 压缩 ts=4，头 = 5*ts + 14）：
//! ```text
//! @0            map
//! @ts           length（Smi）＝字节码字节数
//! @2ts          constant_pool
//! @3ts          handler_table
//! @4ts          source_position_table
//! @5ts          frame_size (int32)
//! @5ts+4        parameter_size (int32)
//! @5ts+8        incoming_new_target_or_generator_register (int32)
//! @5ts+12       osr_loop_nesting_level (int8)
//! @5ts+13       bytecode_age (int8)
//! @5ts+14       bytecode...
//! ```
//! 寄存器编码：`index = reg_file_start - (operand as signed)`；
//! `parameter_index = first_param - index`（0 = this，否则 aN；index ≥ 0 为 rN）。

use crate::tables::{BytecodeDef, OperandTypeInfo, VersionTable};

/// 家族常量（ts = tagged 指针宽度）。
#[derive(Debug, Clone, Copy)]
pub struct FamilyLayout {
    pub tagged_size: usize,
    /// 寄存器文件起始索引（-6）
    pub reg_file_start: i32,
    /// <context> 寄存器索引（-5）
    pub context_index: i32,
    /// <closure> 寄存器索引（-4）
    pub closure_index: i32,
    /// 第一个参数寄存器索引（-8，即 <this>）
    pub first_param: i32,
    /// Wide 前缀缩放因子
    pub wide_scale: u32,
    /// ExtraWide 前缀缩放因子
    pub extra_wide_scale: u32,
}

impl FamilyLayout {
    pub fn new(tagged_size: usize) -> Self {
        // 帧常量（kCPSlotSize=0，standard frame + 2 个 interpreter 附加槽）：
        //   kRegisterFileStartOffset = -6，context = -5，closure = -4，first param = -8
        FamilyLayout {
            tagged_size,
            reg_file_start: -6,
            context_index: -5,
            closure_index: -4,
            first_param: -8,
            wide_scale: 2,
            extra_wide_scale: 4,
        }
    }
    /// BytecodeArray 头 = 5*ts + 14（3 个 tagged + 3×int32 + 2×int8）
    pub fn bca_header(&self) -> usize {
        5 * self.tagged_size + 14
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    Reg(i32),
    RegList { first: i32, count: u32 },
    Imm(i64),
    Idx(u32),
    Flag8(u8),
    RuntimeId(u32),
    IntrinsicId(u32),
    Raw { ty: String, value: u64 },
}

#[derive(Debug, Clone)]
pub struct Instr {
    pub offset: usize,
    pub opcode: u8,
    pub name: String,
    pub operands: Vec<Operand>,
    /// 原始字节（含前缀与操作数）
    pub bytes: Vec<u8>,
    /// 该指令后是否跟源码位置条目（简化：仅记录存在）
    pub scale: u32,
}

pub struct Decoder<'a> {
    table: &'a VersionTable,
    layout: FamilyLayout,
    parameter_count: i32,
}

impl<'a> Decoder<'a> {
    pub fn new(table: &'a VersionTable, layout: FamilyLayout, parameter_count: i32) -> Self {
        Decoder {
            table,
            layout,
            parameter_count,
        }
    }

    fn bc(&self, index: usize) -> Option<&BytecodeDef> {
        self.table.bytecodes.get(index)
    }

    fn operand_info(&self, name: &str) -> OperandTypeInfo {
        self.table
            .operand_types
            .get(name)
            .cloned()
            .unwrap_or(OperandTypeInfo {
                size: 1,
                scalable: true,
            })
    }

    /// 解码整段字节码。
    pub fn decode(&self, code: &[u8]) -> Result<Vec<Instr>, String> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < code.len() {
            let start = i;
            let op_byte = code[i];
            let mut scale = 1u32;
            let mut opcode = op_byte;
            // Wide / ExtraWide 前缀
            let wide_idx = self
                .table
                .bytecodes
                .iter()
                .position(|b| b.prefix.as_deref() == Some("Wide"));
            let extra_idx = self
                .table
                .bytecodes
                .iter()
                .position(|b| b.prefix.as_deref() == Some("ExtraWide"));
            if Some(op_byte as usize) == wide_idx {
                scale = self.layout.wide_scale;
                i += 1;
                opcode = *code.get(i).ok_or("truncated Wide prefix")?;
            } else if Some(op_byte as usize) == extra_idx {
                scale = self.layout.extra_wide_scale;
                i += 1;
                opcode = *code.get(i).ok_or("truncated ExtraWide prefix")?;
            }
            i += 1;
            let def = self
                .bc(opcode as usize)
                .ok_or_else(|| format!("opcode 0x{opcode:02x} out of range at {start}"))?;
            let mut operands = Vec::new();
            let mut k = 0usize;
            while k < def.operands.len() {
                let op_name = def.operands[k].clone();
                // RegList 已消费紧随其后的 RegCount（V8 BytecodeDecoder 同规则）
                if op_name == "RegCount"
                    && k > 0
                    && matches!(def.operands[k - 1].as_str(), "RegList" | "RegOutList")
                {
                    k += 1;
                    continue;
                }
                let info = self.operand_info(&op_name);
                let size = if info.scalable { info.size * scale } else { info.size } as usize;
                if i + size > code.len() {
                    return Err(format!("truncated operand {op_name} of {} at {i}", def.name));
                }
                let raw_bytes = &code[i..i + size];
                i += size;
                let value = read_operand(raw_bytes);
                // RegList/RegOutList：再读一个 RegCount 决定个数
                if matches!(op_name.as_str(), "RegList" | "RegOutList") {
                    let count_info = self.operand_info("RegCount");
                    let count_size = if count_info.scalable {
                        count_info.size * scale
                    } else {
                        count_info.size
                    } as usize;
                    if i + count_size > code.len() {
                        return Err(format!("truncated RegCount of {} at {i}", def.name));
                    }
                    let count = read_operand(&code[i..i + count_size]) as u32;
                    i += count_size;
                    operands.push(Operand::RegList {
                        first: self.reg_index(signed(&value, size)),
                        count,
                    });
                } else {
                    operands.push(self.interpret_operand(&op_name, value, size));
                }
                k += 1;
            }
            let name = match scale {
                1 => def.name.clone(),
                2 => format!("{}_Wide", def.name),
                _ => format!("{}_ExtraWide", def.name),
            };
            out.push(Instr {
                offset: start,
                opcode,
                name,
                operands,
                bytes: code[start..i].to_vec(),
                scale,
            });
        }
        Ok(out)
    }

    /// 操作数编码 → 寄存器索引（Register::FromOperand = kRegisterFileStartOffset - operand）
    fn reg_index(&self, operand: i32) -> i32 {
        self.layout.reg_file_start - operand
    }

    fn interpret_operand(&self, ty: &str, value: u64, size: usize) -> Operand {
        match ty {
            "Reg" | "RegOut" => Operand::Reg(self.reg_index(signed(&value, size))),
            "RegPair" | "RegOutPair" => Operand::RegList {
                first: self.reg_index(signed(&value, size)),
                count: 2,
            },
            "RegOutTriple" => Operand::RegList {
                first: self.reg_index(signed(&value, size)),
                count: 3,
            },
            "Imm" => Operand::Imm(signed(&value, size) as i64),
            "Idx" | "UImm" | "RegCount" | "NativeContextIndex" => Operand::Idx(value as u32),
            "Flag8" => Operand::Flag8(value as u8),
            "RuntimeId" => Operand::RuntimeId(value as u32),
            "IntrinsicId" => Operand::IntrinsicId(value as u32),
            other => Operand::Raw {
                ty: other.to_string(),
                value,
            },
        }
    }

    /// 寄存器名（对齐 V8 Register::ToString，家族常量见 FamilyLayout）。
    pub fn reg_name(&self, index: i32) -> String {
        if index == self.layout.context_index {
            return "<context>".into();
        }
        if index == self.layout.closure_index {
            return "<closure>".into();
        }
        if index < 0 {
            let parameter_index = self.layout.first_param - index;
            if parameter_index == 0 {
                "<this>".into()
            } else {
                format!("a{}", parameter_index - 1)
            }
        } else {
            format!("r{index}")
        }
    }

    pub fn render_operand(&self, op: &Operand) -> String {
        match op {
            Operand::Reg(i) => self.reg_name(*i),
            Operand::RegList { first, count } => {
                if *count == 0 {
                    // V8 RegisterList：count==0 时 first/last 都是 Register(0) → "r0-r0"
                    let r0 = self.reg_name(0);
                    format!("{r0}-{r0}")
                } else {
                    let last = first + (*count as i32 - 1);
                    format!("{}-{}", self.reg_name(*first), self.reg_name(last))
                }
            }
            // V8 BytecodeDecoder：Idx/UImm/Imm → [n]；Flag8 → #n；
            // RuntimeId/IntrinsicId/NativeContextIndex → [名字]
            Operand::Imm(v) => format!("[{v}]"),
            Operand::Idx(v) => format!("[{v}]"),
            Operand::Flag8(v) => format!("#{v}"),
            Operand::RuntimeId(v) => format!("[{}]", self.runtime_name(*v)),
            Operand::IntrinsicId(v) => format!("[_{}]", self.intrinsic_name(*v)),
            Operand::Raw { ty, value } => format!("{ty}:{value}"),
        }
    }

    pub fn parameter_count(&self) -> i32 {
        self.parameter_count
    }

    /// IntrinsicId → 名字（IntrinsicsHelper::ToRuntimeId → Runtime::kInline<Name>）。
    pub fn intrinsic_name(&self, id: u32) -> String {
        self.table
            .intrinsic_names
            .get(id as usize)
            .cloned()
            .unwrap_or_else(|| format!("intrinsic{id}"))
    }

    /// Runtime::FunctionId → 名字（表内提取自 runtime.h；缺表时退回编号）。
    pub fn runtime_name(&self, id: u32) -> String {
        self.table
            .runtime_names
            .get(id as usize)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }
}

fn read_operand(bytes: &[u8]) -> u64 {
    let mut v = 0u64;
    for (i, b) in bytes.iter().enumerate() {
        v |= (*b as u64) << (8 * i);
    }
    v
}

fn signed(value: &u64, size: usize) -> i32 {
    match size {
        1 => (*value as u8 as i8) as i32,
        2 => (*value as u16 as i16) as i32,
        4 => (*value as u32 as i32) as i32,
        _ => *value as i32,
    }
}

