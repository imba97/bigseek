//! 发布规则向量回归：与博客 TS 侧（`post-policy.ts` + `@11ty/gray-matter`）共用
//! `testdata/post-policy-vectors.json`。
//!
//! 博客侧的 `pnpm run search:check-parity` 会读同一份向量校验 TS 实现，
//! 两侧任何一方擅自改动「哪些文章可发布 / id 怎么算 / 标签怎么归一化」都会在这里或那边红灯。

use blog_search::posts::{evaluate_frontmatter, RejectReason};

#[derive(serde::Deserialize)]
struct Vectors {
    version: u32,
    cases: Vec<Case>,
}

#[derive(serde::Deserialize)]
struct Case {
    name: String,
    frontmatter: String,
    expected: Expected,
}

#[derive(serde::Deserialize)]
struct Expected {
    publishable: bool,
    #[serde(default)]
    reject_reason: Option<String>,
    #[serde(default)]
    id: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    date: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    categories: Vec<String>,
}

fn reason_name(reason: RejectReason) -> &'static str {
    match reason {
        RejectReason::Draft => "Draft",
        RejectReason::NoDate => "NoDate",
        RejectReason::BadId => "BadId",
    }
}

#[test]
fn post_policy_vectors_match_implementation() {
    let raw = include_str!("../testdata/post-policy-vectors.json");
    let vectors: Vectors = serde_json::from_str(raw).expect("向量文件解析失败");
    assert_eq!(vectors.version, 1, "向量文件版本不匹配");

    let mut failures = Vec::new();
    for case in &vectors.cases {
        let outcome = match evaluate_frontmatter(&case.frontmatter) {
            Ok(outcome) => outcome,
            Err(err) => {
                failures.push(format!("[{}] 求值失败：{err}", case.name));
                continue;
            }
        };

        if outcome.publishable != case.expected.publishable {
            failures.push(format!(
                "[{}] publishable：期望 {} 实际 {}",
                case.name, case.expected.publishable, outcome.publishable
            ));
            continue;
        }

        if !case.expected.publishable {
            // 拒绝原因只在本侧断言（TS 侧没有这个概念）
            if let Some(expected_reason) = &case.expected.reject_reason {
                let actual = outcome.reject_reason.map(reason_name).unwrap_or("<none>");
                if actual != expected_reason {
                    failures.push(format!(
                        "[{}] reject_reason：期望 {expected_reason} 实际 {actual}",
                        case.name
                    ));
                }
            }
            continue;
        }

        let checks: [(&str, String, String); 4] = [
            ("id", case.expected.id.clone(), outcome.id.clone()),
            ("path", case.expected.path.clone(), outcome.path.clone()),
            ("title", case.expected.title.clone(), outcome.title.clone()),
            ("date", case.expected.date.clone(), outcome.date.clone()),
        ];
        for (field, expected, actual) in checks {
            if expected != actual {
                failures.push(format!(
                    "[{}] {field}：期望 {expected:?} 实际 {actual:?}",
                    case.name
                ));
            }
        }
        if case.expected.tags != outcome.tags {
            failures.push(format!(
                "[{}] tags：期望 {:?} 实际 {:?}",
                case.name, case.expected.tags, outcome.tags
            ));
        }
        if case.expected.categories != outcome.categories {
            failures.push(format!(
                "[{}] categories：期望 {:?} 实际 {:?}",
                case.name, case.expected.categories, outcome.categories
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
