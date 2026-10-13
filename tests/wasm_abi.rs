//! wasm 侧 ABI 契约测试（纯 Rust 编写，`wasm-pack test --node` 执行）。
//!
//! 为什么必须有这一层：`src/wasm.rs` 里的 `#[wasm_bindgen]` 边界在 **native target 上执行不到**
//! （wasm-bindgen 生成的是会 panic 的桩，真实逻辑在编译产物里）。所以「`query()` 返回的是不是 JS 数组」
//! 「对象的 key 是不是约定的那几个」「非法字节流是不是抛可捕获的 JS 异常」这类问题
//! 只能在这个测试里问——它跑在 wasm32 上，能直接操作 `js_sys` 对象。
//!
//! 运行：
//!
//! ```bash
//! wasm-pack test --node --no-default-features --features wasm
//! ```
//!
//! 注意 `#![cfg(target_arch = "wasm32")]`：native `cargo test` 会把这个文件整体编译为空，
//! 因此它不会影响原生测试链路。

#![cfg(target_arch = "wasm32")]

use bigseek::build::{build_index, SearchDocument};
use bigseek::wasm::SearchIndex;
use js_sys::{Array, Object, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_test::wasm_bindgen_test;

/// 与 `tests/fixtures/search-full.json` 内容一致的三篇文档
fn fixture_docs() -> Vec<SearchDocument> {
    vec![
        SearchDocument {
            path: "/post/1".into(),
            title: "性能优化实践".into(),
            date: "2026-01-01".into(),
            tags: vec!["性能".into(), "Vue".into()],
            categories: vec!["前端".into()],
            text: "这篇文章讲浏览器性能优化，包括打包体积与缓存策略。".into(),
        },
        SearchDocument {
            path: "/post/2".into(),
            title: "Rust 重写搜索".into(),
            date: "2026-02-01".into(),
            tags: vec!["Rust".into()],
            categories: vec!["前端".into()],
            text: "用 Rust 写一个 wasm 搜索库，替换 minisearch。性能提升主要来自分词。".into(),
        },
        SearchDocument {
            path: "/post/3".into(),
            title: "Vue3 组件通信".into(),
            date: "2025-12-01".into(),
            tags: vec!["Vue".into()],
            categories: vec!["前端".into()],
            text: "props emit provide inject 四种方式。".into(),
        },
    ]
}

fn fixture_index() -> SearchIndex {
    let (bytes, _) = build_index(&fixture_docs());
    SearchIndex::new(bytes).expect("索引应能构造")
}

fn query_js(
    index: &SearchIndex,
    keywords: &str,
    tag: Option<&str>,
    category: Option<&str>,
    limit: u32,
) -> Array {
    index
        .query(
            keywords,
            tag.map(str::to_string),
            category.map(str::to_string),
            Some(limit),
        )
        .expect("query 不该失败")
}

/// 把 JS 对象上的字段读成字符串；字段不存在或不是字符串都会失败
fn field_str(item: &JsValue, key: &str) -> String {
    Reflect::get(item, &JsValue::from_str(key))
        .unwrap_or_else(|_| panic!("读取字段 {key} 失败"))
        .as_string()
        .unwrap_or_else(|| panic!("字段 {key} 不是字符串"))
}

fn field_array(item: &JsValue, key: &str) -> Array {
    let value = Reflect::get(item, &JsValue::from_str(key))
        .unwrap_or_else(|_| panic!("读取字段 {key} 失败"));
    if !Array::is_array(&value) {
        panic!("字段 {key} 不是 JS 数组");
    }
    Array::from(&value)
}

fn field_f64(item: &JsValue, key: &str) -> f64 {
    Reflect::get(item, &JsValue::from_str(key))
        .unwrap_or_else(|_| panic!("读取字段 {key} 失败"))
        .as_f64()
        .unwrap_or_else(|| panic!("字段 {key} 不是数字"))
}

fn path_of(item: &JsValue) -> String {
    field_str(item, "path")
}

#[wasm_bindgen_test]
fn exposes_documented_meta_api() {
    let index = fixture_index();
    assert_eq!(index.format_version(), 1, "格式版本应为 1");
    assert_eq!(index.doc_count(), 3, "文档数应为 3");
    assert!(index.term_count() > 0, "词条数应大于 0");

    let facets = index.facet_counts();
    assert_eq!(facets.length(), 2, "facetCounts 应返回 [标签数, 分类数]");
    assert_eq!(facets.get(0).as_f64(), Some(3.0), "标签应为 3 个");
    assert_eq!(facets.get(1).as_f64(), Some(1.0), "分类应为 1 个");
}

#[wasm_bindgen_test]
fn query_returns_js_objects_with_documented_fields() {
    let index = fixture_index();
    let hits = query_js(&index, "性能", None, None, 10);

    assert!(Array::is_array(&hits), "query 必须返回 JS 数组");
    assert_eq!(hits.length(), 2, "「性能」应命中 2 篇");

    let first = hits.get(0);
    let object: Object = first.clone().dyn_into().expect("每个命中项必须是 JS 对象");
    let keys = Object::keys(&object);
    let mut names: Vec<String> = (0..keys.length())
        .filter_map(|i| keys.get(i).as_string())
        .collect();
    names.sort();

    // 这是博客 Worker 依赖的字段契约，少一个就会静默显示空白
    assert_eq!(
        names,
        [
            "categories",
            "date",
            "exact",
            "path",
            "score",
            "snippet",
            "tags",
            "title"
        ],
        "命中项字段集合与约定不一致"
    );

    assert_eq!(path_of(&first), "/post/1", "标题命中应排在正文命中之前");
    assert_eq!(field_str(&first, "title"), "性能优化实践");
    assert!(
        field_str(&first, "snippet").contains("性能"),
        "摘要应含关键词"
    );

    let tags = field_array(&first, "tags");
    let tag_names: Vec<String> = (0..tags.length())
        .filter_map(|i| tags.get(i).as_string())
        .collect();
    assert_eq!(tag_names, ["性能", "Vue"], "tags 必须是 JS 字符串数组");
    let categories = field_array(&first, "categories");
    assert_eq!(categories.length(), 1);
    assert!(field_f64(&first, "score") > 0.0, "score 必须是数字");
}

#[wasm_bindgen_test]
fn query_semantics_survive_the_wasm_boundary() {
    let index = fixture_index();

    // 大小写不敏感
    assert_eq!(query_js(&index, "rust", None, None, 10).length(), 1);
    // 单字 CJK 走词典前缀展开
    assert_eq!(query_js(&index, "性", None, None, 10).length(), 2);
    // 分面限定
    assert_eq!(query_js(&index, "性能", Some("Vue"), None, 10).length(), 1);
    // 只用范围、无关键词：摘要留空
    let scoped = query_js(&index, "", Some("Vue"), None, 10);
    assert_eq!(scoped.length(), 2);
    for i in 0..scoped.length() {
        assert_eq!(
            field_str(&scoped.get(i), "snippet"),
            "",
            "纯范围浏览不该带摘要"
        );
    }
    // 无命中返回空数组（而不是 undefined / null）
    let miss = query_js(&index, "不存在的词xyz", None, None, 10);
    assert!(Array::is_array(&miss));
    assert_eq!(miss.length(), 0);
    // limit 生效
    assert!(query_js(&index, "前端", None, None, 1).length() <= 1);
}

#[wasm_bindgen_test]
fn constructor_rejects_invalid_bytes_instead_of_panicking() {
    // 非法字节流必须返回可捕获的 JS 错误，而不是 panic 掉整个 worker
    let result = SearchIndex::new(vec![1, 2, 3, 4]);
    assert!(result.is_err(), "非法索引应当报错");
    let message = result.err().and_then(|e| e.as_string()).unwrap_or_default();
    assert!(!message.is_empty(), "错误信息不该为空");
}
