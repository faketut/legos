# SPEC — legos-gateway（执行网关）

> 代码若与本文冲突，以本文为准做重构。
> 测试映射：`src/sim.rs`、`src/fix.rs` 的 `#[cfg(test)]` 模块。

## 1. 行为契约（Behavior Spec）

### 1.1 `ExecutionGateway` trait 契约

```rust
fn send_order(&mut self, order: &OrderIntent) -> OrderAck;
fn on_quote(&mut self, _symbol_id: u16, _bid: Option<(i64, u32)>, _ask: Option<(i64, u32)>) {}
```

- **`send_order` 永不 panic**：任何失败（未知品种、零数量、网络失败）
  一律返回 `AckStatus::Rejected` 的 `OrderAck`（`OrderAck::rejected(id)`）；
- 回执 → 状态机映射（管线执行）：
  `Accepted → Accepted`，`Filled → Filled`，
  `PartiallyFilled → PartiallyFilled`，`Rejected → Rejected`；
- `on_quote` 默认空实现；`SimulatedExchange` 重写它刷新撮合盘口，
  实盘网关忽略即可。

### 1.2 SimulatedExchange（回测积木）撮合规则

- 报价表：固定 64 槽位 `[(u16, SimQuote); 64]`，**零堆分配**；
  槽位用尽静默忽略（回测品种数远小于 64）；
- `update_quote(symbol_id, bid: (i64, u32), ask: (i64, u32))` 灌入最新最优报价；
- `send_order` 规则：

| 条件 | 回执 |
|------|------|
| 未知品种 / `qty == 0` | `Rejected`（`rejected` 计数 +1） |
| 市价单（`price == 0`） | 按对手价立即成交 |
| 限价单交叉（买 `price ≥ ask` / 卖 `price ≤ bid`）且对手有量 | 成交 |
| 限价单不交叉或对手无量 | `Accepted`（挂单；**不建完整订单簿**，最小可用语义） |

- 成交价 = 对手价 ± `slippage_ticks`（买单加、卖单减）；
- `fill_qty = min(order.qty, touch_qty)`；不足时 `PartiallyFilled`
  并扣减对手盘剩余量；足量时 `Filled`；`fills` 计数 +1。

### 1.3 FixProtocolGateway（实盘积木）报文契约

- `OrderIntent` → FIX 4.4 `NewOrderSingle`（`35=D`）；
- 市价单（`price == 0`）：`40=1` 且**无** `44` 域；限价单：`40=2` + `44=price`；
- `54`: Bid=1 / Ask=2；`55=SYM{symbol_id}`；`11=client_order_id`；
- `8=FIX.4.4` / `9=BodyLength(5 位)` / `10=CheckSum(3 位)` 按规范计算，
  字段间 SOH(`\x01`) 分隔；`9=` 与 `10=` 自洽（单测逐字节校验）；
- 连接**惰性**：`new` 只存配置，首次 `send_order` 才建连
  （`connect_timeout` 2 秒，`TCP_NODELAY`）；
- 任何网络失败（建连/写入）→ 断开并返回 `Rejected`，**不 panic**；
- `34` 域序号每条成功编码的报文 +1。

## 2. 规划中的积木（未实现，先记契约）

- **`TestnetRestGateway`**：Binance/OKX Testnet REST 下单（虚拟资金），
  实现 `ExecutionGateway`；须满足 §1.1 契约（永不 panic、失败即 `Rejected`、
  回执状态机映射一致）。

## 3. 测试映射

| 契约条目 | 测试 |
|----------|------|
| §1.2 交叉成交+滑点 | `crossing_limit_buy_fills_with_slippage` |
| §1.2 挂单 | `non_crossing_limit_rests_as_accepted` |
| §1.2 市价单 | `market_order_fills_at_touch` |
| §1.2 部分成交 | `partial_fill_when_qty_exceeds_touch` |
| §1.1 未知品种拒单 | `unknown_symbol_rejected` |
| §1.3 报文自洽 | `new_order_single_envelope_is_consistent`（9=/10= 校验） |
| §1.3 市价单无 44 域 | `market_order_uses_ordtype_1_without_price` |
| §1.1 建连失败不 panic | `connection_failure_returns_rejected_not_panic` |
| §1.3 回环 TCP | `send_over_loopback_tcp` |
