# FreeClip 2 空间音频（Tauri 重制版）

HUAWEI FreeClip 2 的空间音频控制工具，用 **Tauri 2 + Rust + WebView2** 重写了原版 Python/PySide6 程序。

## 功能

- 切换空间音频模式：关闭 / 头部跟踪 / 固定声场
- 实时电量：左耳 / 右耳 / 充电盒
- 耳机回连时自动弹出华为风格弹窗（产品动画 + 电量）
- 托盘常驻、单实例（不会出现多个进程抢占串口）、关闭窗口最小化到托盘
- 串口 RAII 释放：退出即释放 COM 口，不会出现"端口被占用"

## 运行要求

- Windows 10/11
- WebView2 Runtime（Windows 11 自带；Windows 10 一般随 Edge 已安装）
- 耳机需在系统中通过蓝牙配对，并已连接（程序会自动探测蓝牙 SPP 虚拟串口）

## 使用

从 [Releases](../../releases) 下载 `freeclip2-spatial.exe`，双击即可运行，无需安装、无外部依赖文件。

## 自行构建

```bash
npm install
npx tauri build --no-bundle   # 产物: src-tauri/target/release/freeclip2-spatial.exe
npx tauri build               # 额外生成 MSI 安装包
```

开发调试：

```bash
npx tauri dev
```

## 说明

- 通过耳机私有 SPP 协议（`0x2B/0xB4` 通用设置，feature `0x18`）控制空间音频，协议为逆向所得。
- `src/assets` 中的产品图片、Lottie 动画与品牌标识来自华为智慧音频 App，版权归华为所有，仅用于个人学习与互操作目的，本项目与华为无任何关联。
- 蓝牙低延迟（游戏模式）属于手机侧链路/编解码策略，Windows 端无法通过该协议实现。
