# 在这个仓库里工作

这里的每一条都是踩过之后写下来的：先犯错、找到证据、再修。没有核实过的结论不写进来。
改任何东西之前先读这一页。

## 这是什么

一个二进制索引里的 bigram 全文检索：构建期用 native CLI 产出紧凑索引，运行期用 WebAssembly
查询，两端共用同一份分词与倒排语义。产品文档在 [`README.md`](./README.md) /
[`README_CN.md`](./README_CN.md)，索引格式见 `src/format.rs` 的模块注释。

| 目录 | 职责 |
| --- | --- |
| `src/tokenize.rs` | 归一化 + bigram 分词（构建期与查询期共用，返回原文借用切片） |
| `src/format.rs` | 索引格式常量、varint、头部与文档记录字段偏移 |
| `src/build.rs` | 文档 → 倒排 → 词条 / 块索引 / 倒排 / 文档表 / 字符串池 / 分面 |
| `src/query.rs` | 加载（只校验头 + 解析分面表）与检索（块二分 → 位图 → 打分 → 摘要） |
| `src/posts.rs` | 直读文章：frontmatter/YAML → 发布规则 → markdown 转纯文本（仅 `cli`） |
| `src/wasm.rs` | wasm-bindgen 边界（手工拼 `js_sys` 对象，不引 serde） |
| `src/bin/build-index.rs` | 构建期 CLI + `--verify` 召回自检 |
| `testdata/` | 跨语言契约向量（分词、发布规则）：Rust 与 JS 两侧都按它测试 |
| `tests/` | 向量回归、fixture 直读、真实语料对拍、wasm ABI 契约 |
| `.github/workflows/` | `ci.yaml`（可复用的 checks）+ `release.yaml`（tag → checks → 发布 → 资产） |

两个 feature 互不依赖：`cli`（默认 feature，为了让 `cargo test` / `cargo run` / 编辑器开箱可用）与
`wasm`。**构建 wasm 必须 `--no-default-features --features wasm`**，否则 comrak/regex 会被编进去
（不会坏，LTO 会消掉未引用代码，只是构建变慢）。编辑器报 `unresolved import bigseek::posts` 是 feature
没开，不是编译错误——`.vscode/settings.json` 已经配了 `["cli", "wasm"]`。

## 一次任务做完，跑 CI 同款检查

**一次任务的所有 commit 完成后（推送前）**，把下面这七条跑一遍。`ci.yaml` 里就是它们；本地不跑，
等于把红灯留给 CI——本仓库已经这样红过一次。

| # | 命令 | 对应 `ci.yaml` |
| --- | --- | --- |
| 1 | `cargo fmt --all --check` | `test` / Check formatting |
| 2 | `cargo clippy --all-targets --all-features --locked -- -D warnings` | `test` / Lint with clippy |
| 3 | `cargo test --locked` | `test` / Run tests |
| 4 | `cargo run --release --locked --bin build-index -- --posts-dir tests/fixtures/posts --output target/smoke-posts.idx --verify` | `wasm` / Build a smoke index from markdown posts |
| 5 | `cargo run --release --locked --bin build-index -- --input tests/fixtures/search-full.json --output target/smoke.idx --verify` | `wasm` / Build a smoke index from JSON |
| 6 | `wasm-pack test --node --no-default-features --features wasm` | `wasm` / Test the wasm ABI contract |
| 7 | `wasm-pack build --target web --release --out-dir pkg --no-pack --no-default-features --features wasm` | `wasm` / Build the wasm package |

为什么是这七条，而不是「`cargo test` 绿了就行」：

- **1 和 2 最容易漏**，而它们与 3 一样是门禁：`cargo test` 通过不代表 `fmt` 与 `-D warnings` 的 clippy 通过。
- **4 和 5 是唯一覆盖 `verify_recall` 的地方**。它在 `src/bin/` 里，属于二进制目标——`cargo test` 不会
  走 `--verify` 那条路径。fixture 语料只有 4 篇、其中 2 篇发布，恰好是「直方图计数少算」这类 bug 的最佳
  探针：真实语料（144 篇）会把它掩盖掉，小语料会直接报「抽样为空」。
- **6 和 7 覆盖 native 上执行不到的 ABI 契约**（wasm-bindgen 在 native 上是会 panic 的桩）。`--no-default-features` 不能省。
- **`--locked` 一致**：CI 用它构建，本地漏掉会让锁文件漂移，直到 CI 才暴露。

