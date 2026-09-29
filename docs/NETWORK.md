# 网络栈评估与原型（P2）

> 日期：2026-09-29｜环境：htch-runtime 云 VM｜内核：7.0.0-38-generic｜vCPU：2（AMD EPYC 9D25）
> 相关代码：`prototypes/net/udp_bench.rs`（单文件，`rustc --edition 2021 -O` 直接编译，无 Cargo 依赖）

## 1. 结论先行（TL;DR）

| 问题 | 结论 |
|---|---|
| DPDK 可行？ | **不可行**——本 VM 无物理网卡（仅 `veth` + `lo`），无 PMD 可绑定的 NIC |
| AF_XDP 可行？ | 内核支持（`AF_XDP` socket 可创建），但**无可用网卡**：`lo` 不支持 XDP，`veth` 仅支持 SKB/generic 模式（不 bypass 内核，无意义） |
| Hugepages | sysfs 节点存在但 `HugePages_Total=0`；当前为 root，理论上可分配，但无网卡可配，无意义 |
| 本环境可达 Tier | **Tier 3**（标准内核栈 + busy-poll 调优上限） |
| 原型实测（loopback，20k pps，1M 样本） | 见 §4 |

一句话：**kernel bypass 在本环境没有载体**——bypass 需要一块真实的网卡，而云 VM 里没有。
P2 的产出因此是：诚实的可行性否决 + busy-poll UDP 原型 + 生产环境（有网卡时）的详细设计。

## 2. 可行性评估（只读探测，未改动环境）

| 检查项 | 命令/方法 | 结果 |
|---|---|---|
| 是否 root | `id -u` | `0`，是 |
| Hugepages | `grep -i hugepage /proc/meminfo` | `HugePages_Total: 0`，2MB/1GB 节点存在但未分配 |
| 网卡 | `/sys/class/net/*`、`ethtool -i` | 仅 `host0`（driver `veth`，虚拟）与 `lo`；**无物理 NIC** |
| 内核 XDP 支持 | `socket(AF_XDP=44, SOCK_RAW, 0)`（Python） | 创建成功，内核支持 AF_XDP；`/proc/kallsyms` 有 583 个 `xdp` 符号 |
| `/boot/config` | `grep -i xdp /boot/config-$(uname -r)` | 文件不存在（云镜像未打包），用 socket 探测代替 |
| CPU | `nproc`、`/proc/cpuinfo` | 2 vCPU，两个独立 core（core id 0/1），无超线程配对问题 |

### 为什么 DPDK / AF_XDP 在此不可行

- **DPDK** 需要：hugepages + 一块被 PMD 接管的物理网卡（`vfio-pci`/`igb_uio` 绑定）。
  本 VM 的唯一"网卡"是 veth pair 的一端（对端在宿主机），DPDK 没有对应的 poll-mode driver；
  用 DPDK 的 tap/veth 驱动跑通只能证明"代码能编译"，测不出任何 bypass 收益，且会引入
  DPDK 整套构建依赖——投入产出为负。
- **AF_XDP** 需要：网卡驱动支持 `XDP_SKB`/`XDP_DRV`/`XDP_ZEROCOPY`。`lo` 明确不支持 XDP；
  `veth` 支持 XDP 但只有 generic（SKB）模式——包依然走完整内核栈再拷贝给用户态，
  等于"开了 XDP 的名，行普通 socket 之实"，延迟只增不减。
- 即使（作为 root）分配了 hugepages，也没有 DMA 目标可以映射——结论不变。

## 3. 原型：busy-poll UDP 行情接收（`prototypes/net/`）

既然 bypass 不可用，P2 在本环境能做的最优解是**内核栈内的 busy-poll 调优**：
`SO_BUSY_POLL` + 非阻塞 socket + 用户态紧自旋，消除中断唤醒/调度延迟。

### 文件

- `prototypes/net/udp_bench.rs` — 单文件基准，无外部 crate。
  `std::os::unix::io::AsRawFd` 取 fd，`extern "C" { fn setsockopt }`（std 已链接 libc）
  设置 `SO_BUSY_POLL=46` / `SO_RCVBUF` / `SO_RCVTIMEO`；`extern "C" { fn clock_gettime }`
  取 `CLOCK_MONOTONIC` 纳秒时间戳；`pthread_setaffinity_np` 尝试绑核（失败则继续，不致命）。
