# GamingVoice

**单独调每一个方向的音量**，再折成双耳给耳机。

就是 ArtIsWar 那套「VB-Cable + Equalizer APO + HeSuVi」在做的事 —— 一个 exe 搞定，
不装 APO、不改系统音频服务、不改注册表。

```
游戏(7.1) ──► 虚拟端点 VB-CABLE（8 声道）
                  │   WASAPI loopback 抓 8 条独立声道（不求和）
                  ▼
            每声道增益 / 延时        ← 8 个推子 + 20 dB 上限，这就是"分方向音量"
                  │   y = H · G · x，G 是对角阵
                  ▼
            HRIR 卷积 8 → 2         ← 方位感在这里产生
                  ▼
            耳机（2 声道）
```

---

## 一、它为什么是一个独立程序

这套功能原本要加进 PureVox 的输出侧。读了源码后发现一个硬阻塞：
`src-tauri/src/audio/capture.rs` 的设备回调第一行就把 N 声道求平均成 1 声道 ——
`y = Σ(w_c · g_c) · x = (Σ w_c g_c) · x`，8 个推子最终只能合成一个标量。
不是没实现，是方向信息在采集那一步就被销毁了。

所以 GamingVoice 走**旁路**：自己开 loopback、自己开渲染流，完全不碰那条单声道通路。
它复用同一份 DSP 内核（`src-tauri/src/dsp/surround/`，与 PureVox 共享，不是复制）。

## 二、三分钟跑起来

1. **装 VB-CABLE**（免费虚拟声卡），装完重启。
2. **把它的播放端点改成 7.1** —— 这一步最容易漏：
   `声音设置 → 更多声音设置 → 播放 → CABLE Input → 右键"配置扬声器" → 7.1 环绕`。
   不改这步它只声明 2 声道，游戏照样输出立体声，推子就只会改总音量。
3. 游戏里把输出设备选成 **CABLE Input**，并把音频设为 7.1 / 家庭影院。
4. 跑 `GamingVoice.exe`：
   - 源 = `CABLE Input（8 声道）`
   - 输出 = 你的耳机
   - 点「启动」
5. 打开完整面板，拖推子。戴耳机听。

> **必须戴耳机**。方位信息全在左右耳的时间差和频响差里，音箱放出来就没了。

## 三、交付内容

| 路径 | 说明 |
|---|---|
| `gaming-voice/` | 主程序（Rust，单 exe，无运行时依赖） |
| `gaming-voice/src/gui_windows.rs` | 原生窗口 + 任务栏托盘 |
| `gaming-voice/src/web/` | 网页面板（编译进 exe，无需外部文件） |
| `gaming-voice/src/audio_windows.rs` | WASAPI loopback 采集 / 渲染、设备枚举与真实设备名 |
| `src-tauri/src/dsp/surround/` | DSP 内核：7.1 布局、WAV/HRIR、分块卷积、每声道增益、延迟预算 |
| `docs/EQ-评估.md` | EQ 功能可行性评估（含实测 CPU 数据） |

## 四、构建

```bash
cd gaming-voice
cargo build --release          # → target/release/GamingVoice.exe
cargo test                     # 42 项单元测试
cargo clippy --all-targets -- -D warnings
cargo run --release --example eq_bench   # EQ 段数的成本实测
```

Windows 需要 MSVC Build Tools（`link.exe`）。首次构建拉 `windows` crate，国内建议配镜像：

```toml
# ~/.cargo/config.toml
[source.crates-io]
replace-with = 'rsproxy-sparse'
[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"
```

CI：`.github/workflows/build.yml` 会在 `windows-latest` 上跑 fmt / clippy / test / release，
产物是 `GamingVoice.exe` 工件。

## 五、已知边界

- **采样率固定 48 kHz**。端点不是 48 kHz 时程序会明确报错并告诉你去哪改，不做隐式重采样。
- **只支持 Windows**。DSP 与面板跨平台，采集/渲染/GUI 是 Win32 + WASAPI。
- **HRIR 是合成的**（Woodworth 球头模型：双耳时差 + 头影低通），不是测量数据。
  方位精度不如 HeSuVi / Dolby，但"哪个方向变响了"足够清楚。替换真实 HRIR 见
  `gaming-voice/README.md` 的 HRIR 一节。
- **不做 Dolby / DTS 的替代**。要用 Dolby 的双耳化，就把它的空间音效打开、GamingVoice
  只负责 8 声道增益；两个 HRTF 串起来不是更好，是更糊。
