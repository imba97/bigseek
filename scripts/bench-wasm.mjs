// wasm 性能基准：加载耗时、查询延迟、内存。可选指定索引路径。
//
//   wasm-pack build --target web --release --out-dir pkg --features wasm
//   cargo run --release --features cli --bin build-index -- \
//     --input ../imba97-blog-vite/public/search-full.json --output target/search.idx
//   node scripts/bench-wasm.mjs target/search.idx

import { readFileSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { performance } from 'node:perf_hooks'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const indexArg = process.argv[2] ?? 'target/search.idx'
const indexPath = path.isAbsolute(indexArg) ? indexArg : path.join(root, indexArg)

const { default: init, SearchIndex } = await import(
  pathToFileURL(path.join(root, 'pkg', 'blog_search.js')).href
)

const wasmBytes = readFileSync(path.join(root, 'pkg', 'blog_search_bg.wasm'))
let t = performance.now()
await init({ module_or_path: wasmBytes })
const initMs = performance.now() - t

const bytes = readFileSync(indexPath)
t = performance.now()
const index = new SearchIndex(bytes)
const loadMs = performance.now() - t

const mb = n => `${(n / 1024 / 1024).toFixed(1)} MiB`
console.log(`wasm 体积 : ${(wasmBytes.length / 1024).toFixed(1)} KiB`)
console.log(`索引体积 : ${(bytes.length / 1024).toFixed(0)} KiB（${index.docCount()} 篇 / ${index.termCount()} 词条）`)
console.log(`wasm 实例化: ${initMs.toFixed(1)} ms（一次性，可被浏览器缓存）`)
console.log(`索引加载  : ${loadMs.toFixed(2)} ms（零反序列化，只校验头部 + 解析分面表）`)
console.log(`内存      : rss ${mb(process.memoryUsage().rss)} / heapUsed ${mb(process.memoryUsage().heapUsed)}`)

const queries = ['性能', '浏览器', '重写', '配置', 'vue', 'rust', '响应式', '性能优化', '最开始知道', '性']
const ROUNDS = 50
console.log(`\n查询延迟（${ROUNDS} 次平均）:`)
let worst = 0
for (const q of queries) {
  let hits = 0
  const start = performance.now()
  for (let i = 0; i < ROUNDS; i++) hits = index.query(q, null, null, 40).length
  const ms = (performance.now() - start) / ROUNDS
  worst = Math.max(worst, ms)
  console.log(`  ${q.padEnd(12)} → ${String(hits).padStart(3)} 条   ${ms.toFixed(3)} ms`)
}
console.log(`\n最慢查询：${worst.toFixed(3)} ms`)