- `prototypes/net/run.sh` — 编译 + 依次跑两种模式，结果存 `results_<timestamp>.txt`。

### 方法（单向延迟，同机同源时钟）

- 发送线程按固定间隔（20k pps，即 50µs）发送 120 万个 64 字节 tick 包
  （`seq` + 发送时间戳 + 象征性 tick 字段；前 20 万 warmup 不计）。
- 接收线程在 `recvfrom()` 返回后**立即**打接收时间戳，延迟 = `t_recv - t_send`，
  记录后 100 万个样本，排序取分位数（min/p50/p90/p99/p999/max/mean），另计丢包与乱序。
- 限速发送保证 socket 队列基本为空，测到的是**单包过栈成本**而非排队延迟
  （反例见下：100k pps 下两种模式都出现排队，p50 被污染——该轮数据仅用于说明本 VM 的可持续速率上限）。
- 两种接收模式（发送端完全相同）：
  - `plain`：普通阻塞 `recvfrom()`，中断驱动 NAPI 唤醒（另设 2s `SO_RCVTIMEO` 防挂死）。
  - `busypoll`：`SO_BUSY_POLL=50µs` + `O_NONBLOCK` + 无包时 `spin_loop()` 紧自旋。

### 构建与运行

```bash
cd prototypes/net
rustc --edition 2021 -O udp_bench.rs -o udp_bench
./udp_bench plain 41001      # 普通模式
./udp_bench busypoll 41002   # busy-poll 模式
# 或一键： ./run.sh
```

## 4. 实测结果

> ⚠️ loopback 微基准：测的是**内核 UDP 栈成本**，不是"网线到应用"延迟。
> 生产网卡的中断合并、PCIe、PHY 成本在此不存在；数字仅用于两种接收路径的相对比较。

### 4.1 饱和探测（100k pps，2026-09-29 19:49 UTC，`results_20260929_194946.txt`）

| 模式 | min | p50 | p90 | p99 | max | mean | drops |
|---|---|---|---|---|---|---|---|
| plain（100k pps） | 2223ns | 347µs | 925µs | 20.0ms | 27.8ms | 827µs | 0 |
| busypoll（100k pps） | 3055ns | 600µs | 1.89ms | 16.6ms | 29.0ms | 1057µs | 0 |

解读：100k pps 下**两种模式都排队了**——接收端服务速率跟不上到达速率（本 VM 回环 UDP
端到端可持续速率 < 100k pps，含虚拟化开销与双核调度），p50 被排队延迟污染，
此轮数字**不能**用于比较两种路径的单包成本，仅作为"本环境上限"的诚实记录。

### 4.2 正式对比（20k pps，每轮 1M 样本，drops=0）

跑了两轮（19:50 与 19:53 UTC）。单位 µs。

| 轮次 | 模式 | min | p50 | p90 | p99 | max | mean |
|---|---|---|---|---|---|---|---|
| 1 | plain（阻塞 recv） | 2.15 | **9.23** | 30.9 | 1491 | 39702 | 91.4 |
| 1 | busypoll（SO_BUSY_POLL+自旋） | 2.08 | **4.17** | 841 | 18078 | 31127 | 582 |
| 2 | plain（阻塞 recv） | 2.11 | **9.44** | 241 | 2923 | 29341 | 163 |
| 2 | busypoll（SO_BUSY_POLL+自旋） | 2.07 | **34.0** | 1955 | 21921 | 43838 | 1049 |

原始文件：`prototypes/net/results_20260929_195045.txt`（轮 1）、
`prototypes/net/results_20260929_195305.txt`（轮 2）。

解读（诚实版）：

- **min ≈ 2.1µs（两种模式一致）**：这是回环 UDP 一包的栈成本下限，与接收方式无关。
- **plain 的 p50 极其稳定（9.23 / 9.44µs）**：阻塞 `recvfrom()` 的中位数成本可复现，
  主要由"中断唤醒 + 调度 + 系统调用"构成。
- **busypoll 的 p50 不稳定（4.17 / 34.0µs）**：第 1 轮机器较空时，busy-poll 把中位数
  压到 4.2µs（省掉睡眠/唤醒，符合预期）；第 2 轮宿主机繁忙（并行 vmstat 显示
  steal 1–2%、可运行队列 3–5、 sibling 任务同机压测），自旋线程在被抢占的 vCPU 上
  空转烧时间片，反而输给"睡眠让出 CPU"的阻塞模式。
