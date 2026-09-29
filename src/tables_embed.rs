// 本文件由 scripts/codegen.py 生成，请勿手改。
pub static MANIFEST_JSON: &str = include_str!("../tables/manifest.json");
pub static EMBEDDED_TABLE_FILES: &[(&str, &str)] = &[
    ("v10_2.json", include_str!("../tables/v10_2.json")),
    ("v11_3.json", include_str!("../tables/v11_3.json")),
    ("v12_4.json", include_str!("../tables/v12_4.json")),
    ("v13_6.json", include_str!("../tables/v13_6.json")),
    ("v6_2.json", include_str!("../tables/v6_2.json")),
    ("v6_8.json", include_str!("../tables/v6_8.json")),
    ("v7_8.json", include_str!("../tables/v7_8.json")),
    ("v8_4.json", include_str!("../tables/v8_4.json")),
    ("v9_4.json", include_str!("../tables/v9_4.json")),
];
