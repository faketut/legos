# SPEC — legos-book（高频内存账簿）

> 代码若与本文冲突，以本文为准做重构。
> 测试映射：`src/l2.rs`、`src/l3.rs` 的 `#[cfg(test)]` 模块。

## 1. 数据契约（Data Spec）

### 1.1 L2FlatArrayBook\<LEVELS\> 布局

```
bids: [(i64, u32); LEVELS]   价格降序，bids[0] = 最优买价
asks: [(i64, u32); LEVELS]   价格升序，asks[0] = 最优卖价
bid_len / ask_len: usize      有效前缀长度
```

- 全部栈上连续数组，`apply` 全程**零堆分配**；
- 排序不变式：`bids[..bid_len]` 严格降序、`asks[..ask_len]` 严格升序，
  同价位只保留一条（数量聚合）。

### 1.2 L3MapBook 布局

```
orders:  BTreeMap<u64, Order>        订单 ID → 明细（排队位置分析）
bid_qty: BTreeMap<i64, u32>          价格 → 该档总挂单量
ask_qty: BTreeMap<i64, u32>          价格 → 该档总挂单量
```

- `BTreeMap` 会堆分配：本积木定位**深度研究 / 排队分析**，
  不在极致热路径上；热路径用 `L2FlatArrayBook`。

### 1.3 L2DirectIndexBook\<BASE, TICK\> 布局

```
bids:     [u32; 1024]    买盘数量，下标 s ↔ 价格 BASE + s * TICK
asks:     [u32; 1024]    卖盘数量，下标含义同上
bid_mask: [u64; 16]      买盘占用位图
ask_mask: [u64; 16]      卖盘占用位图
bid_top / ask_top: Option<usize>   当前最优档下标（空盘时 None）
bid_count / ask_count: usize        有效档位数（监控用）
```

- **固定档距契约**：合法价格必须满足 `(price - BASE) % TICK == 0`
  且落在 `[BASE, BASE + TICK * 1024)` 内；越界 / 不对齐的价格事件
  **直接忽略**（与 §2.1 的深度截断同属"静默丢弃远端流动性"的取舍，
  但丢弃的是**区间外**而非**深度外**的流动性）；
- 全部栈上，`apply` 全程**零堆分配**；
- 不变式：`qty[s] > 0 ⟺ mask 第 s 位 = 1`；`bid_top` 恒为买盘最大已占用下标
 （卖盘为最小），删最优档时由位图回扫重建。

## 2. 行为契约（Behavior Spec）

### 2.1 L2 事件处理算法

1. 在对应数组**有效前缀**上二分查找 `price`（`O(log LEVELS)`）：
   买盘用降序 comparator、卖盘用升序；`Ok(idx)` 命中，`Err(idx)` 为插入位；
2. 命中：原地 `qty += delta`；`qty ≤ 0` 时 `copy_within` 前移删除该档；
3. 未命中且 `delta > 0`：`copy_within` 后移插入新档；
   未命中且 `delta ≤ 0`：忽略（减少不存在的档位）；
4. **截断策略**：数组满（`len == LEVELS`）时，
   新档若比最差档还差（`idx == LEVELS`）直接丢弃；
   若更优则插入并挤掉最差档（长度不变）。

事件语义（`apply`）：

| `kind` | 动作 |
|--------|------|
| `Add` | `add_qty(side, price, +qty)` |
| `Cancel` | `add_qty(side, price, −qty)` |
| `Trade` | `add_qty(side, price, −qty)`（吃掉盘口流动性，按事件方向扣减） |

### 2.1b L2DirectIndexBook 事件处理算法（O(1) 确定性）

1. `slot = (price - BASE) / TICK`：越界 / 不对齐 → 忽略（§1.3 固定档距契约）；
2. 命中（`qty[slot] > 0`）：原地 `qty += delta`；`qty ≤ 0` 时清零该槽、
   清位图位；若删掉的是当前最优档（`slot == bid_top/ask_top`），
   用位图回扫找次优——买盘 `prev_occupied`（向下）、卖盘 `next_occupied`
   （向上），至多扫描 16 个 64 位字，**常数上界、与盘口深度无关**；
3. 未命中且 `delta > 0`：写入数量、置位图位；若新档更优则更新
   `bid_top`（下标更大）/ `ask_top`（下标更小）；
   未命中且 `delta ≤ 0`：忽略（减少不存在的档位）；
4. **无 `memmove`、无二分查找**：所有路径的指令数上界固定，
   最坏情况依然确定（对比 §2.1 的 `O(LEVELS)` 平移）。

事件语义（`apply`）与 §2.1 的表完全一致（Add/Cancel/Trade 的 `±qty` 规则相同）。

### 2.2 L3 事件处理语义

| `kind` | 动作 |
|--------|------|
| `Add` | `orders[order_id] = Order::from(tick)`；价格档 `+= qty` |
| `Cancel` | 按 `order_id` 查原订单，全额撤销并从价格档扣减；不存在的 ID **静默忽略** |
| `Trade` | 成交方向取反（买方主动吃卖盘/反之），从最优档起跨档扣减 `qty`；只维护档位总量，不追踪具体吃掉的订单 ID |

### 2.3 OrderBook trait 契约

- `best_bid` / `best_ask`：空账簿返回 `None`；
- `mid_price`（默认实现）：双边均值 `(b + a) / 2`，任一方向缺失则 `None`；
- `apply` 不得 panic：非法/超界输入按 §2.1/§2.2 的忽略规则处理。

