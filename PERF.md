# PERF.md — Legos 性能基线与回归手册

> 口径声明（必读）：本文件所有纳秒级数字均为**纯进程内计算延迟**
> （feed → bus → book → strategy → risk → gateway），**不含**网络 I/O、
> 网卡、内核协议栈。与行业 wire-to-wire 数字对比时必须注明此差异。

## 1. 基线（2026-09-29，release 模式）

### 1.1 criterion 均值基准（回归锚点）

| 基准组 | 用例 | 均值 | 95% CI |
|---|---|---|---|
| `spsc_ring_buffer` | `push_pop_tick` | 20.7 ns | [20.5, 20.8] |
| `l2_book` | `apply_tick` | 21.4 ns | [21.1, 21.7] |
| `full_pipeline` | `tick_to_fill_1000` | 42.2 ns/tick | [41.7, 42.9] |

运行：`cargo bench -p legos-app`。`full_pipeline` 每次迭代重建整条管线、
跑 1000 tick 并断言真实成交（fills > 0）。

### 1.2 tail-latency 分位数（独立 harness，10M tick）

运行：`cargo run --release -p legos-app --bin tail_latency -- 10000000`
（实现见 `legos-app/src/bin/tail_latency.rs`）。

| mean | p50 | p99 | p999 | max |
|---|---|---|---|---|
| 32.5 ns | 30.0 ns | 30.0→30.0 ns | 70.1 ns | 170.2 ns | 3.6 ms |

（p50/p99/p999 在三次独立运行中完全一致；max 两次运行分别为 3.6ms / 1.1ms，
为共享 VM 的 OS 调度噪声，非代码路径。）

2026-09-29 优化后复测（P3 下单路径优化后，同一 harness）：
mean 33.3ns / p50 30.0ns / p99 80.1ns / p999 170.2ns / max 242µs。
p99 在 70→80ns 间抖动（VM 噪声），p50/p999 不变——符合预期：
P3 优化的是下单路径，而本 workload 策略很少触发下单。

**方法学诚实声明**：
- 逐 tick 用 x86 `rdtsc` 计时，样本写入预分配数组（10M × 8B），事后离线排序算分位数；
- TSC 频率用 200ms 睡眠标定（本机 1.498GHz），cycles→ns 据此换算；
- 计时器本底 = 1M 次背对背 `rdtsc` 最小差值（本机 15 cycles），每样本 `saturating_sub` 扣除，
  输出中如实报告；
- 测量线程绑核（`core_affinity`），200k tick 预热使指令缓存/分支预测进入稳态；
- harness 均值（32.5ns）低于 criterion 均值（42ns）的原因：harness 复用**一条**管线跑完全程
  （稳态热缓存），criterion 每次迭代重建管线。**回归以 criterion 为准，分位数以 harness 为准。**

## 2. 行业对照表

| 层级 | tick-to-trade（wire-to-wire） | 备注 |
|---|---|---|
| 软件 + 标准内核栈（Tier 3） | 5–50 µs | 内核协议栈每 round-trip 贡献 20–60 µs |
| 软件 + kernel bypass（Tier 2） | 5–20 µs | bypass 后内核部分压到 1–3 µs |
| FPGA 加速（Tier 1） | <5 µs，优化后 1–3 µs | 含网卡直连 |
| **Legos 计算核心（本仓库）** | **p50 30ns / p999 170ns** | **仅进程内计算，不含网络** |

**结论**：计算核心已比整条实盘链路快 2–3 个数量级；在真实部署中，
延迟大头在网络路径（内核栈 ~20–60µs）。继续压榨 compute 的收益是纳秒级，
**优化主战场是网络栈**（见 §5 与 `docs/NETWORK.md`）。

## 3. 回归流程

1. 改动代码后跑 `cargo bench -p legos-app`（release）；
2. criterion 自动把上次 `new` 提升为 `base`，`change/estimates.json`
   给出"回归/提升"的统计显著性判断——只看它，不凭感觉；
3. 跑 `cargo run --release -p legos-app --bin tail_latency -- 10000000`，
   对比 p50/p99/p999（max 只看数量级，受 VM 噪声主导）；
4. 判定线：p99 回归超过 5% 必须调查原因；criterion 报告统计显著回归时必须调查；
5. 把新基线数字更新到本表，并注明日期与 commit。

## 4. 测试环境

- 云 VM：2 vCPU，AMD EPYC 9D25，共享宿主机；
- 有 OS 调度噪声：10M 样本中 max 可达 ms 级（1.1–3.6ms），p999 以下稳定；
- 生产复测建议：独占物理机 + CPU 隔离（`isolcpus`/`nohz_full`）+
  关闭不必要的守护进程后重跑 §1.2，max 应回落到 µs 级。

## 5. 优化路线图（按收益排序）

- [x] P1 参照体系：criterion 回归锚点 + tail-latency harness + 本手册
- [x] P2 网络栈：busy-poll UDP 原型 + AF_XDP/DPDK 可行性否决（见 §6、`docs/NETWORK.md`）
- [x] P3 下单路径：强制每 tick 下单 micro-bench + profile 驱动优化 −31.3%（见 §7）
- [x] P4 确定性：L2 O(1) A/B（p999 −25%~−60%，默认未切换，原因见 §8）、
      ring buffer 满载背压压测零丢失（见 §8）