## 本地与 CI 不一样的地方

| 现象 | 说明 |
| --- | --- |
| `cargo test --release` 失败 | `[profile.release]` 为 wasm 体积设了 `panic = "abort"`，与测试目标要求的 unwind 不兼容。测试用 debug 跑 |
| Windows + Node 24 下 `wasm-pack test --node` 全部测试通过后仍 exit 1 | runner 退出时触发 libuv 断言（`src/win/async.c`）。把 Node 20/22 前置到 `PATH` 可规避；CI 是 Linux + Node 22，不受影响 |
| `cargo` 拉不到依赖 | 走代理即可，不改全局配置：PowerShell `$env:HTTPS_PROXY='http://127.0.0.1:7897'`，bash `HTTPS_PROXY=... cargo ...` |
| 改了公开 API 后 doctest 失败 | `tokenize` / `dedup_preserve_order` 返回入参的借用切片，`tokenize(&normalize(x))` 里那个临时值活不过返回值。文档示例必须跟着改——这也提醒：这类签名变化是**破坏性变更** |

## 提交

Conventional Commits：英文、祈使语气、`type: 说明` 整行 ≤50 个 Unicode 字符、说明部分不加标点、
**不写 body**。一次任务按语义分组提交（`fix:` / `perf:` / `refactor:` / `docs:` / `ci:` / `test:` / `style:`），
每条提交都能单独编译、单独解释。

## 发版

1. 用 `bumpp` 改版本（同时改 `Cargo.toml` 与 `Cargo.lock`），提交 `chore: release vX.Y.Z`。
2. 打 tag `vX.Y.Z` 并推送。**`v*.*.*` tag 是唯一的触发器**，没有分支或 PR 触发。
3. `release.yaml` 调 `ci.yaml` 做 checks → `harbor` 发布到 crates.io → 把 checks 里构建的那份 artifact
   挂到该 tag 的 GitHub Release。

三条硬约束：

- **workflow 定义取自被 tag 的那个提交。** tag 打在旧提交上，跑的就是旧 workflow；改了 CI 必须先推
  main，再从新提交打 tag。
- **`harbor` 校验「tag 名 == manifest 版本」**，且发布不可重来：crate 名 + 版本号用过就永远占住，
  yank 也不释放。所以流程设计成宁可先失败，也不要猜（`checks` 不过 → `publish` 根本不启动）。
- **`CARGO_REGISTRY_TOKEN` 必须在仓库 secrets 里**（harbor 的 `check` 会校验它存在且非空）。
  最好用只覆盖本项目 crate 的 granular token。

改动 CI 或元数据后，本地先做两件能做的事：YAML 用 `yaml.safe_load` 解析一遍确认结构，
以及 `cargo publish --dry-run --locked`（它会打包并在隔离目录里验证构建，但不上传）。

## 踩过的坑

| 现象 | 原因 | 教训 |
| --- | --- | --- |
| CI 在 `cargo fmt --all --check` 红，本地却绿 | 改完代码没跑 fmt，本地只跑了 test 与 clippy | 见上表第 1 条；fmt 是门禁，不是可选项 |
| `--verify` 报「自检抽样为空」，只在 fixture 上出现 | 直方图用 `or_insert((doc_id, 0))` 把首次出现记成 0，紧接着 `entry.0 != doc_id` 对刚插入的条目为假、跳过累加 → 出现于 N 篇的词条只记 N−1 次，被 `VERIFY_MIN_DOC_FREQ` 全滤掉 | 「同一篇只算一次」要按「已计入本篇」判重，而不是按值是否相等；小语料要留回归测试 |
| `clippy -D warnings` 报 `needless_lifetimes` | 只有单个输入生命周期时不需要显式标注 | 本地也要带 `-D warnings` 跑 clippy，否则只是把红灯推后一步 |
| tag 推了但 release workflow 没反应 | tag 指向的提交里没有那个 workflow 文件 | 改 workflow 与打 tag 的顺序不能反 |
| wasm 产物里混进了 comrak/regex 的依赖 | `wasm-pack build` 忘了 `--no-default-features` | 不会坏，但构建变慢、体积变大；写进命令清单里避免凭记忆 |
