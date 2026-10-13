//! WebAssembly 绑定（`--features wasm`，由 `wasm-pack build --target web` 编译）。
//!
//! 边界设计：
//!
//! - 索引字节通过构造函数一次性传入（`wasm-bindgen` 会拷进 wasm 线性内存），之后查询零拷贝。
//! - 返回值手工用 `js_sys` 拼装，不引入 `serde-wasm-bindgen` / `serde`，避免为了几 KB 数据
//!   把 serde 的反射代码带进 wasm 体积。
//! - 所有可能失败的地方返回 `Result<_, JsValue>`，让 JS 侧能 `try/catch` 到具体错误。

use js_sys::{Array, Object, Reflect};
use wasm_bindgen::prelude::*;

use crate::query::Index;

/// 加载好的搜索索引。JS 侧用法：
///
/// ```js
/// const index = new SearchIndex(bytes)
/// index.query('性能', null, null, 40)   // → SearchHit[]
/// ```
#[wasm_bindgen]
pub struct SearchIndex {
    inner: Index,
}

#[wasm_bindgen]
impl SearchIndex {
    /// 用 `build-index` 产出的字节流构造索引。
    #[wasm_bindgen(constructor)]
    pub fn new(bytes: Vec<u8>) -> Result<SearchIndex, JsValue> {
        Index::load(bytes)
            .map(|inner| SearchIndex { inner })
            .map_err(|e| JsValue::from_str(&e))
    }

    /// 文档数。
    #[wasm_bindgen(js_name = docCount)]
    pub fn doc_count(&self) -> usize {
        self.inner.doc_count()
    }

    /// 词条数。
    #[wasm_bindgen(js_name = termCount)]
    pub fn term_count(&self) -> usize {
        self.inner.term_count()
    }

    /// 索引格式版本，用于与 JS 侧约定做兼容检查。
    #[wasm_bindgen(js_name = formatVersion)]
    pub fn format_version(&self) -> u32 {
        self.inner.format_version()
    }

    /// 分面数量（标签 / 分类），用于自检。
    #[wasm_bindgen(js_name = facetCounts)]
    pub fn facet_counts(&self) -> Array {
        let out = Array::new();
        out.push(&JsValue::from_f64(self.inner.tag_count() as f64));
        out.push(&JsValue::from_f64(self.inner.category_count() as f64));
        out
    }

    /// 查询。
    ///
    /// - `keywords`：已由 JS 侧原样传入，内部自行归一化
    /// - `tag` / `category`：范围限定，传 `null` / `undefined` 表示不限定
    /// - `limit`：返回条数上限，传 `null` / `undefined` 用默认值
    ///
    /// 返回 `SearchHit[]`，每条含 `path/title/date/tags/categories/snippet/score/exact`。
    pub fn query(
        &self,
        keywords: &str,
        tag: Option<String>,
        category: Option<String>,
        limit: Option<u32>,
    ) -> Result<Array, JsValue> {
        let hits = self.inner.query(
            keywords,
            tag.as_deref(),
            category.as_deref(),
            limit.unwrap_or(0) as usize,
        );
        let out = Array::new();
        for hit in hits {
            let obj = Object::new();
            set(&obj, "path", &JsValue::from_str(&hit.path))?;
            set(&obj, "title", &JsValue::from_str(&hit.title))?;
            set(&obj, "date", &JsValue::from_str(&hit.date))?;
            set(&obj, "snippet", &JsValue::from_str(&hit.snippet))?;
            set(&obj, "score", &JsValue::from_f64(f64::from(hit.score)))?;
            set(&obj, "exact", &JsValue::from_bool(hit.exact))?;
            set(&obj, "tags", &string_array(&hit.tags))?;
            set(&obj, "categories", &string_array(&hit.categories))?;
            out.push(&obj);
        }
        Ok(out)
    }
}

fn set(target: &Object, key: &str, value: &JsValue) -> Result<(), JsValue> {
    Reflect::set(target, &JsValue::from_str(key), value).map(|_| ())
}

fn string_array(items: &[String]) -> Array {
    let out = Array::new();
    for item in items {
        out.push(&JsValue::from_str(item));
    }
    out
}