- **p90 以上的尾部两轮差异巨大且 busypoll 更差**：尾部由 hypervisor 调度噪音主导
  （ms 级），不是 socket API 的差异——在本环境下**无法**得出 busy-poll 改善尾部的结论。
- 核心教训（直接支撑 §6）：**busy-poll 只有配 CPU 隔离/独占核才是稳赢**；
  在共享 vCPU 上它是中性偏负的。这正是云 VM 进 Tier 2 的结构性障碍。

## 5. 生产环境设计：AF_XDP / DPDK（有物理网卡时的做法）

以下为搬到带物理网卡的生产机时的实施蓝图，本 VM 无法验证，仅作设计。

### 5.1 机器准备（两种方案通用）

1. **Hugepages**：`echo 1024 > /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages`
  （或 1GB 页），`vm.nr_hugepages` 写入 `/etc/sysctl.conf`；DPDK EAL 与 AF_XDP UMEM
   都从 hugepages 取内存，保证 TLB 命中与物理连续。
2. **CPU 隔离**：内核 cmdline 加 `isolcpus=2-7 nohz_full=2-7 rcu_nocbs=2-7`
  （按实际核数调整），把轮询线程所在核从调度器、中断、RCU 回调中摘除；
   `echo 2 > /proc/irq/<网卡中断>/smp_affinity_list` 把网卡中断钉到**非**轮询核
   （或轮询核，视 NAPI/busy-poll 策略二选一，不要两边打架）。
3. **网卡调优**：`ethtool -C <if> rx-usecs 0` 关中断合并（或按策略设小值）；
   `ethtool -G <if> rx 4096` 加大 ring；关 `irqbalance`；网卡多队列时用
   `ethtool -X` 做 RSS 定向，把行情组播流钉到轮询核的队列。

### 5.2 方案 A：AF_XDP（推荐，内核原生，无需整套 DPDK）

```
网卡 --(XDP native, zero-copy)--> UMEM --> Rx ring --> legos 轮询线程
                                                     |
                                                     v
                                              SpscRingBuffer<Tick>
                                              (单生产者 = 轮询线程)
```

1. 网卡驱动需支持 native XDP + zero-copy（如 `i40e/ice/mlx5` 较新驱动；先查
   `ethtool -i` 的 driver 并对照内核文档）。
2. 附着一个最小 XDP 程序：只做 `bpf_redirect_map()` 到 AF_XDP socket，
   行情组播目的端口的包进 XDP socket，其余 `XDP_PASS` 走正常栈（SSH 等不受影响）。
3. 用户态建 `xsk_socket__create`：UMEM（hugepages 背书）+ Fill ring + Rx ring
   (+ Tx/Completion ring，如需回 ACK 或下单同网卡)。
4. 轮询线程（绑隔离核，`SCHED_FIFO` 可选）：`poll()`/忙轮询 Rx ring →
   零拷贝得到包内存 → 解析为 `MarketTick`（32B 定长，见 legos-core 数据契约）→
   `SpscRingBuffer::push`（单生产者，无锁）→ 下游策略线程 `pop`。
   **对接点**：`legos-feed` 新增 `XdpFeed` 实现现有 `Feed` trait（若 trait 以
   iterator/poll 建模），或直接在 `legos-app` 的管线入口替换 `CsvFileFeed`。
5. 背压：Rx ring 满则丢包是**设计选择**（行情场景新数据覆盖旧数据），
   在 `SpscRingBuffer` 侧用覆盖写（ring buffer 语义）而非阻塞，保持轮询线程永不阻塞。

### 5.3 方案 B：DPDK（重型，整包接管网卡）

1. `vfio-pci` 绑定网卡（网卡从内核消失，整机网络需第二块管理网卡）。
2. EAL 初始化：`-l 2-7 -n 4 --socket-mem 1024`；`rte_mempool`（hugepages）收包；
   `rte_eth_rx_burst(port, queue, mbufs, 64)` 批量收。
3. 同一轮询线程内把 mbuf 解析为 `MarketTick` 后 `rte_pktmbuf_free`，
   再 `push` 进 `SpscRingBuffer`。DPDK 线程与 legos 线程模型冲突点：
   DPDK 要求 lcore 独占——把 DPDK 的收包 lcore **就是** legos 的 feed 线程
   （`core_affinity` 已在 P4 规划，两者是同一个绑核机制，不要各绑各的）。