## 3. 测试映射

| 契约条目 | 测试 |
|----------|------|
| §2.1 排序/最优价 | `best_bid_ask_and_mid`（乱序插入） |
| §2.1 同价聚合 | `aggregate_same_price_level` |
| §2.1 扣减/删档 | `cancel_and_trade_reduce_levels` |
| §2.1 截断 | `depth_truncation_drops_worse_levels` |
| §2.1 二分定位 | `binary_search_positions`（升序/降序/边界） |
| §2.2 按 ID 撤销 | `add_cancel_by_order_id`（含不存在 ID 忽略） |
| §2.2 跨档吃单 | `trade_takes_liquidity_across_levels` |
| §2.3 空账簿 | `empty_book_has_no_quotes`、`mid_price_from_trees` |
| §1.3/§2.1b 网格最优价 | `l2_o1::best_bid_ask_and_mid`（乱序插入） |
| §1.3/§2.1b 同价聚合 | `l2_o1::aggregate_same_price_level` |
| §1.3/§2.1b 扣减/删档 | `l2_o1::cancel_and_trade_reduce_levels` |
| §2.1b 最优档删除回扫 | `l2_o1::best_removal_falls_back_to_next_level`（买盘向下/卖盘向上） |
| §1.3 越界/不对齐忽略 | `l2_o1::out_of_range_and_misaligned_prices_ignored`、`l2_o1::slot_mapping_edges` |
| §2.1b≡§2.1 行为等价 | `l2_o1::equivalence_with_flat_book_on_grid_stream`（20k tick 逐 tick 对比最优价+深度） |
| §2.1b≡§2.1 截断下等价 | `l2_o1::best_quotes_match_flat_book_under_truncation`（40 档 > LEVELS，最优价一致） |
| §2.1b≡§2.1 最坏序列等价 | `l2_o1::worst_case_stream_matches_flat_book`（下标 0 反复插入/删除） |

## 4. A/B 结论：L2FlatArrayBook vs L2DirectIndexBook（2026-09-29）

基准：`legos-app/benches/l2_ab_bench.rs`
（`cargo bench -p legos-app --bench l2_ab_bench -- --nocapture`，release，
本 VM）。两种 tick 流：`mixed`（100k 伪随机 Add/Cancel/Trade，40 档/侧），
`worst`（对 flat 版最坏的跨档跳变：下标 0 反复插入/删除，满数组 `memmove`）。

### 4.1 数据

**均值**（criterion，`apply` 单 op）：

| 流 | flat\<10\> | O(1) | 结论 |
|---|---|---|---|
| mixed | ~40–57ns（VM 噪声大） | ~25–55ns | 基本持平，噪声内 |
| worst | ~55–59ns | ~12–13ns | **O(1) 快约 4.5 倍** |

**尾延迟**（逐 tick `Instant` 计时，含每 tick 一次
`best_bid`/`best_ask`/`mid_price` 读取，还原管线真实路径）：

| 流/实现 | mean | p50 | p99 | p999 | max |
|---|---|---|---|---|---|
| flat/mixed | 74.1ns | 70ns | 100ns | 120ns | 60µs |
| O(1)/mixed | 56.4ns | 50ns | 70ns | **90ns** | 80µs |
| flat/worst | 72.6ns | 70ns | 81ns | 200ns | 107µs |
| O(1)/worst | 57.0ns | 50ns | 60ns | **80ns** | 190µs |

（`max` 的 µs 级毛刺为 VM 噪声；`flat/mixed` 在另一次 ~400 遍独立测量中
p50/p99/p999 稳定在 70/100/120ns，与上表一致。）

### 4.2 判定

- 性能门：**通过**。p999 改善 mixed −25%、worst −60%（判定线 ≥10%），
  p50 从 70ns 降到 50ns，**无回归**；
- 但**默认实现不切换**，`main.rs` 管线继续用 `L2FlatArrayBook<10>`。
  原因：O(1) 版的固定区间契约（§1.3）与实盘数据路径不兼容——
  `scripts/fetch_ticks.py` 用 `PRICE_SCALE=1e8`，BTC tick 价格约 1e13，
  永远落在任何合理的 1024 档固定区间之外，会被**静默忽略**导致账簿恒空；
  而档距 TICK 是编译期常量，无法同时覆盖示例 CSV（价格 ~1e6）和
  Binance 数据（价格 ~1e13）两种量级。flat 版接受任意价格，
  是唯一对任意输入都正确的默认。

### 4.3 切换方法（泛型参数一处替换）

两者实现同一 `OrderBook` trait，管线其余代码零改动。价格区间固定且
网格对齐的品种（如单合约回测），可把装配中的账簿积木换成：

```rust
use legos_book::L2DirectIndexBook;
// 覆盖 [BASE, BASE + TICK * 1024)，价格须对齐 TICK：
let book = L2DirectIndexBook::<99_0000, 100>::new();
```

`BASE`/`TICK` 按品种选择：区间须包住该品种全部可能价格，
`TICK` 取该品种最小报价单位。区间外的 tick 会被静默忽略（§1.3），
切换前务必用历史数据验证无越界（`l2_o1` 单测
`out_of_range_and_misaligned_prices_ignored` 描述了忽略语义）。
