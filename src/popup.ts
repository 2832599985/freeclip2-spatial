import "./popup.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import productAnim from "./assets/img/product_anim.png";
import productStill from "./assets/img/product_still.png";

type Battery = { left: number; right: number; case: number };
type Status = {
  connected: boolean;
  port: string | null;
  mode: number;
  battery: Battery | null;
  message: string;
  needs_repair: boolean;
};

const caseIcon = `<svg viewBox="0 0 24 24"><path d="M4 9h16a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2v-8a2 2 0 0 1 2-2zm1-3h14a1 1 0 0 1 1 1v1H4V7a1 1 0 0 1 1-1z"/></svg>`;

const root = document.querySelector<HTMLDivElement>("#popup")!;
root.innerHTML = `
<div class="stage">
  <div class="card" id="card" title="点击关闭">
    <button class="close" id="close" title="关闭">&#10005;</button>

    <div class="hero">
      <img id="product" src="${productAnim}" alt="HUAWEI FreeClip 2" />
    </div>

    <div class="stats">
      <div class="stat">
        <div class="num" id="v-lr">--<small>%</small></div>
        <div class="tag">L/R</div>
      </div>
      <div class="stat">
        <div class="num" id="v-case">--<small>%</small></div>
        <div class="tag case">${caseIcon}</div>
      </div>
    </div>

    <div class="name">HUAWEI FreeClip 2</div>
    <div class="sub" id="sub">已连接 · 蓝牙音频</div>
  </div>
</div>
`;

const $ = <T extends HTMLElement>(s: string) => document.querySelector<T>(s)!;

// ~4.7s animation + ~1.5s hold, then fade out. A single, non-restartable
// timer: status events may arrive several times while the popup is up, and
// they must NOT push the dismissal back.
const ANIM_MS = 4700;
const HOLD_MS = 1500;
const FADE_MS = 320;
let dismissing = false;
let dismissTimer: number | undefined;

function dismiss() {
  if (dismissing) return;
  dismissing = true;
  if (dismissTimer) clearTimeout(dismissTimer);
  $("#card").classList.add("leaving");
  // Mirror the entrance: the real window slides back down off-screen while
  // the card fades, then the window is destroyed.
  invoke("slide_popup_out").catch(() => {});
  window.setTimeout(() => {
    invoke("dismiss_popup").catch(() => {});
  }, FADE_MS + 220);
}

function startDismissTimer() {
  if (dismissTimer !== undefined || dismissing) return;
  dismissTimer = window.setTimeout(dismiss, ANIM_MS + HOLD_MS);
}

function render(s: Status) {
  const b = s.battery;
  $("#v-lr").innerHTML = b
    ? `${Math.min(100, Math.max(0, Math.min(b.left, b.right)))}<small>%</small>`
    : `--<small>%</small>`;
  $("#v-case").innerHTML = b
    ? `${Math.min(100, Math.max(0, b.case))}<small>%</small>`
    : `--<small>%</small>`;
  $("#sub").textContent = s.connected
    ? `已连接 · ${s.port ?? "蓝牙音频"}`
    : s.message;
}

await listen<Status>("popup-status", (e) => {
  if (e.payload) render(e.payload);
});

// Huawei plays the open-lid animation once, then holds the expanded frame.
window.setTimeout(() => {
  const img = document.querySelector<HTMLImageElement>("#product");
  if (img && !dismissing) img.src = productStill;
}, ANIM_MS);

// Any click closes the popup (the original has no action button).
$("#card").addEventListener("click", dismiss);
$("#close").addEventListener("click", (e) => { e.stopPropagation(); dismiss(); });

// The window is parked below the screen until we say the card has actually
// painted — otherwise the entrance slide moves a blank white rectangle.
// Two frames is enough for WebView2 to have committed the first paint.
requestAnimationFrame(() => requestAnimationFrame(() => {
  invoke("popup_ready").catch(() => {});
}));

startDismissTimer();
