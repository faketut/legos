# SPEC — legos-risk（事前风控）

> 代码若与本文冲突，以本文为准做重构。
> 测试映射：`src/lib.rs` 的 `#[cfg(test)]` 模块。

## 1. 行为契约（Behavior Spec）

### 1.1 `PreTradeRisk::check_order` 签名契约

```rust
fn check_order(&mut self, order: &OrderIntent) -> Result<(), &'static str>;
```

- `Ok(())`：放行。管线推进 `Created → PendingNew`；
- `Err(reason)`：拦截。`reason` **必须是** `legos-core` 枚举的
  `REJECT_*` 原因码之一（`notional_exceeded` / `qty_exceeded` /
  `rate_exceeded`），管线推进 `Created → Rejected`，**订单不得送网关**；
- `&'static str`：原因码可直接用作日志字段 / metrics 标签，零分配。

### 1.2 PassThroughRisk（回测积木）

- 对**任何**输入恒返回 `Ok(())`（含 `price = i64::MAX, qty = u32::MAX`
  的离谱订单）；
- 具体类型 + `#[inline]`：release 下调用被完全内联消除，**零开销**。

### 1.3 HardLimitRisk（实盘积木）

构造参数：`max_notional: i64`（单笔名义金额上限，tick 单位）、
`max_qty: u32`（单笔数量上限）、`max_orders_per_sec: u64`（每秒订单数上限）。

检查顺序（固定优先级，先命中先返回）：

1. **名义金额**：`|(price as i128)| × (qty as i128) > max_notional`
   → `Err("notional_exceeded")`。**必须用 i128 计算**，杜绝 `i64` 溢出误判；
2. **单笔数量**：`qty > max_qty` → `Err("qty_exceeded")`；
3. **速率**：当前 1 秒窗口内已放行数 `≥ max_orders_per_sec`
   → `Err("rate_exceeded")`；通过则计数 +1。

速率窗口算法（零堆分配，两个 `u64` 字段）：

```
if order.ts_ns.wrapping_sub(window_start_ns) >= 1_000_000_000 {
    window_start_ns = order.ts_ns;
    orders_in_window = 0;
}
```

- 调用方（管线）负责在 `check_order` 前把 `OrderIntent.ts_ns` 打上当前时间戳；
- 窗口按**订单时间戳**滚动，不是按墙钟——回测重放历史数据时语义正确。

## 2. 测试映射

| 契约条目 | 测试 |
|----------|------|
| §1.2 永不拦截 | `pass_through_never_rejects`（极端值） |
| §1.3 原因码 | `rejects_notional_and_qty_with_documented_codes` |
| §1.3 速率/窗口重置 | `rate_limit_and_window_reset` |
| §1.3 i128 防溢出 | `notional_uses_i128_no_overflow` |
| §1.1 `&'static str` | `all_reject_codes_are_static_strs` |
