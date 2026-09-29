# SPEC — legos-core（契约中心）

> 本文件是 `legos-core` 的**规范**（Spec-Driven Development 锚点）。
> 代码若与本文冲突，以本文为准做重构。
> 测试映射：`src/lib.rs` 的 `#[cfg(test)]` 模块逐条验证下述契约。

## 1. 数据契约（Data Spec）

### 1.1 MarketTick — 32 字节定长行情事件

系统内流转的最小行情事件。所有跨组件（feed → bus → book → strategy）
传递的行情都使用此类型。

| 偏移 | 大小 | 字段 | 类型 | 说明 |
|------|------|------|------|------|
| 0 | 8 | `price` | `i64` | 整数价格（tick，如 1 tick = 0.0001 USD） |
| 8 | 8 | `ts_ns` | `u64` | 纳秒时间戳 |
| 16 | 4 | `qty` | `u32` | 数量 |
| 20 | 2 | `symbol_id` | `u16` | 品种编号（上限 65535） |
| 22 | 2 | `_pad0` | `u16` | **显式填充**，构造时清零 |
| 24 | 4 | `order_id` | `u32` | 交易所订单号（上限 2³²−1） |
| 28 | 1 | `flags` | `u8` | bit-mask（见 1.2） |
| 29 | 3 | `_pad1` | `[u8;3]` | **显式填充**，构造时清零 |

- `#[repr(C)]`，`size_of == 32`，`align_of == 8`。**拒绝隐式填充**：
  每个对齐空隙都有名有姓（`_pad0`/`_pad1`），私有限制外部只能经
  `MarketTick::new()` 构造（填充恒为 0，避免未初始化字节泄漏进共享内存/网络）。
- `Copy` 语义：按位拷贝，可直接进无锁队列 / 共享内存，全程零堆分配。
- **全局约定**：价格统一整数 tick；**全系统数量字段统一 `u32`**；
  `symbol_id` 全链路 `u16`。

### 1.2 flags bit-mask 位置

```
bit:  7  6  5  4  3 | 2  1 | 0
      保留（恒 0）   | kind  | side
```

| 位 | 含义 | 取值 |
|----|------|------|
| 0 | `side` | 0 = Bid，1 = Ask |
| 1–2 | `kind` | 0 = Add，1 = Cancel，2 = Trade；**3 为保留值，永不产生** |
| 3–7 | reserved | 必须为 0（`reserved_bits_clear()` 供损坏检测） |

访问只经 `side()` / `kind()` 方法；`kind()` 遇到保留值 3 时按契约映射为
`Trade`（该值永不产生，属防御性分支）。

### 1.3 其他定长类型

| 类型 | 大小 | 说明 |
|------|------|------|
| `Order` | 40 B | L3 账簿内部表示；`order_id` 保留 `u64`（由 `MarketTick` 的 u32 零扩展） |
| `OrderIntent` | 40 B | 策略→风控→网关的意图；`price == 0` 约定为市价单 |
| `OrderAck` | 24 B | 网关回执；`filled_qty: u32`，`avg_price: i64`（未成交为 0） |

### 1.4 风控拒绝原因码（`&'static str` 枚举）

| 常量 | 值 | 含义 |
|------|----|------|
| `REJECT_NOTIONAL_EXCEEDED` | `"notional_exceeded"` | 单笔名义金额超限 |
| `REJECT_QTY_EXCEEDED` | `"qty_exceeded"` | 单笔数量超限 |
| `REJECT_RATE_EXCEEDED` | `"rate_exceeded"` | 单位时间订单数超限 |

风格契约：非空、小写下划线、互不相同，可直接用作日志/metrics 标签。

## 2. 行为契约（Behavior Spec）

### 2.1 订单生命周期状态机

```
Created → PendingNew ─┬─→ Accepted ─┬─→ PartiallyFilled ─→ Filled
   │                  │             │        └→ Canceled
   └→ Rejected        ├─→ Filled ───┘  (网关立即成交)
                      ├─→ PartiallyFilled
                      └─→ Rejected
```

合法转换表（`TrackedOrder::advance` 严格实现此表）：