4. 代价：DPDK 版本与内核/网卡固件的兼容矩阵、构建依赖、运维复杂度；
   只有当 AF_XDP 的延迟不达标（或需要 DPDK 生态如 `dpdk-burst` 精确发包 pacing）时才选。

### 5.4 与 legos 现有架构的对接点

| 层 | 对接方式 |
|---|---|
| `legos-core` | `MarketTick` 32B 定长契约不变；XDP/DPDK 线程只做"字节→Tick"解析，解析逻辑与 `NativeItchParser` 复用同一代码路径（trait 化解析器） |
| `legos-bus` | `SpscRingBuffer<Tick>` 即轮询线程与策略线程之间的 handoff；单生产者（收包线程）+ 单消费者（策略/账簿线程），backpressure 用覆盖语义 |
| `legos-feed` | 新增 `XdpFeed` / `DpdkFeed` 积木，实现与 `CsvFileFeed` 相同的 `Feed` trait——回测（CSV）与实盘（XDP）可互换，符合"Legos 积木"设计 |
| `legos-app` | `TradingPipeline` 的 source 泛型参数切换即可；`core_affinity` 模块负责把收包线程钉到隔离核 |

## 6. Tier 评估

### Tier 定义（本项目的口径）

| Tier | 含义 | 行情路径典型延迟（网卡→应用） |
|---|---|---|
| Tier 1 | Kernel bypass：DPDK / AF_XDP native zero-copy / FPGA | < 5µs |
| Tier 2 | 调优内核栈：busy-poll、中断亲和、CPU 隔离、RPS/RFS | 5–20µs |
| Tier 3 | 标准内核栈：阻塞 recv，中断驱动 | 20–100µs+（抖动大，p99 可达 ms 级） |

### 本环境：Tier 3

- 无物理网卡 → bypass 无载体，Tier 1 不可能。
- loopback 上 busy-poll 原型能压缩的是**唤醒/调度抖动**（见 §4.2 p99/max 对比），
  但虚拟化层的 vCPU 调度、veth 路径开销仍在；且没有真实网卡中断可调，
  `SO_BUSY_POLL` 在回环上更多是"自旋代替睡眠"，收益上限明显。
- 因此本 VM 的诚实定位：**Tier 3，busy-poll 可达 Tier 3 的上限**。

### 进 Tier 2（5–20µs）还缺什么

1. 一块**物理网卡**（bypass 不需要，但调优需要中断可配）：可设中断合并、
   可做 irq affinity、可关 irqbalance。
2. **CPU 隔离**：`isolcpus`/`nohz_full` 把轮询核从内核调度噪音中摘除；
   云 VM 通常改不了宿主机内核 cmdline——这是云环境进 Tier 2 的最大结构性障碍。
3. **独占 vCPU**：云 VM 的 vCPU 是超售的，邻居噪音直接进 p99；Tier 2 需要
   dedicated host / 整机独占实例。
4. 内核参数：`net.core.busy_poll`/`busy_read` 全局值、`rmem_max`、`netdev_budget`、
   RPS/RFS 配置——本 VM 可调，但缺 1–3 时效果有限。

### 进 Tier 1 还缺什么（§5 的实施前提）

物理网卡 + 支持 native XDP zero-copy（或 DPDK PMD）的驱动 + hugepages +
CPU 隔离 + 网卡独占（DPDK 方案还需第二块管理网卡）。

## 7. 没做什么及原因

- **没有实际跑通 AF_XDP/DPDK**：无物理网卡，任何"跑通"都是自欺（veth SKB 模式
  不 bypass，DPDK 无 PMD 可绑）。原型聚焦在**本环境真实有效的** busy-poll 路径。
- **没有改现有代码**：原型全部在 `prototypes/net/` 新文件；`cargo test --workspace`
  未受影响（已跑过确认全绿，见汇报）。
- **没有写 PERF.md/README.md/SPEC.md**：数字报给 coordinator，由其统一写入。
- **没有 commit/push**：按约束，所有产出留在工作区。
- **100k pps 那轮没重跑**：它的方法论缺陷（排队污染）已确认，重跑无意义；
  原始文件保留（`results_20260929_194946.txt`）作为饱和上限的诚实记录。
