//! # bigseek
//!
//! `imba97-blog-vite` 站内搜索的独立检索引擎：**构建期用 native 产出紧凑二进制索引，运行期用 WebAssembly 查询**，
//! 两边共用同一份分词、同一份倒排语义。
//!
//! ## 为什么需要它
//!
//! 博客原文用的是 MiniSearch，它的默认分词器按空白 + 标点切词，中文「一对标点之间的整句话」会成为一个词条，
//! 导致只有落在句首的中文词能被检索到。实测 50 个真实技术词、597 个「词 × 文档」对的**文档级召回只有 39.7%**。
//! 本库用 CJK bigram 把它提到 98% 以上，同时把索引换成零反序列化的二进制格式。
//!
//! ## 两端用法
//!
//! 构建期（native CLI，读博客的 `search-full.json`）：
//!
//! ```bash
//! cargo run --release --features cli --bin build-index -- \
//!   --input ../imba97-blog-vite/public/search-full.json \
//!   --output ../imba97-blog-vite/public/search/search.idx
//! ```
//!
//! 运行期（wasm）：
//!
//! ```js
//! import init, { SearchIndex } from './bigseek.js'
//! await init(wasmBytes)
//! const index = new SearchIndex(idxBytes)
//! const hits = index.query('性能优化', null, null, 40)
//! ```
//!
//! ## 索引格式
//!
//! 见 [`format`] 模块文档。要点：
//!
//! - **零反序列化**：`Index::load` 只校验头部并解析分面表，词条与 postings 按需从字节切片读取，
//!   因此加载耗时与索引体积无关（本语料实测 < 1 ms）。
//! - **前端编码词典 + 稀疏块索引**：词典按字典序前端编码（省体积），每 16 个词条记录一个块起始点
//!   （保证随机访问），块内首词共享前缀置 0 以便从任意块直接开始顺序解码。
//! - **字段权重不建多份倒排**：只在文档级建倒排，`title`/`tags`/`categories` 的权重在查询期用
//!   原文 `contains` 现算。候选集只有几十个，成本可忽略，却省掉了 4 倍词典体积。

pub mod build;
pub mod format;
pub mod query;
pub mod tokenize;

// 直读文章（comrak / serde_yaml_ng / regex）挂在 `cli` 上，该 feature 在 default 里，
// 所以 `cargo build` / `cargo test` / 编辑器默认就能解析到它。
#[cfg(feature = "cli")]
pub mod posts;

// wasm 绑定挂在 `wasm` 上。**没有**放进 default（否则 wasm-pack 会连带编译 comrak 等依赖），
// 因此 rust-analyzer 需要显式启用——本仓库的 `.vscode/settings.json` 已经配好
// （rust-analyzer.cargo.features = ["cli", "wasm"]）。若在编辑器里看到本模块被 "configured out"，
// 那是 feature 未启用，不是编译错误。
#[cfg(feature = "wasm")]
pub mod wasm;

pub use build::{build_index, BuildStats, SearchDocument};
pub use query::{Index, SearchHit, SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX};
pub use tokenize::{is_cjk_like, normalize, tokenize};

/// 当前索引格式版本。格式不兼容变更时必须 +1（客户端据此回退）。
pub const FORMAT_VERSION: u32 = format::VERSION;

