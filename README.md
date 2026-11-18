# GamingVoice

**单独调每一个方向的音量**，用 V3 空间引擎按「声道」区分自己与敌人，再把 7.1 经
HeSuVi HRIR 卷积折成双耳，直接喂给耳机。

对齐 ArtIsWar「ArtTune」思路的开源实现：**VB-Cable 虚拟 7.1 端点 + ATK 空间引擎 +
HRIR 双耳化**——一个 exe 搞定，不装 APO、不改系统音频服务、不改注册表。

```
游戏(7.1) ──► 虚拟端点 VB-CABLE（8 声道）
                 │   WASAPI loopback 抓 8 条独立声道（不求和）
                 ▼
           每方向增益 / 延时          ← 8 个推子，就是「分方向音量」
                 │
                 ▼
           ATK V3 空间引擎（1:1 翻译 ArtIsWar 开源 atk_spatial_engine.jsfx）
                 │   FC 自抑制 + Center Duck + 自适应 EQ + 四点 crossover
                 │   + self-suppress + CMR + 瞬态整形 + 下行扩展 + 方向强调 + LFE 侧链
                 │   —— 核心：按「声道」区分自己/敌人，而不是按「响度」
                 ▼
           HeSuVi HRIR 卷积（8 → 2）  ← 内置 EAC_Default.wav，可切换 hrir/ 外部模型
                 │
                 ▼
           耳机 EQ → lookahead 限幅 → 耳机（2 声道）
```

> 双耳化的方位来源是 **HRIR 卷积**（HeSuVi 14 轨 / 16 声道模型，实测数据、前后可分），
> 不是早期版本的球谐域合成模型。默认内置 HeSuVi `EAC_Default.wav`，用户可自行把
> HeSuVi 的 `.wav` 放进 `hrir/` 目录在面板切换（静默热切换）。

---

## 一、为什么它是一个独立程序

这套功能原本要加进 PureVox 的输出侧，但 PureVox 的采集回调第一行就把 N 声道求平均
成 1 声道，8 个推子在物理上只能合成一个标量。所以 GamingVoice 走**旁路**：自己开
loopback、自己开渲染流，完全不碰那条单声道通路。它复用同一份 DSP 内核
（`src-tauri/src/dsp/surround/`，通过 `#[path]` 共享，不是复制）。

---

## 二、三分钟跑起来

1. **装 VB-CABLE**（免费虚拟声卡），装完重启。程序面板内有一键安装。
2. **把它的播放端点改成 7.1** —— 这一步最容易漏：
   `声音设置 → 更多声音设置 → 播放 → CABLE Input → 右键"配置扬声器" → 7.1 环绕`。
   不改这步它只声明 2 声道，游戏照样输出立体声，推子就只会改总音量。
3. 游戏里把输出设备选成 **CABLE Input**，并把音频设为 7.1 / 家庭影院。
4. 跑 `GamingVoice.exe`：源 = `CABLE Input（8 声道）`，输出 = 你的耳机，点「启动」。
5. 打开完整面板，拖推子。**必须戴耳机**——方位信息全在左右耳的时间差和频响差里。

---

## 三、关键旋钮（自己枪声/脚步怎么压）

区分自己与敌人靠的是**声道**，不是响度。FPS 游戏一般把自己枪声/脚步混在
**中置 FC 或前方中央分量 (FL+FR)/2**，敌人声音散布其它声道。

| 旋钮 | 默认 | 控制什么 |
| --- | --- | --- |
| 方向增益（8 推子） | 全 0 | 每个 7.1 方向的音量，保持中性即可 |
| 自身枪声 | 0.8 | FC 中置**中高频**分量从 L/R 的减除量 |
| 自身脚步 | 0.7 | FC 中置**低频**分量（脚步冲击）的减除量 |
| 脚步增强 | 0.5 | FC 静默时抬升 200Hz/3kHz 脚步频段（远处敌人相对放大） |
| **Duck 深度** | -30 dB | 开枪时「前方中央分量 (FL+FR)/2」被压掉的量——压自声的**主旋钮** |
| **听远** | 0.75 | 越大，下行扩展器越温和（远处微弱声不被压掉）+ 脚步 boost 越强——「听更远」的主旋钮 |

> COD 等把自己枪声放在 FL/FR 中央分量、而不是 FC，所以「Duck 深度」才是压自声的主旋钮。
> 面板「敌我诊断」框实时显示 Duck 压降与最近一次枪声的各声道峰值，开一枪即可确认
> 自声混在哪个声道，据此调 Duck 深度，不用动方向推子。

---

## 四、目录结构

| 路径 | 说明 |
| --- | --- |
| `gaming-voice/` | 主程序（Rust，单 exe，无运行时依赖），`cargo build` 入口 |
| `gaming-voice/src/main.rs` | HTTP 服务、状态聚合、启动/停止、HRIR 加载 |
| `gaming-voice/src/engine.rs` | 处理线程：7.1 → 增益 → ATK V3 → HRIR → EQ → 限幅 |
| `gaming-voice/src/audio_windows.rs` | WASAPI loopback 采集 / 渲染、设备枚举与真实设备名 |
| `gaming-voice/src/gui_windows.rs` | 原生窗口 + 任务栏托盘 |
| `gaming-voice/src/web/` | 网页面板（`include_str!` 编译进 exe） |
| `gaming-voice/assets/` | `icon.ico` + 内嵌的 VB-CABLE 安装包 |
| `gaming-voice/hrir/` | 内置 `EAC_Default.wav`（HeSuVi 14 轨，编译期嵌入） |
| `src-tauri/src/dsp/surround/` | DSP 内核：布局、WAV、分块卷积、HRIR、EQ、ATK V3 引擎等 |
| `src-tauri/src/compat/` | APO `Copy:` 混音矩阵解析兼容层（文档引用，不参与编译） |
| `docs/` | 设计评估文档 |
| `hrtf_data/` | 早期球谐域 HRTF 的研究数据（CIPIC SOFA + 生成脚本，当前 HRIR 渲染路径不用） |

---

## 五、构建

```bash
cd gaming-voice
cargo build --release          # → target/release/GamingVoice.exe
cargo test                     # 单元测试
cargo clippy --all-targets
```

Windows 需要 MSVC Build Tools（`link.exe`）。首次构建拉 `windows` crate，国内建议配镜像：

```toml
# ~/.cargo/config.toml
[source.crates-io]
replace-with = 'rsproxy-sparse'
[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"
```

CI：`.github/workflows/build.yml` 在 `windows-latest` 上跑 fmt / clippy / test / release。

---

## 六、已知边界

- **采样率固定 48 kHz**。端点不是 48 kHz 时明确报错并提示去哪改，不做隐式重采样。
- **只支持 Windows**。DSP 与面板跨平台，采集/渲染/GUI 是 Win32 + WASAPI。
- **处理块长锁定 128（2.7 ms）**：采集周期固定 10 ms，改块长不降延迟、只增 CPU。
- **单方向增益上限 +12 dB**：防削波，竞技场景只抬 1~2 个方向即可。
- **HRIR 许可**：Dolby / DTS 的 HRIR 属录制数据，不随源码分发；程序只支持用户自行导入
  HeSuVi / 开放数据集（SONICOM 等）的模型。
