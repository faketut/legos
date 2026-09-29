# SPEC — legos-feed（行情接入）

> 代码若与本文冲突，以本文为准做重构。
> 测试映射：`src/csv.rs`、`src/itch.rs`、`src/lib.rs`（`conformance_tests`）。

## 1. 数据契约（Data Spec）

### 1.1 CSV schema（锁定）

**此 schema 为跨系统契约**：`scripts/fetch_ticks.py` 等外部抓取脚本的输出
必须与下表严格一致（列顺序、列名、取值域），否则 `CsvFileFeed` 会静默跳行。

```
首行（表头，解析时跳过）：
symbol_id,side,price,qty,kind,ts_ns

示例：
1,B,1000000,100,ADD,1710000000000000000
```

| 列 | 类型域 | 取值 |
|----|--------|------|
| `symbol_id` | u16 范围整数 | 品种编号 |
| `side` | `B` / `A` | B = 买(Bid)，A = 卖(Ask)；其他值整行跳过 |
| `price` | `i64` | 整数 tick |
| `qty` | u32 范围整数 | 数量 |
| `kind` | `ADD` / `CANCEL` / `TRADE` | 事件种类；其他值整行跳过 |
| `ts_ns` | `u64` | 纳秒时间戳 |

- CSV **不携带订单号**：`order_id` 统一填 0（需要时可用行号回填，见代码注释）。
- 解析失败的行**跳过**（`lines_skipped` 计数），不中断流；
  `next_event` 在 EOF 后**永久**返回 `None`。
- 空行跳过；`open` 不存在的文件返回 `io::Error`。

### 1.2 ITCH 5.0 帧格式

```
[u16 BE 消息长度][1 字节类型][消息体...] [u16 BE 长度][类型][消息体...] ...
```

- 所有多字节整数**大端**；
- `symbol_id` 取消息头 **Stock Locate**（u16，偏移 1）；
- 时间戳取 6 字节字段（偏移 5），解释为纳秒；
- 价格取 ITCH 原生单位（1/10000 USD 的整数），直接作为 tick。

### 1.3 ITCH 消息类型 → MarketTick 映射

| 类型 | 名称 | 长度 | 字段偏移 | 转为 |
|------|------|------|----------|------|
| `'A'` | Add Order | 36 | ref@11(u64), side@19, qty@20(u32), price@32(u32) | `Add` |
| `'E'` | Order Executed | 31 | ref@11, qty@19(u32) | `Trade`（方向/价从 `'A'` 记录补全） |
| `'C'` | Executed w/ Price | 36 | 同 E，price@32(u32) 覆盖 | `Trade` |
| `'X'` | Order Cancel | 23 | ref@11, qty@19(u32) | `Cancel` |
| `'D'` | Order Delete | 19 | ref@11 | `Cancel`（qty = 该单剩余） |

- 未实现类型（`S/R/H/P/Q/B/U/...`）：按帧长度**跳过**，不影响后续消息；
- `'E'/'X'/'D'` 依赖内部 `open: HashMap<order_ref, (Side, price, 剩余股数)>` 表
  （由 `'A'` 写入；成交/撤销时扣减，归零删除）；
- **精度说明**：ITCH `order_ref` 为 u64，`MarketTick.order_id` 为 u32——
  超出 u32 的订单号截断为低 32 位（`as u32`）；`open` 表内部仍用完整 u64
  做精确匹配；
- 半包（`avail < 2 + len`）缓存等待，不吐事件；`cursor` 过大时压缩前移。

### 1.4 跨 feed 一致性契约（Spec-Based Testing 核心）

`CsvFileFeed` 与 `NativeItchParser` 对**同一逻辑事件**产生的 `MarketTick`
必须满足：

- 同一 32 字节布局（`size_of` 相等）；
- `symbol_id / price / qty / side() / kind() / ts_ns / flags` 逐字段相等；
- 唯一允许的差异：`order_id`（CSV 恒为 0，ITCH 取订单号）。

`src/lib.rs::conformance_tests` 用同一 Add 事件双路解析并逐字段断言。

## 2. 规划中的积木（未实现，先记契约）

- **`TestnetWsFeed`**：Binance/OKX Testnet 免费 WS 行情接入，实现
  `MarketDataFeed`；输出须同样满足 §1.4 一致性契约（与 CSV/ITCH 双路对齐测试）。
- `scripts/fetch_ticks.py`：抓加密交易所公开免费 Tick 存 CSV，
  输出列必须与 §1.1 表**逐列一致**（含表头行）。

## 3. 测试映射

| 契约条目 | 测试 |
|----------|------|
| §1.1 解析/跳行/EOF | `parses_sample_file_end_to_end`、`skips_malformed_lines` |
| §1.1 越界 symbol_id | `rejects_out_of_range_symbol_id` |
| §1.3 各消息类型 | `parses_add_order_fields`、`parses_executed_cancel_delete_lifecycle` |
| §1.3 半包/未知类型 | `half_packet_waits_for_rest`、`unknown_message_type_is_skipped` |
| §1.4 一致性 | `csv_and_itch_emit_structurally_identical_ticks`、`both_feeds_yield_valid_32_byte_ticks` |
