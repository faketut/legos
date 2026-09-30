# Legos —— 零开销编译期积木高频交易系统

> *ZeroOverhead-HFT*：把交易系统的每个组件做成编译期可替换的“乐高积木”。

本系统（代号 **Legos**）的核心架构是**基于 Rust Trait 与泛型的静态分发设计**。
六大组件全部定义为 trait，装配时只用泛型参数指定具体实现——编译期的
**单态化（Monomorphization）** 会把层层解耦的组件抹平、内联成一条直线代码，
运行期**彻底消除虚函数指针跳转（Dynamic Dispatch）**，热路径**零堆分配**，
压榨出如同手写一体化 C++ 般的零开销低延迟表现。

切换积木 = 改一行泛型参数 = 重新编译。不改逻辑代码，不碰运行时配置。

```rust
// 回测装配
Pipeline<CsvFileFeed, SpscRingBuffer<MarketTick, 4096>, L2FlatArrayBook<10>,
         MarketMakerStrategy, PassThroughRisk, SimulatedExchange>
// 实盘装配：只换泛型参数，其余代码一字不动
Pipeline<NativeItchParser, SharedMemoryBus<MarketTick, 4096>, L3MapBook,
         MarketMakerStrategy, HardLimitRisk, FixProtocolGateway>
```

## 四步开发流水线

按“由内到外、由低向高”的顺序构建（若先写策略/网关会导致 Trait 后期大规模重构）：

| Phase | 内容 | 交付 crate |
|-------|------|-----------|
| 1. 数据总线与基础设施 | 先确立核心数据结构 `MarketTick`（32 字节定长、`Copy`，热路径零堆分配）、`Order`，再实现 `SpscRingBuffer`（手写无锁队列：`MaybeUninit` 数组 + 原子 `head`/`tail`）和 `SharedMemoryBus`（Linux 共享内存跨进程）。无锁队列的并发安全是全系统的血管，最先跑通 | `legos-core`, `legos-bus` |
| 2. 高频内存账簿 | `L2FlatArrayBook`：固定连续数组上的**零堆分配二分插入/查找**维护价格梯队，L1/L2 缓存友好；`L3MapBook`：按订单 ID 的树状结构，用于深度排队分析 | `legos-book` |
| 3. 外围网关与仿真器 | `CsvFileFeed`（回测读文件）、`SimulatedExchange`（回测内存撮合）、`NativeItchParser`（实盘 NASDAQ ITCH 5.0 二进制解析）、`FixProtocolGateway`（实盘 FIX 4.4）。解析出的 MarketTick 丢进总线驱动账簿，形成输入闭环 | `legos-feed`, `legos-gateway`, `legos-risk` |
| 4. 策略逻辑与泛型管线编织 | `MarketMakerStrategy` 做市、`ArbitrageStrategy` 跨场所套利；`Pipeline` 主循环用一行泛型声明把 6 大组件在编译期织成直线流水线；`core_affinity` 把主循环绑定到指定物理 CPU 核心；`criterion` 全链路延迟基准 | `legos-strategy`, `legos-app` |

## Crate 地图

```
legos/
├── legos-core      # 地基：MarketTick/Order/OrderIntent/OrderAck（定长 Copy）+ 六大 trait + 订单状态机
│                     # MarketDataFeed / MessageBus / OrderBook / TradingStrategy /
│                     # PreTradeRisk / ExecutionGateway —— 热路径无 dyn、无堆分配
├── legos-bus       # Phase 1：SpscRingBuffer<T: Copy, const CAP: usize>（原子 head/tail）
│                   #          SharedMemoryBus（shm_open+mmap，extern "C" 直调 POSIX，零第三方依赖）
├── legos-book      # Phase 2：L2FlatArrayBook<LEVELS>（扁平数组二分查找+copy_within 平移）
│                   #          L3MapBook（BTreeMap<u64, Order> + 价格档树）
├── legos-feed      # Phase 3：CsvFileFeed（含 data/sample_ticks.csv 示例数据）
│                   #          NativeItchParser（ITCH 5.0：'A'/'E'/'C'/'X'/'D'）
├── legos-risk      # Phase 3：PassThroughRisk（恒 Ok，单态化后零开销）
│                   #          HardLimitRisk（单笔名义金额/数量/每秒订单数硬限额）
├── legos-gateway   # Phase 3：FixProtocolGateway（FIX 4.4 NewOrderSingle，TCP，失败永不 panic）
│                   #          SimulatedExchange（内存撮合 + 滑点模型，64 槽位零堆分配报价表）
├── legos-strategy  # Phase 4：MarketMakerStrategy / ArbitrageStrategy /
│                   #          PythonBindingStrategy（默认 stub；python feature 切真实嵌入）
├── legos-app       # Phase 4：Pipeline<F, B, Bk, S, R, G> 泛型主循环 + core_affinity 绑核
│                   #          + benches/pipeline_bench.rs（criterion 全链路基准）
└── legos-testnet   # 0 成本实盘验证：TestnetWsFeed（Binance Testnet 免费 WS 行情）
                    #          TestnetRestGateway（Testnet 免费 REST 下单，虚拟资金）
                    #          + src/bin/paper_trade.rs（paper-trading 压力测试）
```