/// 便捷入口：加载索引字节流。
pub fn load_index(bytes: Vec<u8>) -> Result<Index, String> {
    Index::load(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_docs() -> Vec<SearchDocument> {
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

    #[test]
    fn index_roundtrip_finds_chinese_substrings() {
        let (bytes, stats) = build_index(&sample_docs());
        assert_eq!(stats.docs, 3);
        let index = load_index(bytes).expect("索引应能加载");

        // 关键回归：整词落在句子中间也必须能搜到（MiniSearch 默认分词做不到）
        let hits = index.query("性能", None, None, 10);
        assert_eq!(hits.len(), 2, "「性能」应命中 2 篇：{hits:#?}");
        assert_eq!(hits[0].path, "/post/1", "标题命中应排在正文命中之前");

        // 跨词边界的 bigram（能优）不该阻断 OR 回退
        let hits = index.query("性能优化", None, None, 10);
        assert!(!hits.is_empty(), "「性能优化」应通过 OR 回退拿到结果");

        // 英文与大小写
        let hits = index.query("vue", None, None, 10);
        assert_eq!(hits.len(), 2);

        // 单字 CJK 走前缀展开
        let hits = index.query("性", None, None, 10);
        assert_eq!(hits.len(), 2, "单字「性」应命中 2 篇");

        // 精确整串命中标记
        let hits = index.query("浏览器性能优化", None, None, 10);
        assert!(hits.iter().any(|h| h.exact), "整串命中应带 exact 标记");
    }

    #[test]
    fn facet_scope_filters() {
        let (bytes, _) = build_index(&sample_docs());
        let index = load_index(bytes).unwrap();

        let hits = index.query("vue", Some("Vue"), None, 10);
        assert_eq!(hits.len(), 2);
        let hits = index.query("性能", Some("Vue"), None, 10);
        assert_eq!(hits.len(), 1);

        // 只用范围、没有关键词：按文档顺序返回，摘要留空
        let hits = index.query("", Some("Vue"), None, 10);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.snippet.is_empty()));
        assert_eq!(hits[0].path, "/post/1");

        // 标签与分类同时限定
        let hits = index.query("", Some("Vue"), Some("前端"), 10);
        assert_eq!(hits.len(), 2);

        // 不存在的标签
        let hits = index.query("性能", Some("不存在"), None, 10);
        assert!(hits.is_empty());
    }

    #[test]
    fn snippet_wraps_hit_position() {
        let mut docs = sample_docs();
        docs[0].text = format!("{}关键词{}", "前".repeat(200), "后".repeat(200));
        let (bytes, _) = build_index(&docs);
        let index = load_index(bytes).unwrap();
        let hits = index.query("关键词", None, None, 10);
        let hit = hits.iter().find(|h| h.path == "/post/1").expect("应命中");
        assert!(hit.snippet.contains("关键词"));
        assert!(hit.snippet.starts_with('…'), "命中点前有内容时应加省略号");
        assert!(hit.snippet.ends_with('…'), "命中点后有内容时应加省略号");
    }

    #[test]
    fn rejects_bad_input() {
        assert!(load_index(Vec::new()).is_err());
        assert!(load_index(vec![0u8; 128]).is_err());
        let (mut bytes, _) = build_index(&sample_docs());
        bytes[4] = 99; // 破坏 version
        assert!(load_index(bytes).is_err());
    }

    /// 头部自洽但段内容恶意的索引：不能静默通过校验、更不能在分配时 abort
    #[test]
    fn rejects_malicious_headers() {
        let (bytes, _) = build_index(&sample_docs());

        // facets 计数 = u32::MAX：必须在 with_capacity 之前拒绝，
        // 否则一个约 100 字节的文件会申请几十 GB（wasm 里是 trap，JS 拿不到 Err）
        let facets_off = format::read_u32(&bytes, format::h::FACETS_OFF).unwrap() as usize;
        let mut broken = bytes.clone();
        broken[facets_off..facets_off + 5].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]);
        let err = match load_index(broken) {
            Ok(_) => panic!("恶意 facets 计数不应通过加载"),
            Err(err) => err,
        };
        assert!(
            err.contains("分面表计数越界"),
            "应指出计数越界，实际：{err}"
        );

        // block_count 拉满：`block_count * 16` 在 wasm32 上会回绕，必须在乘法处拦下
        let mut broken = bytes.clone();
        format::patch_u32(&mut broken, format::h::BLOCK_COUNT, u32::MAX);
        assert!(load_index(broken).is_err(), "blocks 段长度溢出应被拒绝");

        // postings 段越界（旧实现读了 postings_off 却从不校验这一段）
        let mut broken = bytes.clone();
        format::patch_u32(&mut broken, format::h::POSTINGS_OFF, u32::MAX);
        assert!(load_index(broken).is_err(), "postings 段越界应被拒绝");

        // doc_count 与文档表长度不符
        let mut broken = bytes.clone();
        format::patch_u32(&mut broken, format::h::DOC_COUNT, u32::MAX);
        assert!(load_index(broken).is_err(), "文档数异常应被拒绝");
    }

    #[test]
    fn varint_rejects_overlong_encoding() {
        // 5 字节编码但第 5 字节贡献了超过 4 位：旧实现直接左移丢高位，等于静默读错
        let mut pos = 0;
        assert_eq!(
            format::read_varint(&[0xFF, 0xFF, 0xFF, 0xFF, 0x7F], &mut pos),
            None
        );
        // 正常的 5 字节编码仍然可读
        let mut pos = 0;
        assert_eq!(
            format::read_varint(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F], &mut pos),
            Some(u32::MAX)
        );
    }
}
