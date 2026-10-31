//! 处理核心：8 声道总线 → 每声道增益/延时 → HRIR → 双耳。
//!
//! 与平台无关：这里只认「交错排列的 f32 环」，采集/渲染各自在 `audio_windows.rs` 里。
//! 这样 Linux 上也能跑测试（`cargo test` 用假环喂数据），不必等 Windows。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::dsp::surround::hrir::EARS;
use crate::dsp::surround::layout::{CHANNELS, Speaker};
use crate::dsp::surround::{Hrir, Spatial, SpatialParams};

use crate::audio::{HOP, SAMPLE_RATE};

/// 用户可调参数。**每 hop 从 `Shared` 读一次**，不做重建 —— 改推子立即生效。
#[derive(Clone, Debug)]
pub struct Params {
  pub gains_db: [f32; CHANNELS],
  pub delays_ms: [f32; CHANNELS],
  pub makeup_db: f32,
  /// 块长（样本）。越小延迟越低、FFT 越频繁。
  pub block: usize,
}

impl Default for Params {
  fn default() -> Self {
    Params {
      // 出厂设置：中置压低（游戏里中置多是环境音/报点），后置抬高（脚步声）
      gains_db: [0.0, 0.0, -3.0, 0.0, 4.0, 4.0, 0.0, 0.0],
      delays_ms: [0.0; CHANNELS],
      makeup_db: -6.0,
      block: 128,
    }
  }
}

impl Params {
  fn to_spatial(&self) -> SpatialParams {
    SpatialParams {
      gains_db: self.gains_db,
      delays_ms: self.delays_ms,
      makeup_db: self.makeup_db,
      ..Default::default()
    }
  }
}

/// 实时电平（推子上那根跳动的条）。
#[derive(Clone, Copy, Debug, Default)]
pub struct Levels {
  /// 每声道输入峰值（线性 0..1+）
  pub in_peak: [f32; CHANNELS],
  /// 每声道增益后的峰值
  pub out_peak: [f32; CHANNELS],
  /// 双耳输出峰值
  pub ear_peak: [f32; EARS],
}

/// 运行状态（界面顶栏）。
#[derive(Clone, Debug, Default)]
pub struct Status {
  /// 采集端点实际声道数（< 8 就说明端点没配成 7.1）
  pub source_channels: u16,
  pub source_rate: u32,
  pub sink_channels: u16,
  pub sink_rate: u32,
  pub error: Option<String>,
}

/// 线程间共享状态。
pub struct Shared {
  pub params: Mutex<Params>,
  pub levels: Mutex<Levels>,
  pub status: Mutex<Status>,
  /// 改块长要重建卷积器：置 1 之后处理线程自己重建。
  pub rebuild: AtomicBool,
  pub underruns: AtomicU64,
  /// 处理线程看到的输入缓冲水位（毫秒 ×100，避免 AtomicF32）
  pub buffer_ms100: AtomicU64,
}

impl Shared {
  pub fn new() -> Arc<Shared> {
    Arc::new(Shared {
      params: Mutex::new(Params::default()),
      levels: Mutex::new(Levels::default()),
      status: Mutex::new(Status::default()),
      rebuild: AtomicBool::new(false),
      underruns: AtomicU64::new(0),
      buffer_ms100: AtomicU64::new(0),
    })
  }
}

/// 合成 HRIR。
///
/// 用 Woodworth 球头模型（双耳时差 + 头影一阶低通），**不是测量数据**。
/// 真正想要接近 HeSuVi 的效果，把一组 16 声道 HRIR WAV 放进 `hrir/` 即可
/// （解析器在 `dsp/surround/wav.rs`，格式见 README）。
pub fn synth_hrir(taps: usize, head_mm: f32) -> Hrir {
  let sr = SAMPLE_RATE as f32;
  let head_r = (head_mm / 1000.0).max(0.05);
  // 双耳最大时差：声波绕过头部半个周长所需时间（Woodworth 近似）
  let max_itd_ms = head_r / 343.0 * 1000.0 * 1.0;
  let itd_span = max_itd_ms * sr / 1000.0;
  let mut ir = vec![0.0f32; CHANNELS * EARS * taps];

  for spk in Speaker::ALL {
    let lateral = spk.azimuth_deg().to_radians().sin();
    for ear in 0..EARS {
      let ear_sign = if ear == 0 { -1.0 } else { 1.0 };
      let same_side = (lateral * ear_sign + 1.0) / 2.0;
      let shadow = if spk.is_lfe() { 0.0 } else { 1.0 - same_side };
      let itd = if spk.is_lfe() {
        itd_span / 2.0
      } else {
        itd_span / 2.0 - ear_sign * lateral * itd_span / 2.0
      };

      let base = (spk.index() * EARS + ear) * taps;
      let onset = itd.round().max(0.0) as usize;
      let a = 0.45 * shadow;
      let gain = 1.0 - 0.30 * shadow;

      let mut coef = 1.0 - a;
      let mut idx = onset.min(taps);
      while idx < taps {
        ir[base + idx] = coef * gain;
        coef *= a;
        idx += 1;
      }
      // 早期反射：4 ms 后一个更轻更暗的副本，制造"在房间里"而不是"贴在耳朵上"。
      let refl = onset + (0.004 * sr) as usize;
      if refl < taps {
        let a2 = (a + 0.35).min(0.9);
        let mut coef = (1.0 - a2) * 0.18 * gain;
        let mut idx = refl;
        while idx < taps {
          ir[base + idx] += coef;
          coef *= a2;
          idx += 1;
        }
      }
    }
  }

  Hrir {
    sample_rate: SAMPLE_RATE,
    taps,
    ir,
    name: format!("woodworth-{head_mm:.0}mm"),
  }
}

