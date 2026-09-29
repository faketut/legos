# legos-testnet SPEC

> Status: draft → **locked**
> Scope: 0 成本实盘验证积木——Binance Testnet 免费 WS 行情 + REST 下单（虚拟资金）。
> 本 crate 允许网络 I/O（读线程阻塞读 WS、`ureq` 同步 REST）；
> workspace 其余 crate 保持零网络依赖。

## 1. Crate 结构

```text
legos-testnet/
├── src/lib.rs        # 导出 + TestnetWsFeed ↔ CsvFileFeed spec 对齐测试
├── src/ws.rs         # TestnetWsFeed：实现 MarketDataFeed
├── src/rest.rs       # TestnetRestGateway：实现 ExecutionGateway
└── src/bin/paper_trade.rs  # 演示二进制：实盘积木替换泛型参数
```

## 2. 数据刻度契约（与 scripts/fetch_ticks.py 严格一致）

| 字段  | 刻度 | 说明 |
|-------|------|------|
| `price` | `1 tick = 1e-8` 计价货币 | `floor(price_f64 * 1e8 + 0.5)`（非负值统一舍入） |
| `qty`   | `1 = 1e-8` 基础货币 | 同上；超出 `u32` 范围**饱和**为 `u32::MAX` |
| `side`  | resting side | Binance `m`（buyer is maker）：`true → Bid`（买方挂单被吃），`false → Ask`；与 ITCH「side = 被吃掉的挂单方向」语义一致 |
| `kind`  | 固定 `Trade` | WS `@trade` 流只产生 Trade 事件 |
| `ts_ns` | 纳秒 | Binance `T`（毫秒）× 1e6 |
| `order_id` | u32 | Binance `t`（trade id）**截断低 32 位**（同 ITCH 的 u64→u32 截断） |

`MarketTick` 仍为 32 字节、`flags` bit-mask 与 `legos-core/SPEC.md` 一致；
保留位必须为 0（对齐测试断言 `reserved_bits_clear()`）。

## 3. TestnetWsFeed（`ws.rs`）

### 3.1 输入

Binance `@trade` 流消息（testnet `wss://testnet.binance.vision/ws/<sym>@trade`
或主网 `wss://stream.binance.com:9443/ws/<sym>@trade`，**均免费、无需 key**）：

```json
{"e":"trade","s":"BTCUSDT","t":424242,"p":"97500.12","q":"0.5","T":1727486400000,"m":true}
```

`parse_trade_message(symbol_id, text) -> Option<MarketTick>` 为纯函数：
- `e != "trade"`、字段缺失、JSON 非法、`price <= 0`、`qty < 0`、
  非有限浮点 → `None`（跳过，不中断流）；
- 解析成功 → 按 §2 刻度构造 `MarketTick`。

### 3.2 连接语义

- `connect(url, symbol_id)` / `binance_testnet(symbol, symbol_id)` /
  `binance_mainnet(symbol, symbol_id)`：构造时立即建连；
- 读线程阻塞读 WS，文本消息经 `mpsc` 转发；`next_event` 用
  `recv_timeout(1s)` 取消息：
  - 超时 → `None`（暂时无数据，符合 `MarketDataFeed` 契约）；
  - 坏消息 → 跳过，`parse_errors += 1`；
  - 读线程退出（断线）→ `rx = None`、`reconnects += 1`，返回 `None`，
    **下次调用自动重连**；
- 监控计数器：`messages` / `parse_errors` / `reconnects`（`pub` 字段）。

## 4. TestnetRestGateway（`rest.rs`）

### 4.1 认证（硬性规定）

- key **只从环境变量**读取：`BINANCE_TESTNET_API_KEY` /
  `BINANCE_TESTNET_API_SECRET`；缺失或为空 → `new` 返回 `Err(String)`
  （错误信息指引用户去 README 免费申请），**绝不 panic、绝不硬编码**；
- 请求：`POST {base}/api/v3/order?{query}&signature={sig}`，
  头 `X-MBX-APIKEY`；`base` 默认 `https://testnet.binance.vision`。

### 4.2 签名（纯函数，可单测）

`signature = hex(HMAC_SHA256(query_string, api_secret))`，
以 RFC 4231 Test Case 1 向量锁定实现正确性。

### 4.3 查询串构造（纯函数，可单测）

```text
symbol=BTCUSDT&side=BUY&type=LIMIT&timeInForce=GTC
  &quantity=0.5&price=97500.12&timestamp=1727486400000&recvWindow=5000
```

- `price == 0` 的 intent → `type=MARKET`（不带 `price`）；
- `qty == 0` / `symbol_id` 不符 / 限价 `price < 0` → 本地直接 Rejected，
  不发请求（`last_error` 记录原因）；
- 数量/价格用 `fmt_scaled`（1e-8 → 去尾零小数字符串，如 `"0.5"`）。

### 4.4 响应解析（纯函数，可单测）

Binance `status` → `AckStatus`：`NEW`→Accepted，
`PARTIALLY_FILLED`→PartiallyFilled，`FILLED`→Filled；
错误体（含 `code`）、未知 status、非法 JSON、传输失败 → Rejected。
`executedQty`（字符串）→ `filled_qty`（1e-8 刻度 u32）；
`avg_price = floor(cummulativeQuoteQty / executedQty * 1e8)`（未成交为 0）。

### 4.5 可观测性

- `orders_sent: u64`（`pub`）；`last_error() -> Option<&str>`；
- `send_order` 永不 panic；HTTP 4xx 时解析 Binance 错误体后返回 Rejected。

## 5. 对齐测试（`lib.rs::conformance_tests`）

同一逻辑 trade 经 WS JSON 与经 CSV 行解析，`MarketTick` 必须逐字段一致；
`order_id` 豁免（WS = Binance trade id，CSV = 0——与 ITCH 对齐测试相同口径）。

## 6. paper_trade 二进制

`cargo run -p legos-testnet --bin paper_trade -- [SYMBOL] [MAX_TICKS]`
演示把泛型参数换成实盘积木：

```text
TestnetWsFeed → L2FlatArrayBook → MarketMakerStrategy
    → HardLimitRisk → TestnetRestGateway
```

- 缺 key 时 exit(2) 并打印免费申请步骤；
- 每单后 sleep 300ms（testnet 限流保护）；
- 结束打印 ticks/sent/rejected/feed 计数器。
- **这是 testnet 真实下单（虚拟资金），非 dry-run。**

## 7. 非目标

- 不实现重连退避（demo 级别：立即重连）；
- 不实现 OKX testnet（Binance testnet 已覆盖 0 成本需求）；
- 不做资金/持仓对账（paper-trading 压力测试只验证链路与延迟）。
