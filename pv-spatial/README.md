# pv-spatial —— 8 声道虚拟 7.1 调音台

**单独调每一个方向的音量**，然后折成双耳给耳机。就是 ArtIsWar 那套「VB-Cable + Equalizer APO +
HeSuVi」在做的事，一个 exe 搞定，不用装 APO，不用改系统音频服务。

```
游戏(7.1) ──► 虚拟端点(VB-CABLE, 8ch)
                  │  WASAPI loopback 抓 8 声道（不求平均）
                  ▼
            每声道增益/延时  ← 8 个推子，这就是"分方向音量"
                  │  y = H · G · x，G 是对角阵
                  ▼
            HRIR 卷积 8 → 2（方位感在这里产生）
                  ▼
              耳机（2ch）
```

---

## 一、为什么它是独立工具，不是 PureVox 的一个开关

我读了 PureVox 的源码，`src-tauri/src/audio/capture.rs` 第 221–233 行：

```rust
let ch = cfg.channels.max(1) as usize;
let inv = 1.0 / ch as f32;
for f in data.chunks_exact(ch) {
  let mut s = 0.0f32;
  for &x in f { s += from_sample(x); }   // 8 个声道全加一起
  prod.push(s * inv);                    // ÷8，变成一个数
}
```

**采集回调的第一行就把 N 声道求平均成 1 声道**，之后整条引擎（`Fanout` → 处理行 → 输出行）
都是单声道 `RingBuffer<f32>`。

在那条链路上，8 个推子的数学结果必然退化成总音量：

> `y = Σ(w_c · g_c · x) = (Σ w_c g_c) · x` —— 只有一个源信号 `x`，怎么加权都只是一个标量。

所以不是"没做 UI"，是**信息在采集那一步就被销毁了**。要做成，只能另开一条通路：
自己开 loopback、自己开渲染流，完全不碰引擎的单声道管线。这就是本工具。

---

## 二、前提：你得有一个 8 声道的端点

**这一步跳不过去。** 游戏只会在"它认为有 8 个喇叭"时输出 8 路独立声道；
否则它吐的就是立体声，没有"方向"可分。

先跑一下自检，看你现在有没有 8 声道端点：

```powershell
# 右键 check-devices.ps1 → 使用 PowerShell 运行
# 或者直接粘这一行：
Get-ChildItem 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render' | % { $p="$($_.PSPath)\Properties"; $n=(Get-ItemProperty $p -EA 0).'{a45c254e-df1c-4efd-8020-67d146a850e0},2'; $f=(Get-ItemProperty $p -EA 0).'{f19f064d-082c-4e27-bc73-6882a1bb8e4c},0'; if($f){ "{0,-42} {1}ch {2}Hz" -f $n,[BitConverter]::ToUInt16($f,2),[BitConverter]::ToUInt32($f,4) } }
```

### 如果没有（大概率）

装 VB-CABLE（免费，1 分钟）：

1. <https://vb-audio.com/Cable/> 下载 `VBCABLE_Driver_Pack43.zip`
2. 解压，右键 `VBCABLE_Setup_x64.exe` → **以管理员身份运行** → `Install Driver`
3. **重启**（必须，驱动要重新枚举设备）
4. 🎯 **关键一步**：`声音设置 → 更多声音设置 → 播放 → CABLE Input → 右键"配置扬声器" → 选 **7.1 环绕**`
5. 把 `CABLE Input` 设为默认播放设备
6. 游戏内音频设置：**扬声器/输出配置 = 7.1 环绕**，关掉游戏自带的耳机虚拟化

> 第 4 步最容易漏。VB-CABLE 装完默认是**立体声**，不在"配置扬声器"里改成 7.1，
> 它只声明 2 声道，游戏照样输出立体声 —— 你会以为工具没用，其实是端点没配。

### 配好之后

| 角色 | 设备 |
|---|---|
| 系统默认播放 | `CABLE Input`（游戏声音被这里接住） |
| pv-spatial 的「源」 | `CABLE Input`（8ch） |
| pv-spatial 的「输出」 | 你的耳机 / DAC（2ch） |

游戏 → CABLE Input（8 声道）→ pv-spatial 抓走 → 处理后送耳机。CABLE Input 本身不要接耳机，
它是"暗管"。

