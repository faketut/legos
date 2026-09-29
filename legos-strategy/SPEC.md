# SPEC — legos-strategy（策略）

> 代码若与本文冲突，以本文为准做重构。
> 测试映射：各模块的 `#[cfg(test)]`。

## 1. 行为契约（Behavior Spec）

### 1.1 `TradingStrategy` trait 契约

```rust
fn on_tick(
    &mut self,
    bid: Option<(i64, u32)>,
    ask: Option<(i64, u32)>,
    mid: Option<i64>,
) -> Option<OrderIntent>;
```

- 每次调用**至多**返回一个 `OrderIntent`（多腿需求由多次调用完成）；
- 返回的 intent 中 `ts_ns` 填 0——**管线在送风控前统一打戳**；
- 返回 `None` 表示本 tick 无信号；策略**不得**在内部做 IO/分配热路径。

### 1.2 MarketMakerStrategy（高频做市）

参数：`symbol_id: u16`，`spread_ticks: i64`（总价差），
`skew_ticks: i64`（整体偏移），`order_qty: u32`。

- `mid` 为 `None` 时返回 `None`（无中间价不报价）；
- 报价公式：`half = spread_ticks / 2`；
  买价 `= mid − half + skew_ticks`，卖价 `= mid + half + skew_ticks`；
- 每次调用轮流报一边（买→卖→买…，`quote_bid_next` 翻转）；
- `client_order_id` 单调递增（`next_id`）；
- `skew > 0` 上移报价（表达看多 / 库存偏空纠偏）。

### 1.3 ArbitrageStrategy（跨场所套利）

接线：场所 A 的 `bid/ask` 经 `on_tick` 参数传入；场所 B 快照经
`update_venue_b(bid, ask)` 每 tick 同步一次。

触发条件（`threshold_ticks` 覆盖手续费/滑点）：

| 条件 | 动作 |
|------|------|
| `bid_A > ask_B + threshold` | 在 B 以 `ask_B` **买入**（`price = ask_B`） |
| `bid_B > ask_A + threshold` | 在 A 以 `ask_A` **买入**（`price = ask_A`） |
| 任一场所报价缺失 | `None`（不交易） |

- 发出的只是**买入腿**；卖出腿对冲由执行层/组合管理负责（最小可用语义）。

### 1.4 PythonBindingStrategy（中低频研究）

两种形态（同一类型名，编译期切换）：

| 构建 | 行为 |
|------|------|
| 默认（无 feature） | stub：`on_tick` 恒返回 `None`，零开销、零依赖 |
| `--features python` | 真实嵌入 CPython（pyo3 0.22）：`load(path, func, symbol_id)` 加载模型，每 tick 调用 `predict(bid, ask, mid)` |

Python 侧函数契约：

```python
def predict(bid: int | None, ask: int | None, mid: int | None):
    """返回 (side, price, qty)，side 为 "B"/"S"；无信号返回 None。"""
```

- 返回值解析：`side` 仅接受 `"B"`/`"S"`（其他 → `None`）；
  `(String, i64, u32)` 三元组提取失败 → `None`；
- GIL 调用开销在微秒量级，**只适合中低频**；高频请换 `MarketMakerStrategy`。

## 2. 测试映射

| 契约条目 | 测试 |
|----------|------|
| §1.2 报价公式/轮流/ID | `quotes_around_mid_with_spread_and_skew` |
| §1.2 无 mid 不报价 | `no_mid_no_quote` |
| §1.3 价差触发 | `detects_cross_venue_spread` |
| §1.3 阈值下无信号 | `no_signal_when_spread_below_threshold` |
| §1.3 缺快照不交易 | `no_signal_without_venue_b_snapshot` |
| §1.4 stub 静默 | `stub_compiles_and_stays_quiet` |