## 如何替换积木块

所有替换都在**类型层面**完成。`legos-app/src/main.rs` 的 `_demo_swap_bricks_at_compile_time`
函数（虽未被 main 调用，但会被编译）就是证明：

```rust
// 回测 → 实盘：换三个泛型参数
let mut pipe: Pipeline<
    CsvFileFeed,
    SpscRingBuffer<MarketTick, 4096>,
    L3MapBook,            // ← L2FlatArrayBook<10> 换成 L3MapBook
    MarketMakerStrategy,
    HardLimitRisk,        // ← PassThroughRisk 换成 HardLimitRisk
    FixProtocolGateway,   // ← SimulatedExchange 换成 FixProtocolGateway
> = Pipeline::new(feed, bus, book, strategy, risk, gateway);
```

| 场景 | 行情 | 总线 | 账簿 | 策略 | 风控 | 网关 |
|------|------|------|------|------|------|------|
| 本地回测 | `CsvFileFeed` | `SpscRingBuffer` | `L2FlatArrayBook<10>` | `MarketMakerStrategy` | `PassThroughRisk` | `SimulatedExchange` |
| 实盘交易 | `NativeItchParser` | `SharedMemoryBus` | `L2FlatArrayBook<10>` | `MarketMakerStrategy` | `HardLimitRisk` | `FixProtocolGateway` |
| 深度研究 | `CsvFileFeed` | `SpscRingBuffer` | `L3MapBook` | `PythonBindingStrategy` | `PassThroughRisk` | `SimulatedExchange` |
| 模拟盘压力测试 | `TestnetWsFeed` | （直连账簿） | `L2FlatArrayBook<10>` | `MarketMakerStrategy` | `HardLimitRisk` | `TestnetRestGateway` |

## 0 成本实盘验证

全程免费：公开行情不要 key，testnet 下单用虚拟资金。分两步：

### 第 1 步：抓真实 Tick 数据做回测

```bash
# 抓一天的 BTCUSDT 公开成交数据（data.binance.vision 日度 zip，免费、无需 key）
python3 scripts/fetch_ticks.py --symbol BTCUSDT --date 2026-09-27 --out data/btcusdt.csv
# 输出末尾有一行校验：OK rows=478532 cols=symbol_id,side,price,qty,kind,ts_ns ts_range=[...]

# 用抓到的数据跑回测（第一个 CLI 参数覆盖默认示例 CSV）
cargo run -p legos-app -- data/btcusdt.csv
```

输出格式与 `CsvFileFeed` 的解析 schema **严格一致**
（`symbol_id,side,price,qty,kind,ts_ns`，见 `legos-feed/SPEC.md`），
价格/数量刻度（1e-8）与 `TestnetWsFeed` 完全对齐（对齐单测覆盖）。

> 说明：公开成交数据只有 `TRADE` 事件、没有挂单事件，账簿形不成买卖盘口，
> 所以做市策略的 `intents` 为 0——这是**预期行为**，不是 bug。
> 这一步验证的是**吞吐链路**：47.8 万个 tick 零丢失走完
> feed → 总线 → 账簿 → 策略 → 风控 → 网关（含背压不变量回归测试）。
> 策略行为验证请用 `legos-feed/data/sample_ticks.csv`（含 ADD 事件）。

也可用 OKX 公开接口抓近期数据（同样免费、无需 key）：

```bash
python3 scripts/fetch_ticks.py --source okx --symbol BTCUSDT --max-trades 10000 --out data/btcusdt_okx.csv
```

### 第 2 步：Testnet 模拟盘压力测试

把泛型参数换成实盘积木（`legos-testnet/src/bin/paper_trade.rs` 已装配好）：

```rust
TestnetWsFeed          // ← CsvFileFeed：Binance Testnet 免费 WS 行情，无需 key
    → L2FlatArrayBook  // 账簿不变
    → MarketMakerStrategy
    → HardLimitRisk
    → TestnetRestGateway // ← SimulatedExchange：Testnet REST 下单，虚拟资金
```

```bash
# 1. 免费申请 testnet key（虚拟资金，0 成本）：
#    打开 https://testnet.binance.vision → 登录 → API Management → Create API（HMAC）
export BINANCE_TESTNET_API_KEY=...
export BINANCE_TESTNET_API_SECRET=...

# 2. 跑模拟盘（默认 BTCUSDT，500 个 tick 后退出并打印统计）
cargo run -p legos-testnet --bin paper_trade -- BTCUSDT 500
```

安全说明：

- **行情流不需要 key**，匿名即可订阅；
- 下单 key **只从环境变量读取**，代码里没有任何硬编码密钥
  （缺失时直接报错并提示申请步骤，不会静默失败）；
- 下的是 **testnet 真实订单**（虚拟资金），不是 dry-run，
  但每单后 sleep 300ms 做限流保护，Ctrl-C 可随时中断；
