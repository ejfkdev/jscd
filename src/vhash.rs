//! `Version::Hash()` 还原与爆破。
//!
//! V8 把 `hash_combine(major, minor, build, patch)` 写进 code cache 的 version_hash
//! 字段（@4）。combine 是 src/base/functional.h 的 MurmurHash 变体，单值预混合是
//! Thomas Wang 32 位整数哈希（`hash_value_unsigned`）。**折叠方向随 V8 版本变了**：
//!
//! - **right_fold**（V8 ≤ 11.x，变参递归）：`hash_combine(m, n, b, p)`
//!   = `hash_combine(hash_combine(hash_combine(hash_combine(0, p), b), n), m)`
//!   —— patch 最先入链，major 最后。
//! - **left_fold**（V8 ≥ 12.x，`base::Hasher` 左折叠）：seed=0 依次 Add(major, minor,
//!   build, patch) —— major 最先，patch 最后。
//!
//! 算法用真机产物验证（mise 安装的官方 Node 直接生成 .jsc 对拍头部字段）：
//! - right_fold：9.4.146.26(Node16.20.2)→0xacdd64ee、10.2.154.26(Node18.20.8)→0x3569a082、
//!   11.3.244.8(Node20.20.2)→0x00e4c20b
//! - left_fold：12.4.254.21(Node22.12.0)→0x79dafe74、13.6.233.17(Node24.12.0)→0xdc338cfa

const M: u64 = 0xC6A4_A793_5BD1_E995;

/// 折叠方向（版本表按 V8 版本选择）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fold {
    /// V8 ≤ 11.x：patch → build → minor → major
    RightFold,
    /// V8 ≥ 12.x：major → minor → build → patch
    LeftFold,
}

impl Fold {
    pub fn from_algorithm(algo: &str) -> Option<Fold> {
        match algo {
            "right_fold" => Some(Fold::RightFold),
            "left_fold" => Some(Fold::LeftFold),
            _ => None,
        }
    }
    fn order(self) -> [usize; 4] {
        match self {
            // 返回值即遍历顺序：(major, minor, build, patch) 的下标
            Fold::RightFold => [3, 2, 1, 0],
            Fold::LeftFold => [0, 1, 2, 3],
        }
    }
}

/// Thomas Wang 32 位整数混合（hash_value_unsigned，case 4）。
#[inline]
fn mix32(v: u32) -> u64 {
    let mut x = v;
    x = (!x).wrapping_add(x << 15);
    x ^= x >> 12;
    x = x.wrapping_add(x << 2);
    x ^= x >> 4;
    x = x.wrapping_mul(2057);
    x ^= x >> 16;
    x as u64 // 零扩展进 size_t
}

/// hash_combine(size_t seed, size_t value)（64 位主机分支）。
#[inline]
fn hash_combine(mut seed: u64, value: u64) -> u64 {
    let mut h = value.wrapping_mul(M);
    h ^= h >> 47;
    h = h.wrapping_mul(M);
    seed ^= h;
    seed.wrapping_mul(M)
}

/// `Version::Hash() = static_cast<uint32_t>(hash_combine(major, minor, build, patch))`。
pub fn version_hash(fold: Fold, major: u32, minor: u32, build: u32, patch: u32) -> u32 {
    let v = [major, minor, build, patch];
    let mut seed = 0u64;
    for &i in &fold.order() {
        seed = hash_combine(seed, mix32(v[i]));
    }
    seed as u32
}

/// 未知 version_hash 的爆破：离线枚举 V8 版本号四元组 × 两种折叠方向。
/// 范围按真实 V8 版本分布收窄（minor ≤ 24、patch ≤ 128：官方 V8 patch 最高 34），
/// 避免撞上 32 位哈希空间里的伪候选（如 3.47.427.201）。
pub fn brute_force(target: u32) -> Option<(Fold, (u32, u32, u32, u32))> {
    for fold in [Fold::RightFold, Fold::LeftFold] {
        for major in 1u32..30 {
            for minor in 0u32..25 {
                for build in 0u32..512 {
                    for patch in 0u32..128 {
                        if version_hash(fold, major, minor, build, patch) == target {
                            return Some((fold, (major, minor, build, patch)));
                        }
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_fold_matches_node_16_18_20() {
        assert_eq!(version_hash(Fold::RightFold, 9, 4, 146, 26), 0xacdd_64ee);
        assert_eq!(version_hash(Fold::RightFold, 10, 2, 154, 26), 0x3569_a082);
        assert_eq!(version_hash(Fold::RightFold, 11, 3, 244, 8), 0x00e4_c20b);
    }

    #[test]
    fn left_fold_matches_node_22_24() {
        assert_eq!(version_hash(Fold::LeftFold, 12, 4, 254, 21), 0x79da_fe74);
        assert_eq!(version_hash(Fold::LeftFold, 13, 6, 233, 17), 0xdc33_8cfa);
    }

    #[test]
    fn brute_finds_known_hashes_with_fold() {
        let (fold, v) = brute_force(version_hash(Fold::LeftFold, 12, 4, 254, 21)).unwrap();
        assert_eq!(fold, Fold::LeftFold);
        assert_eq!(v, (12, 4, 254, 21));
    }
}
