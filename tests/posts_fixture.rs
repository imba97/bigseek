#![cfg(not(target_arch = "wasm32"))]

//! 直读文章目录的自包含回归（不依赖博客仓库）。
//!
//! fixture 里的 5 个文件覆盖：根 index 排除、日期排序、draft 过滤、id 非数字过滤、
//! 表格与删除线扩展、原始 HTML 与实体保留。

use blog_search::posts::{load_posts, parse_post_date_timestamp};
use blog_search::tokenize::normalize;

#[test]
fn loads_fixture_posts_with_policy_and_order() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/posts");
    let (docs, report) = load_posts(dir).expect("直读 fixture 失败");

    assert_eq!(report.files, 4, "根 index.md 应被排除，只剩 4 篇待判定");
    assert_eq!(report.published, 2, "只有 2 篇可发布");
    assert_eq!(report.skipped_draft, 1);
    assert_eq!(report.skipped_bad_id, 1);
    // 根 index 里的标题必须不出现在结果中
    assert!(
        docs.iter().all(|d| d.title != "不该被索引"),
        "posts/index.md 不该被索引"
    );

    // 日期降序：alpha(2026-03) 在前，beta(2025-01) 在后
    let paths: Vec<&str> = docs.iter().map(|d| d.path.as_str()).collect();
    assert_eq!(paths, ["/post/101", "/post/102"]);

    let alpha = &docs[0];
    assert_eq!(alpha.title, "较新的文章");
    assert_eq!(alpha.date, "2026-03-01 08:00:00");
    assert_eq!(alpha.tags, ["Rust", "搜索"]);
    assert_eq!(alpha.categories, ["前端"]);

    // 表格扩展：markdown-it 默认 preset 支持表格，单元格文字应保留（管道符被渲染掉）
    assert!(
        alpha.text.contains("bigram"),
        "正文含 bigram：{}",
        alpha.text
    );
    assert!(alpha.text.contains("98%"));
    assert!(
        !alpha.text.contains('|'),
        "表格管道符不该出现在纯文本里：{}",
        alpha.text
    );
    // 删除线扩展：~~x~~ 渲染成 <del>，纯文本里不该留波浪号
    assert!(
        !alpha.text.contains("~~"),
        "删除线标记不该出现：{}",
        alpha.text
    );
    assert!(alpha.text.contains("旧的实现"));

    // 原始 HTML 标签被剥掉，但实体保留（与 TS 侧 stripHtml 一致）
    let beta = &docs[1];
    assert!(beta.text.contains("原始 HTML"), "{}", beta.text);
    assert!(!beta.text.contains('<'));
    assert!(beta.text.contains("&amp;"), "实体应原样保留：{}", beta.text);
    assert_eq!(beta.tags, ["单标签"], "标量标签应包成单元素数组");
    assert!(beta.categories.is_empty());
}

#[test]
fn date_timestamp_matches_shanghai_interpretation() {
    // 2026-03-01 08:00:00 (+08:00) == 2026-03-01 00:00:00 UTC
    assert_eq!(
        parse_post_date_timestamp("2026-03-01 08:00:00"),
        Some(1_772_323_200_000)
    );
    assert_eq!(
        parse_post_date_timestamp("2026-03-01"),
        Some(1_772_294_400_000)
    );
    assert_eq!(
        parse_post_date_timestamp("2024-02-29"),
        parse_post_date_timestamp("2024-02-29 00:00:00")
    );
    // 畸形日期：拒绝而不是 panic
    assert_eq!(parse_post_date_timestamp("2026-13-01"), None);
    assert_eq!(parse_post_date_timestamp("2023-02-29"), None);
    assert_eq!(parse_post_date_timestamp(""), None);
    assert_eq!(parse_post_date_timestamp("2026-03-01T08:00:00Z"), None);
}

#[test]
fn extracted_text_is_searchable_by_bigram() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/posts");
    let (docs, _) = load_posts(dir).expect("直读 fixture 失败");

    // 索引是可检索的：整词落在句中的中文也要能命中（这正是当初的动机）
    let (bytes, _) = blog_search::build_index(&docs);
    let index = blog_search::load_index(bytes).expect("索引加载失败");
    let hits = index.query("性能", None, None, 10);
    assert_eq!(hits.len(), 1, "「性能」应命中 alpha：{hits:#?}");
    assert_eq!(hits[0].path, "/post/101");
    assert!(normalize(&hits[0].snippet).contains("性能"));
}
