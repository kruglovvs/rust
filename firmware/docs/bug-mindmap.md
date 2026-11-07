# Bug mindmap — camera capture and RTSP bind

Two independent failures. Each has a small set of mutually exclusive causes and a test that
separates them, so the next run produces evidence instead of another blind config sweep.

---

## A. Camera: `NO-SOI` / `NO-EOI` — frames corrupted, sensor healthy

```
                     OV3660 sensor
                          |
        SCCB / I2C control  ── PROVEN GOOD (PID 0x3660 read over I2C0, 3 runs)
                          |
        XCLK  GPIO10, 20 MHz ── PROVEN GOOD (sensor answers SCCB only while XCLK runs)
                          |
        DVP data: D0..D7, PCLK, HREF, VSYNC
                          |
        cam_hal DMA into the frame buffer
                          |
        driver checks buf[0..2] == FF D8 (SOI) and tail == FF D9 (EOI)
                          |
        FAILS: NO-SOI (bad head) or NO-EOI (bad tail)
```

Control and clock are proven, so the fault is confined to the **data path** or to **what the
driver hands the caller**. Three mutually exclusive causes remain:

| # | Cause | Its signature | What separates it |
|---|---|---|---|
| A1 | **DMA truncation** — frame bigger than the allocated buffer (`buffer_size = 15360` for QVGA/q15) | `bytesused` smaller than a real JPEG; tail missing → NO-EOI | log `len` plus first/last bytes on rejection |
| A2 | **DMA misalignment** — data path offset (PCLK/HREF/VSYNC timing, or data-pin order) | `len` plausible but head is not `FF D8` (e.g. `FF D9` first, or garbage) | same log: inspect the head bytes |
| A3 | **Buffer lifecycle** — driver returns a buffer still being written, or the wrong buffer | alternating good/bad frames, runs of zeros | log buffer `index`, `flags`, `len` |

**Why this has not been resolved yet:** every sweep so far (XCLK 20/16/10 MHz, PSRAM-DMA
on/off, PSRAM vs DRAM framebuffers, `GRAB_LATEST` vs `WHEN_EMPTY`, `fb_count` 1/2, quality
15/25) changed *which* symptom appeared but never captured the buffer contents. A1 and A2
need **opposite** fixes:

- A1 → enlarge `buffer_size`, or shrink frames (quality / resolution), or keep more buffers
- A2 → correct data-pin order or PCLK/HREF/VSYNC polarity and timing
- A3 → single buffer with a blocking grab, or copy the frame before reuse

The decisive datum is `bytesused` and the first and last four bytes of a rejected frame.

---

## B. RTSP: `Service::spawn(554)` → `ENOMEM (os error 12)`

```
boot
 ├─ net::ensure_netif_initialised      LwIP up
 ├─ camera init                        2 x 16400 B PSRAM + internal DMA pools
 ├─ storage bringup                    SD mount, 32 KiB transfer buffers
 ├─ mic spawn                          I2S driver, 64 KiB task stack
 ├─ net::bring_up
 │    ├─ event loop + NVS
 │    ├─ EspWifi + BlockingWifi        Wi-Fi driver allocates from internal RAM
 │    ├─ mDNS advertiser               own task, "created from internal RAM"
 │    ├─ artifact HTTP server
 │    └─ SNTP
 └─ Service::spawn(554)
      ├─ TcpListener::bind      <-- FAILS with ENOMEM
      └─ 2 x UdpSocket::bind
```

`ENOMEM` here is not a Rust error: LwIP could not allocate a PCB or a socket-table entry from
internal RAM. Causes, most to least likely:

| # | Cause | Its signature | What separates it |
|---|---|---|---|
| B1 | **`CONFIG_LWIP_MAX_SOCKETS` exhausted** (default 10; mDNS + HTTP + SNTP already hold several) | fails at the last socket of `Service::spawn` | raise the limit to 16 and retest |
| B2 | **Internal heap exhausted** by mDNS task + Wi-Fi + camera internal pools | free internal SRAM near zero at spawn time | log `heap_caps_get_free_size(MALLOC_CAP_INTERNAL)` right before spawn |
| B3 | **Listen-PCB limit** (`CONFIG_LWIP_MAX_LISTENING_TCP`) | `bind` fails while a plain UDP socket would succeed | raise the limit; check which call errors |

The fixes are cheap and orthogonal: raise the socket limit, log internal free heap before
bind, and **bind the RTSP listener before mDNS/SNTP allocate**, so the socket is reserved
first. A short bounded retry also absorbs transient pool pressure during Wi-Fi association.

---

## Fix order

1. Camera frame diagnostics (`len`, head/tail bytes, flags) → one run resolves A1 / A2 / A3.
2. Raise the LwIP socket limit, log internal heap, bind RTSP earlier → resolves B1 / B2 / B3.
3. Re-test; commit only what the hardware proves.
