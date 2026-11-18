// PureVox — AI 麦克风降噪工具
// Copyright (C) 2024-2026 a2heng <752848283@qq.com>
//
// PureVox is licensed under the GNU General Public License v3.0 or
// later (GPL-3.0-or-later).  See LICENSE for details.
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! 与外部音频生态的**配置层**互操作（纯文本进、纯参数出，不碰设备、不起进程）。
//!
//! - [`apo`]：Equalizer APO 的 `config.txt` / HeSuVi 的 `hesuvi.txt` 语法。
//!
//! 这一层存在的理由：用户手里已经有 ArtIsWar / ArtTune / HeSuVi 那套调好的配置，
//! 让他们"全部重调一遍"是没人会用的功能。导入即迁移。

pub mod apo;
