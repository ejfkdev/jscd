//! 命令行信息的双语支持（对齐 ddc 的做法，另加 OS 级回退）。
//!
//! 判定顺序：`JSCD_LANG` → `LC_ALL` → `LC_MESSAGES` → `LANGUAGE` → `LANG`
//! → `LC_CTYPE` →（仅 Windows）进程内的 `GetUserDefaultLocaleName` → 英文。
//!
//! 优先吃命令行环境变量：cmd / PowerShell / Git Bash 里设了 `LANG`、`LC_ALL` 之类的
//! 直接用，**不碰系统 API**；一个都没给（Windows 上 cmd 默认就是这种情形）才走
//! `os_locale()`，那里是 `windows-sys` 的进程内调用，每个进程至多一次，
//! 且整段被 `catch_unwind` + 长度校验包住 —— API 返回 0、给出越界长度、甚至内部
//! panic，都只会退化成"没拿到"，程序照常跑英文，不会崩。
//!
//! 刻意**不起子进程**：macOS 上不跑 `defaults read -g AppleLocale`（那要 fork 一次），
//! 只认环境变量 —— 终端里 `LANG`/`LC_ALL` 本来就有；GUI 启动、环境变量全空的进程
//! 落到英文，这是可接受的默认。
//!
//! 任何以 `zh` 开头的取值（`zh`、`zh_CN`、`zh_SG`、`zh_TW`、`zh_HK`、`zh_MO`…）
//! 都算中文 —— 简繁、港澳台一视同仁；其余一律英文。`JSCD_LANG=en*` 是显式选择，
//! 即使 `LANG=zh_CN.UTF-8` 也压得住。

use std::sync::OnceLock;

/// 依优先级排列的语言来源。前四个是用户显式/惯例变量，`LANGUAGE` 是 gettext，
/// `LC_CTYPE` 是 MSYS / Git Bash 在 Windows 上实际会带出来的那几个。
const ENV_VARS: [&str; 6] = [
    "JSCD_LANG",
    "LC_ALL",
    "LC_MESSAGES",
    "LANGUAGE",
    "LANG",
    "LC_CTYPE",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    En,
    Zh,
}

impl Lang {
    fn from_value(v: &str) -> Option<Lang> {
        let v = v.trim().to_ascii_lowercase();
        if v.is_empty() {
            return None;
        }
        if v.starts_with("zh") {
            return Some(Lang::Zh);
        }
        // C/POSIX/其它一律当"没说" → 交给下一个来源
        if v.starts_with("c") || v.starts_with("posix") {
            return None;
        }
        Some(Lang::En)
    }

    /// 依次问环境变量；"没说"（`C`/`POSIX`/空）跳过，继续问下一个。
    fn from_env(mut get: impl FnMut(&str) -> Option<String>) -> Option<Lang> {
        ENV_VARS
            .iter()
            .find_map(|var| get(var).and_then(|v| Lang::from_value(&v)))
    }

    fn detect() -> Lang {
        Lang::from_env(|k| std::env::var(k).ok())
            .or_else(|| os_locale().and_then(|v| Lang::from_value(&v)))
            .unwrap_or(Lang::En)
    }
}

/// 系统区域设置：只在环境变量一个都没答上来时才被问到。
///
/// 全程不 panic —— 见文件头的说明。
#[cfg(windows)]
fn os_locale() -> Option<String> {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;

    // 最后的兜底，整段裹进 catch_unwind：返回值离谱、编码坏掉、真出 panic，
    // 一律退化成 None（→ 英文），绝不带崩进程。
    catch_unwind(AssertUnwindSafe(|| -> Option<String> {
        let mut buf = [0u16; 85]; // LOCALE_NAME_MAX_LENGTH
        let n = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
        // 0 = 调用失败，1 = 只写了终止符，负值纯属意外
        let n = usize::try_from(n).ok().filter(|n| *n > 1)?;
        // 万一长度越过缓冲区，夹住再切 —— 宁可少读也不越界
        let n = n.min(buf.len());
        String::from_utf16(&buf[..n - 1]).ok()
    }))
    .ok()
    .flatten()
}

