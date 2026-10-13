// wasm 冒烟测试：证明「wasm-pack 产物 + build-index 产物」在真实 JS 运行时里能查询。
//
// 只依赖 Node 内置能力（不装任何 npm 包），因此可以直接进 CI：
//
//   cargo run --release --features cli --bin build-index -- \
//     --input tests/fixtures/search-full.json --output target/smoke.idx
//   wasm-pack build --target web --release --out-dir pkg --features wasm
//   node scripts/smoke-wasm.mjs target/smoke.idx
//
// 断言覆盖：构造 → 元信息 → 中文句中词（旧引擎的失败场景）→ 分面过滤 → 摘要。

import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import path from 'node:path'
import process from 'node:process'

const here = path.dirname(fileURLToPath(import.meta.url))
const root = path.resolve(here, '..')

const indexArg = process.argv[2] ?? 'target/smoke.idx'
const indexPath = path.isAbsolute(indexArg) ? indexArg : path.join(root, indexArg)
const pkgDir = path.join(root, 'pkg')

const failures = []
function check(label, condition, detail) {
  if (condition) {
    console.log(`  ✅ ${label}`)
  }
  else {
    console.log(`  ❌ ${label}${detail === undefined ? '' : ` — ${detail}`}`)
    failures.push(label)
  }
}

const { default: init, SearchIndex } = await import(
  new URL(`file://${path.join(pkgDir, 'blog_search.js').replaceAll('\\', '/')}`).href
)

const wasmBytes = readFileSync(path.join(pkgDir, 'blog_search_bg.wasm'))
const indexBytes = readFileSync(indexPath)

await init({ module_or_path: wasmBytes })
const index = new SearchIndex(indexBytes)

console.log(`索引：${indexPath}（${(indexBytes.length / 1024).toFixed(1)} KiB）`)
console.log(`wasm：blog_search_bg.wasm（${(wasmBytes.length / 1024).toFixed(1)} KiB）`)
console.log(`文档 ${index.docCount()} 篇 / 词条 ${index.termCount()} 个 / 格式版本 ${index.formatVersion()}`)
console.log(`分面 [标签, 分类] = [${index.facetCounts().join(', ')}]`)

console.log('\n断言：')
check('格式版本为 1', index.formatVersion() === 1, index.formatVersion())
check('文档数为 3', index.docCount() === 3, index.docCount())
check('词条数大于 0', index.termCount() > 0, index.termCount())

const perf = index.query('性能', null, null, 10)
check('「性能」命中 2 篇', perf.length === 2, `实际 ${perf.length}`)
check('标题命中排在正文命中之前', perf[0]?.path === '/post/1', perf[0]?.path)
check('返回的 tags 是数组', Array.isArray(perf[0]?.tags), typeof perf[0]?.tags)
check('摘要包含关键词', String(perf[0]?.snippet ?? '').includes('性能'), perf[0]?.snippet)

const rust = index.query('rust', null, null, 10)
check('「rust」大小写不敏感命中 1 篇', rust.length === 1, `实际 ${rust.length}`)

const single = index.query('性', null, null, 10)
check('单字「性」走前缀展开命中 2 篇', single.length === 2, `实际 ${single.length}`)

const scoped = index.query('性能', 'Vue', null, 10)
check('标签限定 Vue 后「性能」只剩 1 篇', scoped.length === 1, `实际 ${scoped.length}`)

const scopedOnly = index.query('', 'Vue', null, 10)
check('只用标签限定返回 2 篇且摘要为空', scopedOnly.length === 2 && scopedOnly.every(h => h.snippet === ''))

const miss = index.query('不存在的词xyz', null, null, 10)
check('无命中返回空数组', Array.isArray(miss) && miss.length === 0, miss.length)

const limited = index.query('前端', null, null, 1)
check('limit 生效', limited.length <= 1, limited.length)

let threw = false
try {
  // eslint-disable-next-line no-new
  new SearchIndex(new Uint8Array([1, 2, 3, 4]))
}
catch {
  threw = true
}
check('非法字节流抛错而不是静默返回空索引', threw)

console.log('')
if (failures.length) {
  console.error(`wasm 冒烟测试失败：${failures.length} 项 —— ${failures.join('、')}`)
  process.exit(1)
}
console.log('wasm 冒烟测试全部通过')
