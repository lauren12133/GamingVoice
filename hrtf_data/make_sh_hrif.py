#!/usr/bin/env python3
"""CIPIC KEMAR (subject_021) SOFA → 3 阶球谐域 HRTF（SH-HRTF）。

球谐约定与 src-tauri/src/dsp/surround/ambisonics.rs 完全一致：
  - ACN 顺序（n = l² + l + m）
  - SN3D 归一化（semi-normalized）
  - 实球谐，不含 Condon-Shortley 相位（m>0→cos, m<0→sin）
  - 输入方向用 SOFA 球坐标：azimuth(逆时针) + elevation

流程：读 IR(1250,2,200)@44.1k → 每耳/每 taps 做最小二乘球谐拟合(16 系数)
     → 重采样 48k → 补零到 256 → 峰值归一 → 输出 Rust 源码。
"""
import numpy as np
from math import factorial, sqrt, pi
import h5py

ORDER = 3
HOA = (ORDER + 1) ** 2  # 16
SR_IN = 44100
SR_OUT = 48000
TAPS = 256
OUT_RS = "../src-tauri/src/dsp/surround/sh_hrif.rs"

# ACN 顺序的 (l, m)
ACN_LM = [(l, m) for l in range(ORDER + 1) for m in range(-l, l + 1)]


def sn3d_norm(l, m):
    m = abs(m)
    return sqrt((2 * l + 1) / (4 * pi) * factorial(l - m) / factorial(l + m))


def legendre(l, m, x):
    """无 Condon-Shortley 相位的带勒让德 P_l^m(x)，x=sin(elev)。仅 l<=3。"""
    s = np.sqrt(np.maximum(0.0, 1.0 - x * x))
    table = {
        (0, 0): 1.0,
        (1, 0): x,
        (1, 1): s,
        (2, 0): 0.5 * (3 * x * x - 1),
        (2, 1): 3 * x * s,
        (2, 2): 3 * s * s,
        (3, 0): 0.5 * x * (5 * x * x - 3),
        (3, 1): 1.5 * (5 * x * x - 1) * s,
        (3, 2): 15 * x * s * s,
        (3, 3): 15 * s * s * s,
    }
    return table[(l, m)]


def sh(l, m, elev, azim):
    """实球谐 SN3D，与 ambisonics.rs::sh 一致。elev/azim 为弧度。"""
    am = abs(m)
    base = sn3d_norm(l, am) * legendre(l, am, np.sin(elev))
    if m > 0:
        return base * sqrt(2.0) * np.cos(m * azim)
    if m < 0:
        return base * sqrt(2.0) * np.sin(am * azim)
    return base


