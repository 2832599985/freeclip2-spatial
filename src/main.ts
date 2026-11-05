import "./style.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import lottie, { type AnimationItem } from "lottie-web";

// Official Huawei assets extracted from com.huawei.smartaudio 2.0.7.321
import productImg from "./assets/img/product_still.png";
import huaweiLogo from "./assets/img/huawei_logo.png";
import animClose from "./assets/lottie/new_close.json";
import animFollow from "./assets/lottie/new_follow.json";
import animFixed from "./assets/lottie/new_fixed.json";

type Battery = { left: number; right: number; case: number };
type Status = {
  connected: boolean;
  port: string | null;
  mode: number;
  battery: Battery | null;
  message: string;
};

const MODES = [
  { value: 0, name: "关闭", desc: "原生立体声，不渲染空间效果", anim: animClose, ip: 303 },
  { value: 1, name: "头部跟踪", desc: "跟随头部动作实时重塑声场", anim: animFollow, ip: 0 },
  { value: 2, name: "固定声场", desc: "固定虚拟声场，适合观影", anim: animFixed, ip: 126 },
];

const app = document.querySelector<HTMLDivElement>("#app")!;

app.innerHTML = `
<div class="app-shell">
  <div class="titlebar">
    <div class="brand">
      <img class="brand-mark" src="${huaweiLogo}" alt="HUAWEI" />
      <span class="brand-name">FreeClip 2 · 空间音频</span>
    </div>
    <div class="titlebar-actions">
      <button class="tb-btn" id="btn-refresh" title="刷新状态">&#8635;</button>
      <button class="tb-btn" id="btn-min" title="最小化到托盘">&#8722;</button>
      <button class="tb-btn" id="btn-quit" title="最小化到托盘">&#10005;</button>
    </div>
  </div>

  <div class="content">
    <div class="hero" id="hero">
      <div class="hero-icon">
        <div class="pulse"></div>
        <img class="hero-product" src="${productImg}" alt="FreeClip 2" />
      </div>
      <div class="hero-text">
        <div class="hero-title">HUAWEI FreeClip 2</div>
        <div class="hero-sub" id="hero-sub">正在连接…</div>
      </div>
      <div class="status-pill" id="pill"><span class="dot"></span><span id="pill-text">连接中</span></div>
    </div>

    <div class="section">
      <div class="section-head"><span class="section-label">空间音频模式</span><span class="section-hint" id="port-hint"></span></div>
      <div class="modes" id="modes">
        ${MODES.map((m) => `
          <button class="mode" data-mode="${m.value}" disabled>
            <div class="mode-glyph" id="glyph-${m.value}"></div>
            <div class="mode-body"><div class="mode-name">${m.name}</div><div class="mode-desc">${m.desc}</div></div>
            <div class="mode-check"></div>
          </button>`).join("")}
      </div>
    </div>

    <div class="section">
      <div class="section-head"><span class="section-label">电量</span></div>
      <div class="batteries" id="batteries">
        ${["左耳", "右耳", "充电盒"].map((label, i) => `
          <div class="batt">
            <div class="batt-ring">
              <svg viewBox="0 0 46 46">
                <circle class="track" cx="23" cy="23" r="19"/>
                <circle class="bar" id="bar-${i}" cx="23" cy="23" r="19" stroke-dasharray="0 119.4"/>
              </svg>
              <div class="val" id="val-${i}">--</div>
            </div>
            <div class="batt-label">${label}</div>
          </div>`).join("")}
      </div>
    </div>

    <div class="footer">
      <button class="btn" id="btn-settings">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .34 1.87l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06A1.7 1.7 0 0 0 15 19.4a1.7 1.7 0 0 0-1 1.56V21a2 2 0 1 1-4 0v-.09A1.7 1.7 0 0 0 9 19.4a1.7 1.7 0 0 0-1.87.34l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.7 1.7 0 0 0 4.6 15a1.7 1.7 0 0 0-1.56-1H3a2 2 0 1 1 0-4h.09A1.7 1.7 0 0 0 4.6 9a1.7 1.7 0 0 0-.34-1.87l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.7 1.7 0 0 0 9 4.6a1.7 1.7 0 0 0 1-1.56V3a2 2 0 1 1 4 0v.09A1.7 1.7 0 0 0 15 4.6a1.7 1.7 0 0 0 1.87-.34l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.7 1.7 0 0 0 19.4 9c.15.61.7 1.04 1.33 1.05H21a2 2 0 1 1 0 4h-.09A1.7 1.7 0 0 0 19.4 15z"/></svg>
        蓝牙设置
      </button>
      <button class="btn" id="btn-retry">重新检测</button>
      <button class="btn" id="btn-popup">弹窗预览</button>
    </div>
  </div>
</div>
<div class="toast" id="toast"></div>
`;

const $ = <T extends HTMLElement>(sel: string) => document.querySelector<T>(sel)!;
let toastTimer: number | undefined;