/// macOS / 其它 Unix：不额外起作用 —— 语言只认环境变量，不 fork 子进程。
#[cfg(not(windows))]
fn os_locale() -> Option<String> {
    None
}

/// 进程级语言，只解析一次。
pub fn lang() -> Lang {
    static LANG: OnceLock<Lang> = OnceLock::new();
    *LANG.get_or_init(Lang::detect)
}

/// 按语言二选一（两个都是字面量，编译期不拼接）。
pub fn pick(en: &'static str, zh: &'static str) -> &'static str {
    match lang() {
        Lang::En => en,
        Lang::Zh => zh,
    }
}

/// 与 `pick` 相同，但两个都是已经拼好的 `String`。
pub fn pick_owned(en: String, zh: String) -> String {
    match lang() {
        Lang::En => en,
        Lang::Zh => zh,
    }
}

/// `bi!("english", "中文")` → 取其中一个字面量。
#[macro_export]
macro_rules! bi {
    ($en:expr, $zh:expr) => {
        $crate::lang::pick($en, $zh)
    };
}

/// `bif!("{} files", "{} 个文件"; n)` → 对选中的字面量做 `format!`。
/// 两种语言的格式串共用同一组实参；语序不同就用 `{0}`/`{1}` 定位。
#[macro_export]
macro_rules! bif {
    ($en:literal, $zh:literal) => {
        $crate::lang::pick($en, $zh)
    };
    ($en:literal, $zh:literal; $($arg:tt)*) => {
        match $crate::lang::lang() {
            $crate::lang::Lang::Zh => format!($zh, $($arg)*),
            $crate::lang::Lang::En => format!($en, $($arg)*),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zh_values_all_count_as_chinese() {
        for v in [
            "zh",
            "zh_CN",
            "zh_CN.UTF-8",
            "zh_TW",
            "zh_TW.UTF-8",
            "zh_HK",
            "zh_MO",
            "zh_SG",
            "ZH_cn",
        ] {
            assert_eq!(Lang::from_value(v), Some(Lang::Zh), "{v}");
        }
    }

    #[test]
    fn other_values_are_english_or_unset() {
        assert_eq!(Lang::from_value("en_US.UTF-8"), Some(Lang::En));
        assert_eq!(Lang::from_value("fr_FR"), Some(Lang::En));
        assert_eq!(Lang::from_value("ja_JP.UTF-8"), Some(Lang::En));
        // C/POSIX 视为"没说"
        assert_eq!(Lang::from_value("C"), None);
        assert_eq!(Lang::from_value("POSIX"), None);
        assert_eq!(Lang::from_value(""), None);
    }

    #[test]
    fn env_priority_and_fallthrough() {
        fn env(pairs: &[(&str, &str)]) -> Option<Lang> {
            Lang::from_env(|k| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| (*v).to_string())
            })
        }

        assert_eq!(env(&[("LANG", "zh_CN.UTF-8")]), Some(Lang::Zh));
        // JSCD_LANG 压过其余全部
        assert_eq!(
            env(&[("JSCD_LANG", "en"), ("LANG", "zh_CN.UTF-8")]),
            Some(Lang::En)
        );
        // LC_ALL=C 等于"没说"，落到 LANG
        assert_eq!(env(&[("LC_ALL", "C"), ("LANG", "zh_TW")]), Some(Lang::Zh));
        // Git Bash 那套：只有 LC_CTYPE
        assert_eq!(env(&[("LC_CTYPE", "zh_CN.UTF-8")]), Some(Lang::Zh));
        // 一个都没给 → None，交棒给 OS 回退/英文
        assert_eq!(env(&[]), None);
    }
}