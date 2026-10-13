# blog-search

`imba97-blog-vite` 站内搜索的独立检索引擎：**构建期用 native CLI 产出紧凑二进制索引，运行期用 WebAssembly 查询**，
两端共用同一份分词实现与同一份倒排语义。

本仓库是 `https://github.com/imba97/imba97-blog-vite` 的搜索依赖，由博客的构建流程拉取、编译，
把产物放进 `public/search/`（wasm + glue + 索引）后随站点一起部署。

---

## 为什么需要它

博客原先用 [MiniSearch](https://github.com/lucaong/minisearch)，它的默认分词器按空白 + 标点切词，
中文「一对标点之间的整句话」会成为一个词条。实测（50 个真实中文技术词、597 个「词 × 文档」对）：

| 引擎 | 文档级召回 | 完全命中的词 | 冷启动建索引 |
| --- | --- | --- | --- |
| MiniSearch 默认分词 | **39.7%**（237/597） | 0/48 | 79 ms |
| MiniSearch + CJK bigram | 98.3%（587/597） | 47/48 | 300 ms |
| **本库（bigram + OR 回退 + 原文校验）** | **100.0%**（597/597） | 48/48 | **1.2 ms**（零反序列化加载） |

失败模式很具体：只有落在句子开头的词能被搜到，像 `性能`（4 篇）、`重写`（1 篇）这种整词落在句中的查询直接 0 结果。
标题、标签、英文因为不会跨标点，所以「看起来能用」——这也是它长期没被发现的原因。

实测数据完整版见博客仓库的 [`ai-docs/design/site-search.md`](https://github.com/imba97/imba97-blog-vite/blob/main/ai-docs/design/site-search.md)。

## 设计要点

1. **一份实现，两处运行**。分词、打分、索引语义都在这个 crate 里：native CLI 建索引，wasm 查索引。
   不存在「构建期一套、运行期一套」的漂移（旧方案里这是最容易出错的点）。
2. **中英分词**：CJK / 假名 / 谚文走 **bigram**（`性能优化` → `性能`/`能优`/`优化`），拉丁按 `is_alphanumeric` 切。
   单字符 CJK 查询走词典前缀展开，所以 `性` 也能搜到。
3. **两段式检索**：先 AND（所有 token 都命中），为空再退化为 OR 并按命中 token 数排序。
   这一步专门解决 bigram 的跨词边界问题——`性能优化` 会生成不存在的 `能优`，纯 AND 会误伤。
4. **候选 + 原文子串校验**：候选集只有几十个，逐个用原文 `contains` 验证，拿到「精确子串」语义，
   既不需要中文分词词典，也不会因 bigram 碎片而误报。
5. **字段权重不建多份倒排**：只在文档级建倒排，`title`/`tags`/`categories` 的权重在查询期现算。
   代价是几十次 `contains`，收益是词典体积不 ×4。
6. **零反序列化**：加载时只校验头部、解析分面表（标签/分类），词条与 postings 按需从字节切片读。
   654 KiB / 144 篇的索引加载 **1.2 ms**。

## 索引格式（v1）

```
header 80 B   magic "BSSI" | version | flags | doc_count | term_count | block_count
              terms/blocks/postings/docs/pool/facets 各自的 off/len
terms         按字典序前端编码的词条记录，每 16 条为一块
              varint prefix_len | varint suffix_len | suffix | varint postings_len | varint doc_freq
blocks        稀疏块索引（16 B/块）：first_term_off | term_count | postings_off | reserved
              块内首词共享前缀置 0 → 可从任意块直接开始顺序解码
postings      doc id 升序 delta + varint，按词条顺序连续存放
docs          定长 48 B：path/title/date/text/tags/cats 的 (off, len)
pool          所有原始字符串（tags/cats 用 U+001F 连接），相同串去重
facets        varint 计数 + (名称, doc id 列表)
```

本语料实测体积：词典 180 KiB + postings 54 KiB + 文档表 7 KiB + 字符串池 388 KiB + 分面 3 KiB = **654 KiB**。

> ⚠️ **不要用「减小下载体积」当立项理由。** 654 KiB 比原来的 `search-full.json`（401 KiB）更大，
> gzip 后也基本持平。本库的收益是**冷启动**（300 ms → 1.2 ms）、**内存**（JS 堆 14.5 MiB → wasm 线性内存）
> 和**召回质量**，不是流量。

## 用法

### 构建期（native CLI）

```bash
# 输入是博客 TS 侧产出的 search-full.json（文档抽取的唯一真源仍在博客仓库）
cargo run --release --features cli --bin build-index -- \
  --input ../imba97-blog-vite/public/search-full.json \
  --output ../imba97-blog-vite/public/search/search.idx \
  --verify
```

`--verify` 会在写完索引后立刻加载它，自动从语料抽样 ≥40 个中文词（取跨文档重复出现的 2 字 CJK 串，
真实词会复现、跨词边界的碎片一般不会），做**文档级召回**自检，低于 95% 直接非 0 退出。
这样每次博客构建都自带质量门禁，语料更新后依然有效。

### 运行期（wasm）

```bash
wasm-pack build --target web --release --out-dir pkg --features wasm
```

```js
import init, { SearchIndex } from './blog_search.js'

await init({ module_or_path: wasmBytes })   // 显式传字节，绕开 MIME / 路径问题
const index = new SearchIndex(indexBytes)

index.query('性能优化', null, null, 40)
// → [{ path, title, date, tags, categories, snippet, score, exact }, ...]
//    tag / category 传 null 表示不限范围；limit 传 null 用默认 40
```

产物（本语料）：`blog_search_bg.wasm` **75.4 KiB**、glue JS 12.4 KiB。

> 是否带 `wasm` feature 是编译期开关：`cli` 与 `wasm` 互不依赖，wasm 产物里不会链接 `serde_json`。

## 测试

```bash
cargo fmt --check
cargo clippy --all-targets --features cli -- -D warnings
cargo test --features cli

# wasm 冒烟（真实 JS 运行时跑一遍查询）
wasm-pack build --target web --release --out-dir pkg --features wasm
cargo run --release --features cli --bin build-index -- \
  --input tests/fixtures/search-full.json --output target/smoke.idx
node scripts/smoke-wasm.mjs target/smoke.idx

# 性能基准
node scripts/bench-wasm.mjs target/search.idx

# 真实语料召回回归（opt-in，需要博客仓库的产物）
BLOG_SEARCH_DOCS=../imba97-blog-vite/public/search-full.json \
BLOG_SEARCH_INDEX=../imba97-blog-vite/public/search/search.idx \
cargo test --release --test real_corpus -- --nocapture
```

## 分词一致性契约

`testdata/tokenize-vectors.json` 是**跨语言**的分词向量，Rust 侧由 `tests/tokenize_vectors.rs` 校验，
博客 TS 侧由 `pnpm run search:check-parity` 校验同一份文件。任何一侧擅自改动分词语义都会红灯。

归一化刻意只做**逐字符 1:1 替换**（全角 ASCII → 半角、`U+3000` → 空格、ASCII 大写 → 小写），不做完整 NFKC：
一是省掉 unicode-normalization 的表体积，二是「归一化文本的字符下标 == 原文的字符下标」，
摘要（snippet）可以直接把命中位置映射回原文，不需要额外位置表。若日后要支持繁体/组合字符，
在 `normalize` 里补映射表并把 `FORMAT_VERSION` +1 即可。

## 目录

```
src/tokenize.rs        归一化 + bigram 分词（两端共用）
src/format.rs          格式常量、varint、头部字段偏移
src/build.rs           索引构建（BTreeMap 倒排 → 前端编码词典 → 各段拼装）
src/query.rs           索引加载 + 检索（块二分 → 候选 → 校验 → 打分 → 摘要）
src/wasm.rs            wasm-bindgen 边界（手工拼 js_sys 对象，不引 serde）
src/bin/build-index.rs 构建期 CLI + 召回自检
tests/                 分词向量、往返查询、真实语料召回（opt-in）
scripts/               冒烟与基准（仅依赖 Node 内置能力）
```

## 已知取舍

- **不做词干化 / 同义词 / 拼音**：当前语料不需要。要加的话在 `tokenize` 里扩，同样受向量测试约束。
- **词典 180 KiB 是主要体积开销**：24154 个 bigram 词条的必然结果。若日后要压，
  可以只对 `body` 用 bigram、`title`/`tags` 保持整词，或改用真正的 FST（预计再省 20~35%）。
- **查询比 MiniSearch 慢一点**（0.06~2 ms vs 0.3 ms）：代价来自 OR 回退 + 原文校验，换来的是召回与精确性。
  相对一次按键的帧预算，这个量级没有观感差异。
- **单进程内不并发**：`Index` 只读、无内部可变状态，`Send + Sync` 天然满足；wasm 侧按单线程使用。

## License

MIT
