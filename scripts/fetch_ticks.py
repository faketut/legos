#!/usr/bin/env python3
"""0 成本抓取加密交易所公开 Tick 数据，转为 legos CSV schema。

数据源（全部免费、无需 key）：
  1. data.binance.vision 日度 aggTrades zip（默认）：整天数据一次下载，
     无需 API key、无频率限制之忧；
  2. --source okx：OKX 公开 /api/v5/market/history-trades（仅近期数据，
     按 tradeId 向前分页，最多 --max-trades 笔）。

输出列与 legos-feed CsvFileFeed 的解析 schema 严格一致
（见 legos-feed/SPEC.md「CSV 数据契约」）：

    symbol_id,side,price,qty,kind,ts_ns

刻度契约（与 legos-testnet 的 TestnetWsFeed 严格一致）：
  * price：1 tick = 1e-8 计价货币，price_ticks = floor(price * 1e8 + 0.5)
  * qty  ：1 = 1e-8 基础货币，超出 u32 范围饱和为 4294967295
  * side ：Binance is_buyer_maker=True -> B（买方挂单被吃）；
           OKX taker side=buy -> A（taker 吃掉卖方挂单），sell -> B
  * kind ：固定 TRADE；ts_ns：毫秒时间戳 * 1e6

脚本跑完打印一行校验：行数、坏行数、字段、时间范围。
"""

import argparse
import csv
import datetime as dt
import io
import json
import sys
import time
import urllib.request
import zipfile

PRICE_SCALE = 100_000_000
QTY_SCALE = 100_000_000
U32_MAX = 4294967295
HEADER = ["symbol_id", "side", "price", "qty", "kind", "ts_ns"]


def to_ticks(x: float, scale: int) -> int:
    """非负浮点 -> 整数刻度（floor(x + 0.5)，与 Rust 端一致）。"""
    return int(x * scale + 0.5)


def http_get(url: str, retries: int = 3) -> bytes:
    last = None
    for i in range(retries):
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "legos-fetch-ticks/0.1"})
            with urllib.request.urlopen(req, timeout=60) as r:
                return r.read()
        except Exception as e:  # noqa: BLE001 - 重试所有网络错误
            last = e
            time.sleep(2 ** i)
    raise RuntimeError(f"GET {url} 失败（重试 {retries} 次）: {last}")


def fetch_binance_vision(symbol: str, date: str):
    """下载 data.binance.vision 日度 aggTrades zip，逐行产出 (price, qty, is_buyer_maker, ts_ms)。"""
    sym = symbol.upper()
    url = (
        f"https://data.binance.vision/data/spot/daily/aggTrades/"
        f"{sym}/{sym}-aggTrades-{date}.zip"
    )
    print(f"[1/3] 下载 {url}", flush=True)
    try:
        data = http_get(url)
    except RuntimeError as e:
        raise RuntimeError(
            f"{e}\n提示：data.binance.vision 的日 zip 一般在 UTC 次日生成，"
            f"请用昨天的日期（如 --date {(dt.date.today() - dt.timedelta(days=1)).isoformat()}）"
        ) from e
    print(f"[2/3] 下载完成（{len(data) / 1e6:.1f} MB），解压转换中…", flush=True)
    zf = zipfile.ZipFile(io.BytesIO(data))
    names = zf.namelist()
    if len(names) != 1:
        raise RuntimeError(f"zip 内文件数异常：{names}")
    bad = 0
    with zf.open(names[0]) as f:
        # 内层列（spot aggTrades）：
        # trade_id,price,quantity,first_trade_id,last_trade_id,transact_time,is_buyer_maker[,is_best_match]
        # 注意：transact_time 可能是毫秒（13 位）或微秒（16 位），按量级归一化到纳秒。
        reader = csv.reader(io.TextIOWrapper(f))
        for row in reader:
            if len(row) not in (7, 8):
                bad += 1
                continue
            try:
                price = float(row[1])
                qty = float(row[2])
                ts_raw = int(row[5])
                is_buyer_maker = row[6].strip().lower() == "true"
            except ValueError:
                bad += 1
                continue
            if not (price > 0 and qty >= 0):
                bad += 1
                continue
            if ts_raw >= 1_000_000_000_000_000:  # 微秒
                ts_ms = ts_raw // 1000
            else:  # 毫秒
                ts_ms = ts_raw
            yield price, qty, is_buyer_maker, ts_ms
    if bad:
        print(f"  跳过坏行 {bad} 行", flush=True)


