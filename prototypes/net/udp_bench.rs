// prototypes/net/udp_bench.rs
//
// Busy-poll UDP market-data receive benchmark (loopback).
//
// Compares two receive paths for the same paced UDP tick stream:
//   * `plain`    - stock kernel path: blocking recvfrom(), interrupt-driven NAPI.
//   * `busypoll` - SO_BUSY_POLL=50us + non-blocking socket + tight userspace spin.
//
// Single-file build, no Cargo changes, no external crates:
//     rustc --edition 2021 -O udp_bench.rs -o udp_bench
//     ./udp_bench plain 41001
//     ./udp_bench busypoll 41002
//
// Methodology (one-way latency, same monotonic clock both ends):
//   - sender thread paces 1.2M 64-byte tick packets at 100k pps over 127.0.0.1;
//     each packet carries the sender's CLOCK_MONOTONIC timestamp (ns).
//   - receiver thread timestamps immediately after recvfrom() returns and
//     records (t_recv - t_send) for the last 1M packets (200k warmup discarded).
//   - pacing keeps the socket queue ~empty, so the measured latency is the
//     per-packet stack cost, not queueing delay.
//   - FIN marker (seq = u64::MAX) terminates the receiver.
//
// This is a loopback micro-benchmark: it measures kernel UDP stack cost, NOT a
// NIC wire-to-app latency. It cannot demonstrate AF_XDP/DPDK (no physical NIC
// in this VM); see docs/NETWORK.md.

use std::net::UdpSocket;
use std::os::raw::{c_int, c_void};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ---- libc-level constants (declared manually; std already links libc) ----
const SOL_SOCKET: c_int = 1;
const SO_RCVBUF: c_int = 8;
const SO_SNDBUF: c_int = 7;
const SO_RCVTIMEO: c_int = 20;
const SO_BUSY_POLL: c_int = 46;
const CLOCK_MONOTONIC: c_int = 1;
const BUSY_POLL_USEC: c_int = 50;

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}
#[repr(C)]
struct Timeval {
    tv_sec: i64,
    tv_usec: i64,
}
#[repr(C)]
struct CpuSet {
    bits: [u64; 16],
}

extern "C" {
    fn clock_gettime(clk_id: c_int, tp: *mut Timespec) -> c_int;
    fn setsockopt(
        fd: c_int,
        level: c_int,
        optname: c_int,
        optval: *const c_void,
        optlen: u32,
    ) -> c_int;
    fn pthread_self() -> usize;
    fn pthread_setaffinity_np(thread: usize, cpusetsize: usize, cpuset: *const CpuSet) -> c_int;
}

