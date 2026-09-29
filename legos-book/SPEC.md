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
