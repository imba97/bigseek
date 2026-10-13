#![cfg(not(target_arch = "wasm32"))]

//! 真实语料回归（opt-in）：直接用博客产出的 `search-full.json` + `search.idx` 跑召回率。
//!
//! 通过环境变量启用，缺省时自动跳过（普通 `cargo test` 不依赖博客仓库）：
//!
//! ```bash
//! BLOG_SEARCH_DOCS=../imba97-blog-vite/public/search-full.json \
//! BLOG_SEARCH_INDEX=../imba97-blog-vite/public/search/search.idx \
//! cargo test --test real_corpus -- --nocapture
//! ```
//!
//! 度量口径与 `ai-docs/design/site-search.md` 完全一致：对每个真实中文词，
//! 统计「语料里确实含该词的文档」中有多少被检索到（文档级召回）。

use bigseek::tokenize::normalize;
use bigseek::{load_index, SEARCH_LIMIT_MAX};

#[derive(serde::Deserialize)]
struct Doc {
    path: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    text: String,
}

/// 这个博客里真会被搜到的中文技术词
const TERMS: &[&str] = &[
    "性能",
    "优化",
    "浏览器",
    "重写",
    "打包",
    "缓存",
    "组件",
    "接口",
    "封装",
    "渲染",
    "部署",
    "重构",
    "内存",
    "并发",
    "索引",
    "动画",
    "图片",
    "压缩",
    "构建",
    "依赖",
    "模板",
    "响应式",
    "异步",
    "线程",
    "服务端",
    "客户端",
    "插件",
    "主题",
    "配置",
    "权限",
    "调试",
    "测试",
    "类型",
    "语法",
    "函数",
    "变量",
    "数组",
    "对象",
    "闭包",
    "原型",
    "事件",
    "循环",
    "网络",
    "请求",
    "响应",
    "框架",
    "数据库",
    "容器",
    "代理",
    "协议",
];

fn env_path(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

#[test]
fn real_corpus_recall() {
    let (Some(docs_path), Some(index_path)) =
        (env_path("BLOG_SEARCH_DOCS"), env_path("BLOG_SEARCH_INDEX"))
    else {
        eprintln!("跳过：未设置 BLOG_SEARCH_DOCS / BLOG_SEARCH_INDEX");
        return;
    };

    let raw = std::fs::read_to_string(&docs_path).expect("读取 search-full.json 失败");
    let docs: Vec<Doc> = serde_json::from_str(&raw).expect("解析 search-full.json 失败");
    let bytes = std::fs::read(&index_path).expect("读取 search.idx 失败");
    let index = load_index(bytes).expect("加载索引失败");

    assert_eq!(index.doc_count(), docs.len(), "索引文档数与 JSON 不一致");

    let normalized: Vec<String> = docs
        .iter()
        .map(|d| format!("{}\n{}\n{}", normalize(&d.title), normalize(&d.text), ""))
        .collect();

    let mut total_expected = 0usize;
    let mut total_found = 0usize;
    let mut perfect = 0usize;
    let mut tested = 0usize;
    let mut failures: Vec<String> = Vec::new();

    println!("\n{:<10} {:>6} {:>6}", "词", "应命中", "实命中");
    for term in TERMS {
        let needle = normalize(term);
        let expected: Vec<usize> = normalized
            .iter()
            .enumerate()
            .filter(|(_, text)| text.contains(&needle))
            .map(|(i, _)| i)
            .collect();
        if expected.is_empty() {
            continue;
        }
        tested += 1;
        let hits = index.query(term, None, None, SEARCH_LIMIT_MAX);
        let found = expected
            .iter()
            .filter(|&&i| hits.iter().any(|h| h.path == docs[i].path))
            .count();
        total_expected += expected.len();
        total_found += found;
        if found == expected.len() {
            perfect += 1;
        } else {
            failures.push(format!("{term}({found}/{})", expected.len()));
        }
        println!("{:<10} {:>6} {:>6}", term, expected.len(), found);
    }

    let recall = total_found as f64 / total_expected as f64 * 100.0;
    println!(
        "\n文档级召回：{total_found}/{total_expected} = {recall:.1}%    完全命中的词 {perfect}/{tested}"
    );
    if !failures.is_empty() {
        println!("未完全命中：{}", failures.join(" "));
    }

    // 这两条是 MiniSearch 默认分词下必然为 0 的（整词落在句子中间）
    for term in ["性能", "重写"] {
        let hits = index.query(term, None, None, SEARCH_LIMIT_MAX);
        assert!(
            !hits.is_empty(),
            "「{term}」在旧引擎下 0 结果，新引擎必须能搜到"
        );
    }

    assert!(recall >= 95.0, "文档级召回 {recall:.1}% 低于 95% 门槛");
}