function toast(msg: string, isErr = false) {
  const el = $("#toast");
  el.textContent = msg;
  el.classList.toggle("err", isErr);
  el.classList.add("show");
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => el.classList.remove("show"), 2600);
}

const CIRC = 2 * Math.PI * 19;

// Official Huawei Lottie animations, one per spatial-audio mode.
// Each animation is optional: a malformed Lottie must never break the rest
// of the UI (buttons, status polling, window controls).
const animations: Partial<Record<number, AnimationItem>> = {};
MODES.forEach((m) => {
  try {
    const el = document.querySelector<HTMLElement>(`#glyph-${m.value}`);
    if (!el) return;
    animations[m.value] = lottie.loadAnimation({
      container: el,
      renderer: "svg",
      loop: true,
      autoplay: false,
      animationData: m.anim as unknown as object,
      rendererSettings: { preserveAspectRatio: "xMidYMid meet" },
    });
  } catch (e) {
    console.warn("Lottie failed for mode", m.value, e);
  }
});

function playActiveMode(mode: number) {
  MODES.forEach((m) => {
    const a = animations[m.value];
    if (!a) return;
    try {
      if (m.value === mode) {
        // Each Huawei Lottie has its own in-point (e.g. 关闭 starts at 303).
        a.goToAndPlay(m.ip, true);
      } else {
        a.goToAndStop(m.ip, true);
      }
    } catch (e) {
      console.warn("Lottie play failed", e);
    }
  });
}

function render(s: Status) {
  const hero = $("#hero");
  hero.classList.toggle("connected", s.connected);
  const pill = $("#pill");
  pill.className = "status-pill" + (s.connected ? " ok" : " err");
  $("#pill-text").textContent = s.connected ? "已连接" : "离线";
  $("#hero-sub").textContent = s.connected
    ? `${s.port} · ${s.message}`
    : "离线 · 耳机未连接";
  $("#port-hint").textContent = s.connected ? (s.port ?? "") : "";

  document.querySelectorAll<HTMLButtonElement>(".mode").forEach((btn) => {
    const v = Number(btn.dataset.mode);
    btn.classList.toggle("active", s.connected && v === s.mode);
    btn.disabled = !s.connected;
  });
  if (s.connected) {
    playActiveMode(s.mode);
  } else {
    MODES.forEach((m) => {
      try { animations[m.value]?.goToAndStop(m.ip, true); } catch { /* ignore */ }
    });
  }

  const vals = s.battery
    ? [s.battery.left, s.battery.right, s.battery.case]
    : [null, null, null];
  vals.forEach((v, i) => {
    const bar = $(`#bar-${i}`) as unknown as SVGCircleElement;
    const val = $(`#val-${i}`);
    if (v === null) {
      bar.style.strokeDasharray = "0 119.4";
      val.textContent = "--";
      return;
    }
    const pct = Math.min(100, Math.max(0, v));
    bar.style.strokeDasharray = `${(CIRC * pct) / 100} 119.4`;
    bar.style.stroke = pct <= 20 ? "var(--red)" : pct <= 40 ? "var(--amber)" : "var(--green)";
    val.textContent = `${pct}%`;
  });

}

// All Tauri commands now return instantly from an in-memory snapshot; the
// worker pushes `status-changed` whenever anything actually changes.
async function refresh() {
  try {
    const s = await invoke<Status>("get_status");
    render(s);
  } catch (e) {
    render({ connected: false, port: null, mode: 0, battery: null, message: String(e) });
  }
}

async function setMode(mode: number) {
  try {
    await invoke("set_spatial", { mode });
    toast(`正在切换为「${MODES[mode].name}」…`);
  } catch (e) {
    toast(String(e), true);
  }
}

document.querySelectorAll<HTMLButtonElement>(".mode").forEach((btn) => {
  btn.addEventListener("click", () => setMode(Number(btn.dataset.mode)));
});
$("#btn-refresh").addEventListener("click", refresh);
$("#btn-retry").addEventListener("click", refresh);
$("#btn-settings").addEventListener("click", () => invoke("open_bluetooth_settings"));
$("#btn-popup").addEventListener("click", () => invoke("preview_popup"));
$("#btn-min").addEventListener("click", () => getCurrentWindow().hide());
// X minimizes to the tray (quit lives in the tray menu).
$("#btn-quit").addEventListener("click", () => getCurrentWindow().hide());

// No top-level await: a hung event bridge must never block button handlers.
listen<number>("tray-set-mode", (e) => setMode(e.payload)).catch((e) =>
  console.warn("tray listener failed", e),
);
listen<Status>("status-changed", (e) => {
  if (e.payload) render(e.payload);
}).catch((e) => console.warn("status listener failed", e));

// Lightweight fallback poll (instant, memory-only).
window.setInterval(() => {
  if (!document.hidden) refresh();
}, 5000);

refresh();