- 如所在地区打不开 `testnet.binance.vision`（如返回 451），
  `TestnetWsFeed::binance_mainnet` 可换用主网公开行情流做只读验证。

## Demo 实录

以下为真实录制的终端输出（非示意）。

### Demo 1：本地回测——20 个 tick 走完六积木链路

```bash
$ cargo run -p legos-app --bin legos-app
[legos-app] 主循环已绑定到 CPU 核心 0
[legos-app] 回测数据源: legos-feed/data/sample_ticks.csv
[legos-app] 回测完成: PipelineStats {
    ticks: 20,
    intents: 18,
    sent: 18,
    rejected_by_risk: 0,
    rejected_by_gateway: 0,
    fills: 0,
    illegal_transitions: 0,
}
```

解读：做市策略对 20 个 tick 产生 18 个交易意图，全部通过风控并送达模拟网关，订单状态机零非法转换。`fills: 0` 是因为示例数据的买卖报价没有交叉（模拟撮合只在交叉时成交）；链路本身已被 `intents`/`sent` 验证，成交路径的覆盖见单测与基准。

### Demo 2：性能——1000 万 tick 的尾部延迟

```bash
$ cargo run --release -p legos-app --bin tail_latency
tail_latency: N = 10000000 ticks
pinned to core 1
tsc_hz = 1.498e9 ticks/s (200ms sleep calibration)
rdtsc overhead floor = 15 cycles (subtracted from every sample)
warmup done (200000 ticks)
fills = 9999996
sort took 28.847599ms
+----------------+------------+
| metric         | latency    |
+----------------+------------+
| mean           |     35.8 ns |
| p50            |     30.0 ns |
| p99            |     70.1 ns |
| p999           |    170.2 ns |
| max            | 3169124.9 ns |
+----------------+------------+
note: per-tick pipeline latency, in-process compute only (no network I/O).
```

解读：逐 tick 用 CPU 时间戳计数器计时并扣除计时器本底；p999 170.2ns，max 的 3.2ms 是共享虚拟机的调度噪声（见 PERF.md）。口径为纯进程内计算，不含网络 I/O。

### Demo 3：Testnet 模拟盘

见上文"0 成本实盘验证 → 第 2 步"。本机因 Binance 地理限制（451）无法录制，请在本地运行：

```bash
cargo run -p legos-testnet --bin paper_trade -- BTCUSDT 500
```

## 构建 / 测试 / 运行

需要 Rust 1.70+（本仓库用 rustup 安装，见环境备注）：

```bash
cargo build --workspace        # 构建全部 9 个 crate
cargo test --workspace         # 运行全部单测（88 个，含多线程 SPSC 压力测试）
cargo run -p legos-app         # 回测：CSV → 总线 → 账簿 → 做市策略 → 风控 → 模拟撮合
cargo run -p legos-app -- data/btcusdt.csv   # 用 scripts/fetch_ticks.py 抓的真实数据回测
cargo bench -p legos-app       # criterion 全链路延迟基准（feed→bus→book→strategy→risk→gateway）
```

启用 Python 嵌入（需要 Python 3.8+ 开发环境）：

```bash
cargo build -p legos-strategy --features python
```

## 基准结果

`cargo bench -p legos-app`（release，`benches/pipeline_bench.rs`），机器：x86_64 Linux：

| 基准 | 含义 | 结果 |
|------|------|------|
| `spsc_ring_buffer/push_pop_tick` | 无锁队列一推一取 | ~17.4 ns |
| `l2_book/apply_tick` | L2 账簿处理一个 MarketTick | ~21.7 ns |
| `full_pipeline/tick_to_fill_1000` | 全链路 1000 tick（feed→bus→book→strategy→risk→gateway） | ~47.8 µs（≈47.8 ns/tick） |

> 注：以上为开发机上的相对参考值，实盘延迟以生产环境实测为准。

## Stub 清单（诚实说明）

| 位置 | Stub 内容 | 原因 |
|------|-----------|------|
| `legos-strategy::PythonBindingStrategy`（默认构建） | `on_tick` 恒返回 `None` 的同名占位 | 避免默认构建依赖 Python 解释器；`--features python` 切换为 pyo3 真实嵌入 |
| `NativeItchParser` 未实现的消息类型（`'S'`/`'R'`/`'H'`/`'P'`/`'Q'`/`'B'`/`'U'` 等） | 按长度跳过 | 实现 `'A'`/`'E'`/`'C'`/`'X'`/`'D'` 已覆盖盘口重构所需的全部订单流；其余类型在模块文档中列表说明，可按需追加分支 |
| `SimulatedExchange` 的非交叉限价单 | 返回 `Accepted`（挂单，不建完整订单簿） | 回测撮合的最小可用语义；文档化说明 |
| `ArbitrageStrategy` | 只发出买入腿意图 | 卖出腿对冲归执行层/组合管理；文档化说明 |
| `CsvFileFeed` 的 `order_id` | 恒为 0 | CSV 格式不携带订单号；L2 账簿不依赖它，L3 场景请用 ITCH feed |

## 许可证

MIT
