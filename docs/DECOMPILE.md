# 反编译（decompile）

把字节码重建为**语法合法、尽量可运行**的伪 JS。

## 流水线

```
Decoder ─▶ Instr[] ─┬─ 表达式重建（acc/寄存器状态机，含 V8 的真实操作数语义）
                    ├─ 控制流结构化（if/else、while、switch、break/continue、try/catch）
                    ├─ 名称还原（参数 aN / context 变量名 / 全局名 / RO 名表）
                    └─ AST 打印（括号由优先级最小化插入，语句起始处做 `{`/`function` 防护）
```

## 已实现的关键语义（真机校准）

| 项 | 结论 |
|---|---|
| 二元运算 | **`OP r` = `r OP acc`**（寄存器在左、累加器在右）。证据：`sub(a,b)=a-b` → `Ldar a1; Sub a0`；`sub2=b-a` → `Ldar a0; Sub a1` |
| 比较 | 同规则：`TestLessThan r` = `r < acc`（`x<0` → `LdaZero; TestLessThan a0`） |
| 跳转目标 | `GetAbsoluteOffset(rel) = offset + rel + prefix_size`（Wide 前缀占 1 字节）；`*Constant` 形式取自常量池 Smi |
| 循环 | 回边目标 = 循环头；头指令必须放在循环体内（`continue` 回到顶部需重新求值条件） |
| context 变量 | `LdaCurrentContextSlot [i]` 的 i-2 索引 ScopeInfo.context_locals；函数无自有上下文时沿 `outer_scope_info` 链回溯 |
| 属性名 | 常量池里的字符串；若为**只读堆引用**（`<ro:chunk/off>`）则查 `ro-map`（见下） |
| 13.x 改名 | `LdaNamedProperty`→`GetNamedProperty`、`LdaKeyedProperty`→`GetKeyedProperty`、`Sta*`→`Set*`/`Define*` 等（10.x 起），反编译器同时接受两代名字 |

## 合法性的工程约束

1. 所有语句经 `emit_*` 生成，块结构由递归范围发射保证配平。
2. 非左值赋值目标、非左值 `++/--`、数字字面量成员访问（`0.x`）、保留字变量名（`default`）、
   注释内 `*/`、`{`/`function` 起始语句 —— 都有专门防护。
3. 未知 opcode 退化为 `/* TODO Name operands */` + `__unknown_Name` 占位，**不产出垃圾代码**。
4. 门禁：内置括号配平检查（始终执行）；`--verify` 额外调用 `node --check` 实编译校验。

## 只读堆名表（ro-map）

属性名常以只读堆引用出现（字符串存在 V8 快照里、不在 .jsc 内）。用探针法可还原：

```bash
# 1) 准备候选标识符词表（每行一个）
# 2) 生成探针并提取映射
scripts/build_ro_map.sh 16.20.2 idents.txt ro-map.json
# 3) 反编译时使用
jscd decompile --ro-map ro-map.json --verify app.jsc
```

原理：探针里每个函数只做一次 `o.<name>`，其常量池中的 ro 引用即该名字对应的 (chunk, offset)。
映射与 V8 构建绑定（同一 Node 版本复用）。

## 当前保真度与已知限制

- 已验证：5 个 Node 版本 × 3 个真实源文件 + fixture，**语法门禁 20/20 通过**；
  大语料（14 个真实文件、485KB jsc）产出 678 个函数、31K 行、0 处 TODO 占位。
- 限制：累加器在部分跨结构场景（嵌套循环/长表达式链）会漂移为 `undefined`；
  for-in / class 成员 / async-await 细节尚为近似；未建表的 ro 引用显示为 `<ro..>` 占位。