def main():
    f = h5py.File("subject_021.sofa", "r")
    ir = f["Data.IR"][:]  # (1250, 2, 200)
    az_deg = f["SourcePosition"][:, 0]
    el_deg = f["SourcePosition"][:, 1]
    f.close()

    az = np.deg2rad(az_deg)
    elev = np.deg2rad(el_deg)

    # 球谐矩阵 Y[k, n]，k=方向，n=球谐分量
    M = len(az_deg)
    Y = np.empty((M, HOA), dtype=np.float64)
    for n, (l, m) in enumerate(ACN_LM):
        Y[:, n] = sh(l, m, elev, az)

    # 最小二乘伪逆：(YᵀY)⁻¹Yᵀ，一次得到所有 taps/耳 的系数
    pinv = np.linalg.pinv(Y)  # (16, 1250)
    # ir: (1250, 2, 200) → 展平成 (1250, 400) 一次拟合
    flat = ir.reshape(M, -1)
    coef = pinv @ flat  # (16, 400)
    coef = coef.reshape(HOA, 2, 200)  # (16, 2, taps@44.1k)

    # 重采样 44.1k → 48k
    from scipy.signal import resample_poly
    n_out = int(round(coef.shape[2] * SR_OUT / SR_IN))  # ≈218
    res = np.zeros((HOA, 2, n_out), dtype=np.float64)
    for i in range(HOA):
        for e in range(2):
            res[i, e] = resample_poly(coef[i, e], SR_OUT, SR_IN)

    # 补零到 TAPS
    sh_hrif = np.zeros((HOA, 2, TAPS), dtype=np.float32)
    sh_hrif[:, :, :n_out] = res.astype(np.float32)

    # 峰值归一（引擎有 makeup，这里归一让数据范围规整，避免溢出）
    peak = np.max(np.abs(sh_hrif))
    sh_hrif /= peak

    # ---- 合理性验证 ----
    rms = np.sqrt(np.mean(sh_hrif ** 2, axis=2))  # (16, 2)
    print("各球谐分量的 RMS（左耳）：")
    for n, (l, m) in enumerate(ACN_LM):
        print(f"  n={n:2d} l={l} m={m:+d}: L={rms[n,0]:.4f} R={rms[n,1]:.4f}")
    print(f"W 分量峰值位置（左耳）: {np.argmax(np.abs(sh_hrif[0,0]))}")
    print(f"X 分量峰值位置（左耳）: {np.argmax(np.abs(sh_hrif[3,0]))}")
    # 左右耳应近似对称：W 分量左右 RMS 接近
    print(f"W 左右 RMS 比: {rms[0,0]/rms[0,1]:.4f}")

    # ---- 输出 Rust 源码 ----
    lines = []
    lines.append("// 本文件由 hrtf_data/make_sh_hrif.py 自动生成，请勿手改。")
    lines.append("// 数据来源：CIPIC HRTF 数据库 subject_021（KEMAR，大耳廓），")
    lines.append("// 经 3 阶球谐变换（SN3D/ACN）得到的球谐域 HRTF（SH-HRTF）。")
    lines.append("// 许可：CIPIC 数据库为公开研究数据（U.C. Davis CIPIC Lab），见 LICENSE-THIRD-PARTY。")
    lines.append("")
    lines.append("/// SH-HRTF 抽头数（48 kHz）。")
    lines.append(f"pub const SH_HRIF_TAPS: usize = {TAPS};")
    lines.append("/// 每只耳一条。")
    lines.append("pub const SH_HRIF_EARS: usize = 2;")
    lines.append("/// 3 阶球谐分量数。")
    lines.append(f"pub const SH_HRIF_HOA: usize = {HOA};")
    lines.append("")
    lines.append("/// 球谐域双耳解码核：`SH_HRIF[hoa][ear][t]`。")
    lines.append("/// 渲染：`ear_out = Σ_hoa (hoa[hoa] ⊛ SH_HRIF[hoa][ear])`。")
    lines.append("/// 用 `static` 而非 `const`：数组较大（32 KB），const 会在每次引用处内联复制。")
    lines.append("pub static SH_HRIF: [[[f32; SH_HRIF_TAPS]; SH_HRIF_EARS]; SH_HRIF_HOA] = [")

    def fmt(v):
        # 7 位有效数字：f32 只能精确表示约 7 位十进制，再多会被 clippy 的
        # excessive_precision 拦下，且对音频数据无意义（-140 dB 以下）。
        s = f"{v:.7g}"
        if "e" in s or "E" in s or "." in s:
            return s
        return s + ".0"

    for i in range(HOA):
        lines.append("  [  // hoa = %d" % i)
        for e in range(2):
            vals = ", ".join(fmt(v) for v in sh_hrif[i, e])
            lines.append(f"    [{vals}],  // ear = {e}")
        lines.append("  ],")
    lines.append("];")
    lines.append("")

    with open(OUT_RS, "w") as fp:
        fp.write("\n".join(lines))
    import os
    size = os.path.getsize(OUT_RS)
    print(f"\n已写出 {OUT_RS}（{size/1024:.1f} KB）")


if __name__ == "__main__":
    main()
