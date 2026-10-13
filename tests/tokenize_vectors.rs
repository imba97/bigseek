//! 分词向量回归：与博客 TS 侧实现共用 `testdata/tokenize-vectors.json`。
//!
//! 博客侧的 `pnpm run search:check-parity` 会读同一份向量校验 TS 实现，
//! 两边任何一方擅自改动分词语义都会在这里或那边红灯。

use bigseek::tokenize::{normalize, tokenize};

#[derive(serde::Deserialize)]
struct Vectors {
    version: u32,
    cases: Vec<Case>,
}

#[derive(serde::Deserialize)]
struct Case {
    name: String,
    input: String,
    normalized: String,
    tokens: Vec<String>,
}

#[test]
fn tokenize_vectors_match_implementation() {
    let raw = include_str!("../testdata/tokenize-vectors.json");
    let vectors: Vectors = serde_json::from_str(raw).expect("向量文件解析失败");
    assert_eq!(vectors.version, 1, "向量文件版本不匹配");

    let mut failures = Vec::new();
    for case in &vectors.cases {
        let normalized = normalize(&case.input);
        if normalized != case.normalized {
            failures.push(format!(
                "[{}] 归一化不一致\n  输入: {:?}\n  期望: {:?}\n  实际: {:?}",
                case.name, case.input, case.normalized, normalized
            ));
            continue;
        }
        let tokens = tokenize(&normalized);
        let expected: Vec<&str> = case.tokens.iter().map(String::as_str).collect();
        if tokens != expected {
            failures.push(format!(
                "[{}] 分词不一致\n  输入: {:?}\n  期望: {:?}\n  实际: {:?}",
                case.name, case.input, case.tokens, tokens
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} 个向量用例失败：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn normalize_preserves_char_count() {
    // snippet 依赖「归一化不增删字符」这一性质，这里守住它
    let samples = [
        "性能优化 Vue3 ｖｕｅ３　全角空格 İ ẞ 中文",
        "emoji 🚀 也算一个 char",
        "combining é vs é",
    ];
    for sample in samples {
        assert_eq!(
            normalize(sample).chars().count(),
            sample.chars().count(),
            "归一化改变了字符数：{sample:?}"
        );
    }
}