fn now_ns() -> u64 {
    unsafe {
        let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
        clock_gettime(CLOCK_MONOTONIC, &mut ts);
        (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
    }
}

fn pin_to_cpu(cpu: usize) -> bool {
    let mut set = CpuSet { bits: [0u64; 16] };
    set.bits[cpu / 64] = 1u64 << (cpu % 64);
    unsafe {
        pthread_setaffinity_np(pthread_self(), std::mem::size_of::<CpuSet>(), &set) == 0
    }
}

fn set_int_opt(sock: &UdpSocket, name: c_int, val: c_int) -> bool {
    let r = unsafe {
        setsockopt(
            sock.as_raw_fd(),
            SOL_SOCKET,
            name,
            &val as *const c_int as *const c_void,
            4,
        )
    };
    r == 0
}

// 64-byte synthetic market tick: seq + sender timestamp + tick fields.
#[repr(C)]
struct Tick {
    seq: u64,        // 0..8
    send_ns: u64,    // 8..16  sender CLOCK_MONOTONIC timestamp
    symbol_id: u32,  // 16..20
    _pad0: u32,      // 20..24
    price_ticks: i64, // 24..32
    qty: u64,        // 32..40
    side: u8,        // 40
    _pad1: [u8; 23], // 41..64
}

const WARMUP: u64 = 200_000;
const N: u64 = 1_000_000;
const INTERVAL_NS: u64 = 50_000; // paced 20k pps: sustainable on this 2-vCPU VM
const FIN_SEQ: u64 = u64::MAX;

fn main() {
    assert_eq!(std::mem::size_of::<Tick>(), 64);

    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("plain");
    let busypoll = mode == "busypoll";
    let port: u16 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(41001);

    println!("mode={} port={} packets={} (+{} warmup) pace={} pps", mode, port, N, WARMUP,
        1_000_000_000 / INTERVAL_NS);

    let ready = Arc::new(AtomicBool::new(false));
    let r_ready = ready.clone();

    // ---------------- receiver ----------------
    let rx = std::thread::spawn(move || {
        let sock = UdpSocket::bind(format!("127.0.0.1:{}", port)).expect("bind rx");
        assert!(set_int_opt(&sock, SO_RCVBUF, 16 * 1024 * 1024), "SO_RCVBUF");
        let pinned = pin_to_cpu(1);
        if busypoll {
            assert!(set_int_opt(&sock, SO_BUSY_POLL, BUSY_POLL_USEC), "SO_BUSY_POLL");
            sock.set_nonblocking(true).expect("nonblocking");
        } else {
            // 2s timeout so a dead sender can't hang the bench forever.
            let tv = Timeval { tv_sec: 2, tv_usec: 0 };
            let r = unsafe {
                setsockopt(
                    sock.as_raw_fd(),
                    SOL_SOCKET,
                    SO_RCVTIMEO,
                    &tv as *const Timeval as *const c_void,
                    std::mem::size_of::<Timeval>() as u32,
                )
            };
            assert_eq!(r, 0, "SO_RCVTIMEO");
        }
        println!("[rx] bound, busypoll={} pinned_cpu1={}", busypoll, pinned);
        r_ready.store(true, Ordering::Release);

        let mut buf = [0u8; 64];
        let mut samples: Vec<u64> = Vec::with_capacity(N as usize);
        let mut expected: u64 = 0;
        let mut drops: u64 = 0;

        loop {
            let n = match sock.recv_from(&mut buf) {
                Ok((n, _)) => n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::hint::spin_loop();
                    continue;
                }
                Err(e) => panic!("[rx] recv error: {}", e),
            };
            let t_recv = now_ns();
            if n != 64 {
                continue;
            }
            let seq = u64::from_ne_bytes(buf[0..8].try_into().unwrap());
            if seq == FIN_SEQ {
                break;
            }
            if seq != expected {
                drops += seq.saturating_sub(expected);
                expected = seq + 1;
            } else {
                expected += 1;
            }
            if seq >= WARMUP {
                let send_ns = u64::from_ne_bytes(buf[8..16].try_into().unwrap());
                samples.push(t_recv.saturating_sub(send_ns));
            }
        }
        (samples, drops)
    });

    // ---------------- sender ----------------
    let s_ready = ready.clone();
    let tx = std::thread::spawn(move || {
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind tx");
        sock.connect(format!("127.0.0.1:{}", port)).expect("connect");
        assert!(set_int_opt(&sock, SO_SNDBUF, 16 * 1024 * 1024), "SO_SNDBUF");
        let pinned = pin_to_cpu(0);
        while !s_ready.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        println!("[tx] pinned_cpu0={} starting paced send", pinned);

        let mut tick = Tick {
            seq: 0,
            send_ns: 0,
            symbol_id: 7,
            _pad0: 0,
            price_ticks: 1_000_000,
            qty: 100,
            side: 1,
            _pad1: [0u8; 23],
        };
        // Build the wire bytes once; patch seq + timestamp per packet.
        let mut wire = [0u8; 64];
        let mut next = now_ns();
        for i in 0..(WARMUP + N) {
            while now_ns() < next {
                std::hint::spin_loop();
            }
            next += INTERVAL_NS;
            tick.seq = i;
            tick.send_ns = now_ns();
            // copy struct -> wire (repr(C), no padding surprises at 64B)
            let src: &[u8; 64] =
                unsafe { &*(&tick as *const Tick as *const [u8; 64]) };
            wire.copy_from_slice(src);
            // mutate a couple of fields so the compiler can't hoist anything
            wire[16] = (i & 0xff) as u8;
            let _ = sock.send(&wire);
        }
        // FIN marker
        let mut fin = [0u8; 64];
        fin[0..8].copy_from_slice(&FIN_SEQ.to_ne_bytes());
        let _ = sock.send(&fin);
        println!("[tx] done, sent {} packets", WARMUP + N);
    });

    let (samples, drops) = rx.join().expect("rx thread");
    tx.join().expect("tx thread");

    let n = samples.len();
    println!("received_measured={} drops={}", n, drops);
    if n == 0 {
        println!("NO SAMPLES - bench failed");
        return;
    }
    let mut s = samples;
    s.sort_unstable();
    let pct = |p: f64| s[(((p / 100.0) * n as f64) as usize).min(n - 1)];
    let sum: u128 = s.iter().map(|&x| x as u128).sum();
    let mean = sum as f64 / n as f64;
    println!("latency_ns: min={} p50={} p90={} p99={} p999={} max={} mean={:.1}",
        s[0], pct(50.0), pct(90.0), pct(99.0), pct(99.9), s[n - 1], mean);
}
