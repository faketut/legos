# SPEC — legos-app（泛型管线与基准）

> Phase 4 交付物。本文是管线与基准的**前置规格**：先写 spec，再（已）实现；
> 代码若与本文冲突，以本文为准做重构。
> 测试映射：`src/main.rs` 的 `#[cfg(test)]`；基准见 `benches/pipeline_bench.rs`。

## 1. 行为契约（Behavior Spec）

### 1.1 Pipeline 数据流

```
feed.next_event() → bus.push() → bus.pop() → book.apply()
    → strategy.on_tick() → risk.check_order() → gateway.send_order()
```

- `Pipeline<F, B, Bk, S, R, G>` 六类型参数**全泛型**（无 `dyn`），
  单态化后逐层内联，零虚函数跳转；
- `B: MessageBus<Item = MarketTick>`；
- 每个 tick 的处理（`on_tick`）：
  1. `book.apply(&ev)`；
  2. 取 `best_bid / best_ask / mid_price`；
  3. `gateway.on_quote(ev.symbol_id, bid, ask)`（Sim 刷新盘口；FIX 忽略）；
  4. `strategy.on_tick(bid, ask, mid)` → `Some(intent)` 则：
     - `intent.ts_ns = now_ns()`（管线统一打戳，供风控速率窗口）；
     - 创建 `TrackedOrder::new(client_order_id)`（`Created`）；
     - `risk.check_order(&intent)`：
       `Ok` → `advance(PendingNew)` → `gateway.send_order` →
       按回执 `advance(Accepted | Filled | PartiallyFilled | Rejected)`；
       `Err` → `rejected_by_risk += 1`，`advance(Rejected)`，
       **订单不得送网关**；
     - 任何 `advance` 返回 `Err` → `illegal_transitions += 1`
       （**契约要求恒为 0**；非 0 即实现 bug）；
- 同步管线语义：订单在一个 tick 内走完可观测生命周期；
  `Accepted` 为该 tick 的观察终点（实盘中后续由成交回报继续推进，
  见 `legos-core/SPEC.md` §2.2）。

### 1.2 run() 终止规则

- `pump_once`：先把 feed 泵入 bus（bus 满则停，避免覆盖），
  再把 bus 排空走完链路；返回本轮是否有进展；
- `run()`：连续 **3 轮无进展**（feed 耗尽且 bus 排空）后返回 `PipelineStats`；
- `run_forever()`：永不返回；feed 的 `None` 只视为暂时无数据（实盘常驻）。

### 1.3 CPU 亲和性

- `main` 启动时把主循环绑到第一个可用核心
  （`core_affinity::set_for_current`），避免跨核迁移抖动；
- 绑定失败**不中断运行**（降级为普通调度，仅打日志）。

### 1.4 换积木演示

`_demo_swap_bricks_at_compile_time`：只改泛型参数即把
`L2FlatArrayBook<10> + PassThroughRisk + SimulatedExchange` 换成
`L3MapBook + HardLimitRisk + FixProtocolGateway`；
函数不被调用但必须通过编译——证明换积木零逻辑改动。

### 1.5 PipelineStats 字段

`ticks`（走完链路的 tick 数）、`intents`（策略意图数）、`sent`（送网关数）、
`rejected_by_risk`、`rejected_by_gateway`、`fills`（Filled + PartiallyFilled）、
`illegal_transitions`（恒为 0）。

## 2. 基准契约（criterion）

`cargo bench -p legos-app`（release）。三组基准：

| 基准 | 测量 | 契约 |
|------|------|------|
| `spsc_ring_buffer/push_pop_tick` | 单次 push+pop | `Throughput::Elements(1)` |
| `l2_book/apply_tick` | 单次 `apply` | 10k tick 循环复用，`Elements(1)` |
| `full_pipeline/tick_to_fill_1000` | 1000 tick 端到端 | 每次 `b.iter` **重建整条管线**（不得复用账簿/网关状态）；`Elements(1000)` |

反作弊规则：

- `black_box` 包裹输入与输出（`fills`），禁止编译器把链路优化为空；
- `tick_to_fill_1000` 内断言 `fills > 0`——基准必须走**真实成交路径**，
  退化为空转时直接失败；
- 基准内联的管线装配必须与 `main.rs` 的 `Pipeline::on_tick` **逻辑一致**
  （feed→bus→book→strategy→risk→gateway，含 `on_quote` 盘口刷新）。

## 3. 测试映射

| 契约条目 | 测试 |
|----------|------|
| §1.1 全链路/统计 | `pipeline_end_to_end_backtest`（含 `illegal_transitions == 0`） |
| §1.1 风控调用次数 | `risk_called_exactly_once_per_intent`（Mock `CountingRisk`） |
| §1.1 拦截短路网关 | `risk_rejection_short_circuits_gateway`（`sent == 0`） |
| §1.1 硬风控 | `pipeline_with_hard_risk_blocks_oversize` |
| §1.4 换账簿 | `pipeline_with_l3_book_compiles_and_runs` |