def fetch_okx(inst_id: str, max_trades: int):
    """OKX 公开 history-trades（无需 key），按 tradeId 向前分页。"""
    base = "https://www.okx.com/api/v5/market/history-trades"
    after = None
    got = 0
    bad = 0
    while got < max_trades:
        url = f"{base}?instId={inst_id}&limit=100"
        if after:
            url += f"&after={after}"
        raw = http_get(url)
        payload = json.loads(raw.decode())
        if payload.get("code") != "0":
            raise RuntimeError(f"OKX 返回错误：{payload}")
        data = payload.get("data", [])
        if not data:
            break
        for t in data:
            try:
                price = float(t["px"])
                qty = float(t["sz"])
                ts_ms = int(t["ts"])
                taker_side = t["side"]  # taker 方向：buy=主动买，吃卖方挂单
            except (KeyError, ValueError):
                bad += 1
                continue
            if not (price > 0 and qty >= 0):
                bad += 1
                continue
            # taker buy -> resting Ask 被吃 -> A；taker sell -> B
            is_buyer_maker = taker_side != "buy"
            yield price, qty, is_buyer_maker, ts_ms
            got += 1
            if got >= max_trades:
                break
        after = data[-1]["tradeId"]
        time.sleep(0.15)  # 礼貌限速
    if bad:
        print(f"  跳过坏行 {bad} 行", flush=True)


def main() -> int:
    ap = argparse.ArgumentParser(description="0 成本抓取公开 Tick 数据，转 legos CSV")
    ap.add_argument("--symbol", default="BTCUSDT", help="Binance 交易对（默认 BTCUSDT）")
    ap.add_argument(
        "--date",
        default=(dt.date.today() - dt.timedelta(days=1)).isoformat(),
        help="日期 YYYY-MM-DD（默认昨天 UTC；仅 binance-vision 源有效）",
    )
    ap.add_argument("--out", required=True, help="输出 CSV 路径")
    ap.add_argument("--symbol-id", type=int, default=1, help="symbol_id（默认 1）")
    ap.add_argument(
        "--source",
        choices=["auto", "binance-vision", "okx"],
        default="auto",
        help="数据源（默认 auto = binance-vision，失败时提示改用 okx）",
    )
    ap.add_argument("--max-trades", type=int, default=0, help="最多抓取笔数（0=不限；okx 源建议设）")
    args = ap.parse_args()

    source = args.source
    if source == "auto":
        source = "binance-vision"

    if source == "binance-vision":
        rows = fetch_binance_vision(args.symbol, args.date)
    else:
        inst_id = args.symbol.upper().replace("USDT", "-USDT", 1) if "-" not in args.symbol.upper() else args.symbol.upper()
        # BTCUSDT -> BTC-USDT；已经带横线的保持原样
        if "-" not in inst_id:
            inst_id = args.symbol.upper()[:-4] + "-USDT"
        rows = fetch_okx(inst_id, args.max_trades or 10_000)

    print(f"[3/3] 写入 {args.out}", flush=True)
    n = 0
    ts_min = None
    ts_max = None
    with open(args.out, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(HEADER)
        for price, qty, is_buyer_maker, ts_ms in rows:
            if args.max_trades and n >= args.max_trades:
                break
            side = "B" if is_buyer_maker else "A"
            p = to_ticks(price, PRICE_SCALE)
            q = min(to_ticks(qty, QTY_SCALE), U32_MAX)
            ts_ns = ts_ms * 1_000_000
            w.writerow([args.symbol_id, side, p, q, "TRADE", ts_ns])
            n += 1
            ts_min = ts_ms if ts_min is None or ts_ms < ts_min else ts_min
            ts_max = ts_ms if ts_max is None or ts_ms > ts_max else ts_max
            if n % 200_000 == 0:
                print(f"  …已写 {n} 行", flush=True)

    # 校验行：行数、字段、时间范围
    if n == 0:
        print("FAIL rows=0：没有抓到任何数据", flush=True)
        return 1
    t0 = dt.datetime.fromtimestamp(ts_min / 1000, tz=dt.timezone.utc).isoformat()
    t1 = dt.datetime.fromtimestamp(ts_max / 1000, tz=dt.timezone.utc).isoformat()
    print(
        f"OK rows={n} cols={','.join(HEADER)} "
        f"ts_range=[{t0} .. {t1}] source={source} symbol={args.symbol.upper()}",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