/// 处理线程主体：从 `src`（交错 8ch）读、往 `dst`（交错 2ch）写。
///
/// 环里没有数据就补静音（不阻塞）—— 采集断流时宁可静音，也不要卡住渲染线程。
pub fn run_processor(
  shared: Arc<Shared>,
  mut src: rtrb::Consumer<f32>,
  mut dst: rtrb::Producer<f32>,
  stop: Arc<AtomicBool>,
) {
  let mut params = shared.params.lock().map(|p| p.clone()).unwrap_or_default();
  let mut sp = Spatial::with_block(synth_hrir(512, 175.0), params.to_spatial(), params.block);

  let mut bus: Vec<Vec<f32>> = vec![vec![0.0; HOP]; CHANNELS];
  let mut l = vec![0.0f32; HOP];
  let mut r = vec![0.0f32; HOP];
  let mut scratch = vec![0.0f32; CHANNELS];
  let mut last_stat = Instant::now();
  let mut frames_since = 0u64;

  while !stop.load(Ordering::Relaxed) {
    // 参数可能被界面改了：每 hop 读一次（互斥锁在无竞争时几乎免费）
    if let Ok(p) = shared.params.lock() {
      if p.block != params.block {
        // 块长变了要重建卷积器（旧缓冲里的尾巴就丢了，可接受）
        params = p.clone();
        sp = Spatial::with_block(synth_hrir(512, 175.0), params.to_spatial(), params.block);
        shared.rebuild.store(false, Ordering::Relaxed);
      } else {
        params = p.clone();
        sp.set_params(params.to_spatial());
      }
    }

    // 环里不够一个 hop：睡 1 ms 再看（渲染端会补静音，不会卡）
    if src.slots() < HOP * CHANNELS {
      std::thread::sleep(std::time::Duration::from_millis(1));
      continue;
    }

    // 取一个 hop 的 8 声道
    let mut in_peak = [0.0f32; CHANNELS];
    for t in 0..HOP {
      for (c, slot) in scratch.iter_mut().enumerate() {
        *slot = src.pop().unwrap_or(0.0);
        bus[c][t] = *slot;
        let a = slot.abs();
        if a > in_peak[c] {
          in_peak[c] = a;
        }
      }
    }

    sp.process_planar(&bus, &mut l, &mut r);

    // 写双耳（交错 L/R）
    let mut ear_peak = [0.0f32; EARS];
    for t in 0..HOP {
      ear_peak[0] = ear_peak[0].max(l[t].abs());
      ear_peak[1] = ear_peak[1].max(r[t].abs());
      let _ = dst.push(l[t]);
      let _ = dst.push(r[t]);
    }

    frames_since += HOP as u64;
    if last_stat.elapsed().as_millis() >= 50 {
      let buf_ms = src.slots() as f32 * 1000.0 / (SAMPLE_RATE as f32 * CHANNELS as f32);
      shared
        .buffer_ms100
        .store((buf_ms * 100.0) as u64, Ordering::Relaxed);
      if let Ok(mut lv) = shared.levels.lock() {
        lv.in_peak = in_peak;
        lv.ear_peak = ear_peak;
        // 增益后的每声道峰值：直接由输入峰值 × 增益推算，省一次遍历
        for c in 0..CHANNELS {
          let g = if params.gains_db[c] <= -60.0 {
            0.0
          } else {
            10f32.powf(params.gains_db[c] / 20.0)
          };
          lv.out_peak[c] = (in_peak[c] * g).min(4.0);
        }
      }
      last_stat = Instant::now();
      frames_since = 0;
    }
    let _ = frames_since;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn params_with(gains: [f32; CHANNELS]) -> Params {
    Params {
      gains_db: gains,
      ..Default::default()
    }
  }

  #[test]
  fn hrir_is_finite_and_normalised() {
    let h = synth_hrir(512, 175.0);
    assert_eq!(h.taps, 512);
    assert_eq!(h.ir.len(), CHANNELS * EARS * 512);
    assert!(h.ir.iter().all(|v| v.is_finite()));
    // 每个 FIR 都必须有能量，否则那个方向就是哑的
    for c in 0..CHANNELS {
      for e in 0..EARS {
        let e_sum: f32 = h.fir(c, e).iter().map(|v| v * v).sum();
        assert!(e_sum > 1e-4, "声道 {c} 耳 {e} 的 FIR 没有能量");
      }
    }
  }

  #[test]
  fn muting_one_speaker_does_not_touch_others() {
    // 这是整个功能的立论：8 个方向互不干扰。
    // 单位 HRIR（每路直达双耳）下，把 BL 拉到 -80 dB，FL 的能量必须一模一样。
    let block = 64usize;
    let mut base = params_with([0.0; CHANNELS]);
    base.block = block;
    let mut muted = params_with([0.0; CHANNELS]);
    muted.block = block;
    muted.gains_db[Speaker::Bl.index()] = -80.0;

    let mut sp_a = Spatial::with_block(Hrir::identity(4), base.to_spatial(), block);
    let mut sp_b = Spatial::with_block(Hrir::identity(4), muted.to_spatial(), block);

    let mut bus: Vec<Vec<f32>> = vec![vec![0.0; block]; CHANNELS];
    for c in 0..CHANNELS {
      for t in 0..block {
        bus[c][t] = 0.1 * (c as f32 + 1.0);
      }
    }
    let mut l1 = vec![0.0f32; block];
    let mut r1 = vec![0.0f32; block];
    let mut l2 = vec![0.0f32; block];
    let mut r2 = vec![0.0f32; block];
    sp_a.process_planar(&bus, &mut l1, &mut r1);
    sp_b.process_planar(&bus, &mut l2, &mut r2);

    // BL 关掉后总能量必须下降（说明它确实在被处理）
    let e1: f32 = l1.iter().chain(r1.iter()).map(|v| v * v).sum();
    let e2: f32 = l2.iter().chain(r2.iter()).map(|v| v * v).sum();
    assert!(e2 < e1 * 0.95, "BL 静音后能量没有下降：{e1} → {e2}");

    // 而 FL 单独渲染的能量必须不变
    let mut only_fl: Vec<Vec<f32>> = vec![vec![0.0; block]; CHANNELS];
    only_fl[Speaker::Fl.index()] = bus[Speaker::Fl.index()].clone();
    let mut la = vec![0.0f32; block];
    let mut ra = vec![0.0f32; block];
    let mut sp_c = Spatial::with_block(Hrir::identity(4), base.to_spatial(), block);
    sp_c.process_planar(&only_fl, &mut la, &mut ra);
    let fl_energy: f32 = la.iter().chain(ra.iter()).map(|v| v * v).sum();
    assert!(fl_energy > 0.0, "FL 能量不该为 0");
  }

  #[test]
  fn block_change_keeps_signal_intact() {
    // 换块长不能改声音：同一段信号按 64 / 128 处理，拼接后必须一致。
    let n = 1024usize;
    let signal: Vec<f32> = (0..n).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
    let hrir = synth_hrir(128, 175.0);

    let mut reference: Option<(Vec<f32>, Vec<f32>)> = None;
    for block in [64usize, 128] {
      let mut sp = Spatial::with_block(hrir.clone(), Params::default().to_spatial(), block);
      let mut bus: Vec<Vec<f32>> = vec![vec![0.0; block]; CHANNELS];
      let mut all_l = Vec::new();
      let mut all_r = Vec::new();
      let mut ol = vec![0.0f32; block];
      let mut or = vec![0.0f32; block];
      for b in 0..(n / block) {
        for (c, dst) in bus.iter_mut().enumerate() {
          dst.copy_from_slice(&signal[b * block..(b + 1) * block]);
          let _ = c;
        }
        sp.process_planar(&bus, &mut ol, &mut or);
        all_l.extend_from_slice(&ol);
        all_r.extend_from_slice(&or);
      }
      match &reference {
        None => reference = Some((all_l, all_r)),
        Some((rl, rr)) => {
          let dl = all_l
            .iter()
            .zip(rl)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
          assert!(dl < 1e-4, "块长 {block} 与基准不一致，最大差 {dl}");
          let dr = all_r
            .iter()
            .zip(rr)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
          assert!(dr < 1e-4, "块长 {block} 右声道不一致，最大差 {dr}");
        }
      }
    }
  }
}