| from | to（合法） |
|------|-----------|
| `Created` | `PendingNew`, `Rejected` |
| `PendingNew` | `Accepted`, `PartiallyFilled`, `Filled`, `Rejected` |
| `Accepted` | `PartiallyFilled`, `Filled`, `Canceled` |
| `PartiallyFilled` | `PartiallyFilled`, `Filled`, `Canceled` |
| `Filled` / `Canceled` / `Rejected`（终态） | 无出边 |

**拦截规则**：任何非法转换返回 `Err(IllegalTransition { from, to })`，
且**不得改变当前状态**。典型拦截用例：`Filled` 的订单再次被撤单
（`Filled → Canceled`）必须失败——终态无出边。

`is_terminal()`：`Filled | Canceled | Rejected` 为终态。

### 2.2 管线中的状态推进（同步管线语义）

每个 `OrderIntent` 配一个 `TrackedOrder`（`Created` 出生）：

1. `risk.check_order` 返回 `Ok` → `advance(PendingNew)`；
   返回 `Err` → `advance(Rejected)`，订单**不得**送网关；
2. `gateway.send_order` 回执映射：
   `Accepted → Accepted`，`Filled → Filled`，
   `PartiallyFilled → PartiallyFilled`，`Rejected → Rejected`；
3. 同步管线中订单在一个 tick 内走完可观测生命周期；
   `Accepted` 在此为观察终点（实盘中后续由成交回报继续推进）。

## 3. Code-as-Spec：Trait 签名

六大积木插槽（只声明契约，不含算法；实现由 Rust 编译器裁判）：

```rust
pub trait MarketDataFeed {
    fn next_event(&mut self) -> Option<MarketTick>;
    // 契约：吐出的每个 MarketTick 符合 §1；EOF/暂时无数据均返回 None。
    // 一致性：不同 feed 对同一逻辑事件的输出须逐字段一致
    //       （order_id 来源不同除外，见 legos-feed/SPEC.md）。
}

pub trait MessageBus {
    type Item: Copy;
    fn push(&self, item: Self::Item) -> bool; // 满返回 false，永不阻塞/分配
    fn pop(&self) -> Option<Self::Item>;
    // 契约：只取 &self（内部原子同步）；SPSC 保序、无丢失。
}

pub trait OrderBook {
    fn apply(&mut self, ev: &MarketTick);
    fn best_bid(&self) -> Option<(i64, u32)>;
    fn best_ask(&self) -> Option<(i64, u32)>;
    fn mid_price(&self) -> Option<i64> { /* 默认：双边均值，任一缺失则 None */ }
}

pub trait TradingStrategy {
    fn on_tick(
        &mut self,
        bid: Option<(i64, u32)>,
        ask: Option<(i64, u32)>,
        mid: Option<i64>,
    ) -> Option<OrderIntent>;
    // 契约：每次调用至多一个 intent；ts_ns 由管线统一打戳。
}

pub trait PreTradeRisk {
    fn check_order(&mut self, order: &OrderIntent) -> Result<(), &'static str>;
    // 契约：Err 的原因码必须是 §1.4 枚举之一。
}

pub trait ExecutionGateway {
    fn send_order(&mut self, order: &OrderIntent) -> OrderAck;
    // 契约：永不 panic；失败以 AckStatus::Rejected 表达。
    fn on_quote(&mut self, _symbol_id: u16, _bid: Option<(i64, u32)>, _ask: Option<(i64, u32)>) {}
}
```

## 4. 测试映射

| 契约条目 | 测试 |
|----------|------|
| §1.1 大小/对齐/偏移 | `market_tick_data_contract`（`addr_of!` 锁死每个偏移） |
| §1.2 bit-mask | `flags_bitmask_contract`（6 种组合 + 填充清零） |
| §1.4 原因码风格 | `reject_reason_codes_are_documented` |
| §2.1 合法路径 | `order_state_machine_valid_paths` |
| §2.1 非法拦截（49 种穷举） | `order_state_machine_bombardment` |
| trait 默认方法 | `mid_price_needs_both_sides`（Mock 实现） |
