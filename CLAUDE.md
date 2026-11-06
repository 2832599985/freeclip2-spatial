# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Tauri 2 (Rust + WebView2) replacement for the original PyInstaller/PySide6 "FreeClip2 空间音频" tool. It controls HUAWEI FreeClip 2 spatial-audio mode (off / head tracking / fixed) and reads battery over the earbuds' private Bluetooth SPP protocol, and shows a Huawei-style "reconnected" popup.

## Commands

All frontend commands run from the repo root; Rust commands run from `src-tauri/`.

```bash
npm install                  # first time
npm run dev                  # Vite dev server only (port 1420)
npx tauri dev                # full app, hot reload
npx tauri build --no-bundle  # release exe only → src-tauri/target/release/freeclip2-spatial.exe
npx tauri build              # release + MSI installer
npm run build                # tsc type-check + vite build (two entry points)

cd src-tauri && cargo test                    # protocol unit tests (CRC, frame bytes)
cd src-tauri && cargo test live_ -- --ignored --nocapture   # live test against COM9 (earbuds must be connected)
```

The built exe is fully standalone (frontend assets are embedded); the only runtime dependency is WebView2 Runtime, preinstalled on Windows 11.

### Icon changes

`tauri-build` embeds `icons/icon.ico` into the exe's PE resources at build time but caches the result. After changing the icon you must force the build script to rerun:

```bash
cd src-tauri && touch build.rs && cd .. && npx tauri build --no-bundle
```

Windows Explorer also caches file icons separately; if the exe file icon looks stale, clear the icon cache / restart Explorer.

## Architecture

### Rust backend (`src-tauri/src/`)

- `protocol.rs` — pure protocol layer, no Tauri types.
  - Frame: `5A | LEN(2,BE) | 00 | SVC | CMD | TLV... | CRC16/XMODEM(2,BE)`; TLV = `TYPE | SIZE | VALUE`.
  - `Buds` owns one short-lived COM session and closes it on `Drop` (RAII). Never hold a `Buds` across commands.
  - **Windows SPP gotcha**: BTHENUM virtual COM ports only deliver data when DTR *and* RTS are asserted. `Buds::open` does this explicitly; pyserial did it implicitly.
  - Session handshake `0x2B/0x70` is sent 3× but never required to ACK (FreeClip 2 often doesn't). It must still be sent before `0x2B/0xB4` commands.
  - Spatial audio is generic setting feature `0x18` under `0x2B/0xB4`; reads need the third empty TLV.
  - `bluetooth_ports()` returns *all* COM ports, because Windows reports BTHENUM ports as `SerialPortType::Unknown`; `find_earbuds` probes each with a battery frame and keeps the one that answers.

- `lib.rs` — application layer.
  - **Single-owner worker thread** (`start_worker`): the only code allowed to open the COM port. It polls every 2 s (`full_status` quick probe), handles `Request::{SetMode, Preview}`, emits `status-changed`, and fires the reconnect popup on a disconnected→connected edge.
  - **Tauri commands never touch the serial port.** `get_status` returns the in-memory `SharedStatus` snapshot; `set_spatial` / `preview_popup` just push a `Request` into the worker channel. Doing serial I/O on the UI thread was the cause of the app freezing ("未响应").
  - All low-level serial errors while disconnected collapse to `Status::offline()` — the UI shows "离线", not a raw serial error.
  - `spawn_popup` creates the `popup` webview window: transparent, undecorated, always-on-top, 252×420 logical. It is built hidden and parked **below the panel** (Win32 `SPI_GETWORKAREA` + `SM_CYSCREEN`, in **physical pixels** — tao's monitor size reports the panel's native mode, e.g. 3840×2160, rather than the scaled desktop, so pure Tauri centering lands off-center on 4K/150% displays).
  - The entrance slide must not start until the page has painted: `popup.html` calls `popup_ready` after two `requestAnimationFrame`s, and only then does `begin_popup_slide` walk the window up to the centered position (~320 ms, ease-out cubic). A 900 ms watchdog slides anyway if the page never reports. Starting earlier moves a blank white rectangle, and any CSS opacity animation on the card makes the slide invisible — the card must stay fully opaque while the window travels.
  - The popup self-destroys after 8.5 s as a safety net; a click fades the card and calls `slide_popup_out`, which drops the window back off the bottom edge before `dismiss_popup` destroys it.
  - `tauri-plugin-single-instance` must stay the **first** plugin registered; a second launch forwards to the first and exits, which is what guarantees only one process can hold the COM port.
  - Close button (`CloseRequested`) hides the window to tray; quit is only in the tray menu.

- `main.rs` — just calls `freeclip2_spatial_lib::run()`; release builds use `windows_subsystem = "windows"` (no console), so `eprintln!` diagnostics only appear when launched from a terminal.

### Frontend (`src/`)

- Plain TypeScript + Vite, **no UI framework**. Two HTML entry points → `src/main.ts` (main window) and `src/popup.ts` (reconnect popup). Both build their entire DOM with template literals.
- Main window: mode cards driven by Lottie animations from the official Huawei app (`src/assets/lottie/*.json`, each has its own in-point), battery rings drawn with SVG `stroke-dasharray`, tray/close behavior. State is event-driven: `listen("status-changed")` renders, with a 5 s memory-only `get_status` fallback poll.
- Popup: APNG product animation (`product_anim.png`) that swaps to a still frame after ~4.7 s, then auto-dismisses; single non-restartable dismiss timer (repeated `popup-status` events must not extend its life). Any click closes.
- Do not use top-level `await` in these entry files — a hung event bridge previously blocked all button handlers and made the UI dead.

### Known environmental failure mode

A stale BTHENUM COM handle (e.g. after the original Python app crashes) makes `Buds::open` fail with `信号灯超时时间已到` (WinError 121) or access-denied while Device Manager still shows the port as OK. Toggling the Bluetooth radio off/on re-enumerates the port and fixes it; PnP disable/enable and `bthserv` restart often do not work.