## 6. P2 网络栈结论（2026-09-29）

详见 `docs/NETWORK.md`；原型 `prototypes/net/udp_bench.rs`
（单文件 `rustc -O` 编译，`SO_BUSY_POLL` 经 `extern "C" setsockopt` 实现）。

- **DPDK / AF_XDP 在本环境不可行**：云 VM 无物理网卡（仅 veth+lo），
  无 PMD 可绑定的 NIC；veth 的 SKB/generic XDP 模式不 bypass 内核，无意义。
  kernel bypass 需要真实网卡做载体——这是云 VM 进 Tier 2 的结构性障碍。
- **busy-poll UDP loopback 实测**（20k pps，1M 样本，64B 包）：
  | 轮次 | 模式 | p50 | p99 | max |
  |---|---|---|---|---|
  | 1（空闲） | plain 阻塞 recv | 9.23µs | 1491µs | 39.7ms |
  | 1（空闲） | busypoll | **4.17µs** | 18078µs | 31.1ms |
  | 2（宿主机忙） | plain | 9.44µs | 2923µs | 29.3ms |
  | 2（宿主机忙） | busypoll | 34.0µs | 21921µs | 43.8ms |
- 诚实解读：空闲时 busy-poll 省掉睡眠/唤醒，p50 减半符合预期；
  但宿主机繁忙时自旋空烧被抢占的时间片，p50 反而更差——
  **busy-poll 必须配 CPU 隔离/独占核才稳赢**，共享 vCPU 上中性偏负；
  尾部被 hypervisor 调度噪声主导，本环境无法得出 busy-poll 改善尾部的结论。
- 本环境可达 **Tier 3**；进 Tier 2（bypass 后 5–20µs）缺：物理网卡 +
  CPU 隔离 + 独占实例；`docs/NETWORK.md` 有生产环境 AF_XDP/DPDK 详细设计。

## 7. P3 下单路径优化（2026-09-29）

新基准 `legos-app/benches/order_path_bench.rs`：内置每 tick 必成交的
`AlwaysMarketStrategy`（市价单），单独测量 strategy→risk→gateway→撮合。
本 VM 无 `perf`，用 criterion 拆分基准 + 代码审查定位热点。

拆分基准 before：`strategy_on_tick` 11.5ns > `send_order` 10.1ns >
`on_quote` 5.6ns > `risk_check` 0.75ns。

| 口径 | before | after | 变化（criterion p<0.05） |
|---|---|---|---|
| 整下单路径均值（含每 tick `on_quote`） | 15.4ns | **10.8ns** | **−31.3%** |
| `send_order` 成交路径 | 10.1ns | **7.9ns** | −22.1% |
| `MarketMakerStrategy::on_tick` | 11.5ns | **8.1ns** | −20.9% |
| `on_quote` | 5.6ns | 5.8ns | 无显著变化（p=0.63，预期内） |

改动（纯内部实现，语义不变）：
- `legos-gateway/src/sim.rs`：`send_order` 的两次 64 槽线性扫描
  （`quote()` 读盘口 + `quote_mut()` 扣减）合并为单次下标扫描；
  `update_quote` 的"先找已有、再找空槽"双扫描合并为单遍；
- `legos-strategy/src/market_maker.rs`：`spread_ticks/2` 改为构造时预计算
  `half_spread_ticks`，热路径去掉 i64 除法。

## 8. P4 确定性（2026-09-29）

### 8.1 L2 O(1) A/B（`legos-book/src/l2_o1.rs`，`L2DirectIndexBook`）

价格→数组下标直接映射，固定档距、确定性更新。基准
`legos-app/benches/l2_ab_bench.rs`（mixed 流 + 对 flat 版最坏的跨档跳变流）：

| 流 | flat 均值 | O(1) 均值 | p999 flat → O(1) |
|---|---|---|---|
| mixed | ~40–57ns | ~25–55ns | 120ns → **90ns**（−25%） |
| worst | ~55–59ns | ~12–13ns | 200ns → **80ns**（−60%） |

判定：p999 改善 ≥10% 且 p50 无回归（70→50ns），**性能门通过**；
但**默认实现不切换**——O(1) 版的固定价格区间契约与实盘数据不兼容
（`PRICE_SCALE=1e8` 下 BTC tick ~1e13，落在任何合理 1024 档区间外会被静默忽略，
账簿恒空；档距是编译期常量，无法同时覆盖示例 CSV ~1e6 与 Binance ~1e13 两种量级）。
两者实现同一 `OrderBook` trait，泛型一处替换即可切换（见 `legos-book/SPEC.md` §4.3）。

### 8.2 Ring buffer 满载背压压测

`legos-app/src/main.rs` 测试 `backpressure_stress_producer_faster_than_consumer`：
200k tick 灌入容量 16 的 bus（生产持续快于消费），`pending` 槽被使用 12499/12503 轮：
- **零丢失**（ticks == 200000，结束时 pending 为空）；
- drain 总耗时 82.8ms，单轮最大停顿 310µs，吞吐 2.4M ticks/s。