---

## 三、编译

```powershell
cd PureVox-surround\pv-spatial
cargo build --release
# 产物：target\release\pv-spatial.exe（约 3 MB，单文件，无运行时依赖）
```

依赖只有 `rustfft`（卷积）、`rtrb`（无锁环）、`serde_json`、`windows`。不需要 Node、不需要 WebView2、
不需要 Tauri —— 界面是内嵌的 HTML，用浏览器打开。

---

## 四、用

双击 `pv-spatial.exe`，浏览器会自动打开 `http://127.0.0.1:8760/`。

1. **源**选 8 声道那个（列表里带 `★`），**输出**选耳机
2. 点 **启动**
3. 播放游戏 / 视频，界面里的电平条会动
4. 拖 8 个推子

| 推子 | 方位 | 典型用法 |
|---|---|---|
| FL / FR | 前左 / 前右 | 压一点，让前方不盖住脚步 |
| FC | 中置 | 游戏里多是环境音与报点，通常压 |
| LFE | 低频 | 爆炸轰鸣，竞技时压掉 |
| BL / BR | 后左 / 后右 | **抬这个**，脚步声来自后方时更清楚 |
| SL / SR | 侧左 / 侧右 | 侧翼绕后，抬一点 |

预设：`战术·脚步` 就是"压前方、抬后方"，`影院` 反过来。

顶栏三个数字：**处理块** 2.7 ms（128 样本）、**缓冲** 是环里积压的量、**欠载** 是爆音次数。
欠载一直涨就把块长调大（256/480）。

---

## 五、这台机器上验证过什么

| 项 | 状态 |
|---|---|
| DSP（8ch 总线、每声道增益、HRIR 卷积、块长可切换） | ✅ 39 个单测，与 PureVox 补丁同一份文件 |
| 处理线程（环、参数热更新、电平统计） | ✅ 3 个单测：**BL 静音后其他方向能量不变**、换块长不改声音 |
| HTTP / JSON / 静态资源 | ✅ curl 实跑 |
| 界面（8 推子、雷达、预设、设备下拉、电平条） | ✅ Playwright 实跑：8 行、三列对齐、无 console 错误、推子与预设写入生效 |
| **WASAPI 采集与渲染（`audio_windows.rs`，约 440 行）** | ⚠️ **没有在 Windows 上编译过** |

最后一条要说清楚：沙箱里装不上 `x86_64-pc-windows-msvc` target（`rustup target add` 拉不下来），
所以 `audio_windows.rs` 只做到了"逐行对照 PureVox 里已编译通过的 `audio/loopback_windows.rs`"，
**没有实际编译验证**。这部分是纯 WASAPI 调用，逻辑直白，但第一次编译可能有签名不匹配的小错。

真报错了把错误贴出来，都是几行的修。

---

## 六、已知限制

- **只支持 48 kHz**。端点的混音格式不是 48 kHz 时会明确报错，让你去改系统设置 ——
  硬做重采样会引入一个我没法验证的延迟源，不如不做。
- **HRIR 是合成的**（Woodworth 球头模型：双耳时差 + 头影低通），**不是测量数据**。
  方位感不如 HeSuVi / Dolby 精细，但"哪个方向变响了"是准的。
  想换成真 HRIR：`dsp/surround/wav.rs` 已经能解析 16 声道 WAV，
  按「8 音箱 × 2 耳」的顺序排就行。
- **不支持 EQ 曲线**（31 段那些）。`compat/apo.rs` 里的解析器已经能读 Equalizer APO 的
  `config.txt`，但还没接进这条链路 —— 那是下一步。
- 输出端点如果是 5.1/7.1 音箱，会把双耳信号铺到所有声道（不是真环绕）。

---

## 七、和 PureVox 补丁的关系

`pv-spatial/src/dsp.rs` 用 `#[path]` 直接引用 `../src-tauri/src/dsp/surround/mod.rs`——
**同一份文件，不是复制**。所以：

- 改 PureVox 那边的 DSP，这边重编译就同步生效；
- 反过来说，**`pv-spatial/` 目录不能单独拷走**，它依赖 `../src-tauri/` 存在。
  整个 `PureVox-surround/` 一起用。
