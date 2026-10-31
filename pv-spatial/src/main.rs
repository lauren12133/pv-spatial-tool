//! pv-spatial —— 8 声道虚拟 7.1 调音台
//!
//! 链路：`游戏(7.1) → 虚拟端点 → WASAPI loopback 抓 8 声道 → 每声道增益/延时
//!        → HRIR 卷积 → 双耳 2 声道 → 耳机`
//!
//! 为什么不做进 PureVox：它的引擎是 **F32 单声道**（`ARCHITECTURE.md` §2），
//! 采集回调第一行就把 N 声道求平均成 1 声道（`audio/capture.rs:221-233`）。
//! 8 个推子在那条链路上只能合成一个标量，`Σ(w_c·g_c)·x`，物理上不可能分方向。
//!
//! 这个工具走的是**旁路**：自己开 loopback 和自己的渲染流，完全不碰引擎的单声道通路。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

mod audio {
  /// 全链路统一 48 kHz：I48k 是 Windows 混音的默认值，也是 HRIR 的常见采样率。
  pub const SAMPLE_RATE: u32 = 48_000;
  /// 引擎 hop（10 ms）。处理块长可以更小，但取数据的单位是它。
  pub const HOP: usize = 480;
}

mod dsp;

mod engine;

#[cfg(windows)]
#[path = "audio_windows.rs"]
mod audio_platform;

#[cfg(not(windows))]
#[path = "audio_stub.rs"]
mod audio_platform;

use engine::Shared;

const PORT: u16 = 8760;

/// 正在跑的音频会话。
struct Runtime {
  stop: Arc<AtomicBool>,
  threads: Vec<std::thread::JoinHandle<()>>,
  source_label: String,
  sink_label: String,
}

static RT: Mutex<Option<Runtime>> = Mutex::new(None);

/// 端点枚举缓存。
///
/// 界面 100 ms 轮询一次，而每次 `enum_endpoints` 都要走 COM：创建枚举器、逐个
/// `Activate` + `GetMixFormat`，几十毫秒起步。不缓存的话光是轮询就能烧掉半核。
/// 插拔设备后点界面上的"刷新设备"清空缓存。
static EPS: Mutex<Vec<audio_platform::Endpoint>> = Mutex::new(Vec::new());

fn endpoints_cached() -> Vec<audio_platform::Endpoint> {
  let mut g = match EPS.lock() {
    Ok(g) => g,
    Err(_) => return Vec::new(),
  };
  if g.is_empty() {
    if let Ok(v) = audio_platform::enum_endpoints() {
      *g = v;
    }
  }
  g.clone()
}

fn main() {
  let shared = Shared::new();

  // 先枚举一次端点，让界面一打开就有内容
  let endpoints = endpoints_cached();
  if endpoints.is_empty() {
    eprintln!("[警告] 没有枚举到任何输出端点。");
  } else {
    println!("枚举到 {} 个输出端点：", endpoints.len());
    for e in &endpoints {
      println!(
        "  {}{}",
        e.label,
        if e.is_surround() { "   ← 8 声道，可作 7.1 源" } else { "" }
      );
    }
  }

  let listener = TcpListener::bind(("127.0.0.1", PORT)).unwrap_or_else(|e| {
    eprintln!("端口 {PORT} 被占用：{e}");
    std::process::exit(1);
  });
  let url = format!("http://127.0.0.1:{PORT}/");
  println!("\n界面：{url}  （Ctrl+C 退出）\n");
  open_browser(&url);

  for stream in listener.incoming().flatten() {
    let sh = shared.clone();
    std::thread::spawn(move || {
      let _ = handle(stream, sh);
    });
  }
}

fn open_browser(url: &str) {
  #[cfg(windows)]
  let _ = std::process::Command::new("cmd")
    .args(["/C", "start", "", url])
    .spawn();
  #[cfg(target_os = "macos")]
  let _ = std::process::Command::new("open").arg(url).spawn();
  #[cfg(all(unix, not(target_os = "macos")))]
  let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

// ---------------------------------------------------------------- HTTP

fn handle(mut s: TcpStream, shared: Arc<Shared>) -> std::io::Result<()> {
  let mut reader = BufReader::new(s.try_clone()?);
  let mut line = String::new();
  if reader.read_line(&mut line)? == 0 {
    return Ok(());
  }
  let mut parts = line.split_whitespace();
  let method = parts.next().unwrap_or("").to_string();
  let path = parts.next().unwrap_or("/").to_string();

  // 读头部，主要为了 Content-Length
  let mut len = 0usize;
  loop {
    let mut h = String::new();
    if reader.read_line(&mut h)? == 0 {
      break;
    }
    let t = h.trim();
    if t.is_empty() {
      break;
    }
    if let Some(v) = t.to_ascii_lowercase().strip_prefix("content-length:") {
      len = v.trim().parse().unwrap_or(0);
    }
  }
  let mut body = vec![0u8; len];
  if len > 0 {
    reader.read_exact(&mut body)?;
  }

  let (ctype, payload): (&str, Vec<u8>) = match (method.as_str(), path.as_str()) {
    ("GET", "/") | ("GET", "/index.html") => ("text/html; charset=utf-8", INDEX.as_bytes().to_vec()),
    ("GET", "/app.js") => ("text/javascript; charset=utf-8", APP_JS.as_bytes().to_vec()),
    ("GET", "/style.css") => ("text/css; charset=utf-8", STYLE.as_bytes().to_vec()),
    ("GET", "/api/state") => ("application/json", state_json(&shared).to_string().into_bytes()),
    ("POST", "/api/params") => ("application/json", set_params(&shared, &body).into_bytes()),
    ("POST", "/api/start") => ("application/json", start(&shared, &body).into_bytes()),
    ("POST", "/api/stop") => ("application/json", stop().into_bytes()),
    ("POST", "/api/refresh") => ("application/json", refresh().into_bytes()),
    _ => ("text/plain; charset=utf-8", b"404".to_vec()),
  };

  let head = format!(
    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
    payload.len()
  );
  s.write_all(head.as_bytes())?;
  s.write_all(&payload)?;
  s.flush()
}

fn state_json(shared: &Arc<Shared>) -> Value {
  let p = shared.params.lock().map(|p| p.clone()).unwrap_or_default();
  let lv = shared.levels.lock().map(|l| *l).unwrap_or_default();
  let (running, src, snk, err) = match RT.lock().ok().and_then(|g| g.as_ref().map(|r| {
    (true, r.source_label.clone(), r.sink_label.clone(), None::<String>)
  })) {
    Some(v) => v,
    None => (false, String::new(), String::new(), None),
  };
  let st = shared.status.lock().map(|s| s.clone()).unwrap_or_default();
  let endpoints: Vec<Value> = endpoints_cached()
    .iter()
    .map(|e| {
      json!({
        "id": e.id, "label": e.label, "channels": e.channels,
        "default": e.default, "surround": e.is_surround(),
      })
    })
    .collect();

  json!({
    "running": running,
    "source": src,
    "sink": snk,
    "error": err.or(st.error.clone()),
    "endpoints": endpoints,
    "params": {
      "gains_db": p.gains_db,
      "delays_ms": p.delays_ms,
      "makeup_db": p.makeup_db,
      "block": p.block,
    },
    "levels": {
      "in_peak": lv.in_peak,
      "out_peak": lv.out_peak,
      "ear_peak": lv.ear_peak,
    },
    "status": {
      "block_ms": params_block_ms(p.block),
      "buffer_ms": shared.buffer_ms100.load(Ordering::Relaxed) as f32 / 100.0,
      "underruns": shared.underruns.load(Ordering::Relaxed),
      "source_channels": st.source_channels,
      "source_rate": st.source_rate,
      "sink_channels": st.sink_channels,
      "sink_rate": st.sink_rate,
    }
  })
}

fn params_block_ms(block: usize) -> f32 {
  block as f32 * 1000.0 / audio::SAMPLE_RATE as f32
}

fn set_params(shared: &Arc<Shared>, body: &[u8]) -> String {
  let Ok(v) = serde_json::from_slice::<Value>(body) else {
    return json!({"ok": false, "error": "body 不是合法 JSON"}).to_string();
  };
  let Ok(mut p) = shared.params.lock() else {
    return json!({"ok": false, "error": "内部锁失败"}).to_string();
  };
  if let Some(arr) = v.get("gains_db").and_then(|x| x.as_array()) {
    for (i, item) in arr.iter().take(8).enumerate() {
      if let Some(d) = item.as_f64() {
        p.gains_db[i] = (d as f32).clamp(-60.0, 12.0);
      }
    }
  }
  if let Some(arr) = v.get("delays_ms").and_then(|x| x.as_array()) {
    for (i, item) in arr.iter().take(8).enumerate() {
      if let Some(d) = item.as_f64() {
        p.delays_ms[i] = (d as f32).clamp(0.0, 25.0);
      }
    }
  }
  if let Some(d) = v.get("makeup_db").and_then(|x| x.as_f64()) {
    p.makeup_db = (d as f32).clamp(-30.0, 12.0);
  }
  if let Some(b) = v.get("block").and_then(|x| x.as_u64()) {
    let b = b as usize;
    if (16..=8192).contains(&b) {
      p.block = b;
    }
  }
  json!({"ok": true}).to_string()
}

fn start(shared: &Arc<Shared>, body: &[u8]) -> String {
  if RT.lock().map(|g| g.is_some()).unwrap_or(false) {
    let _ = stop();
  }
  let v: Value = serde_json::from_slice(body).unwrap_or(json!({}));
  let source = v.get("source").and_then(|x| x.as_str()).map(|s| s.to_string());
  let sink = v.get("sink").and_then(|x| x.as_str()).map(|s| s.to_string());

  let stop_flag = Arc::new(AtomicBool::new(false));
  // 输入环 2 s（8 声道交错），输出环 0.5 s（2 声道交错）
  let (in_prod, in_cons) = rtrb::RingBuffer::<f32>::new(audio::SAMPLE_RATE as usize * 2 * 8);
  let (out_prod, out_cons) = rtrb::RingBuffer::<f32>::new(audio::SAMPLE_RATE as usize / 2 * 2);

  let (tx_cap, rx_cap) = std::sync::mpsc::channel();
  let (tx_ren, rx_ren) = std::sync::mpsc::channel();

  // 先起渲染：它是"主时钟"，先跑起来处理线程才有地方写
  let ren = audio_platform::start_render(sink.clone(), out_cons, stop_flag.clone(), tx_ren);
  match rx_ren.recv_timeout(Duration::from_secs(5)) {
    Ok(Ok((ch, _rate))) => {
      if let Ok(mut st) = shared.status.lock() {
        st.sink_channels = ch;
      }
    }
    Ok(Err(e)) => {
      stop_flag.store(true, Ordering::Relaxed);
      return json!({"ok": false, "error": format!("输出设备打不开：{e}")}).to_string();
    }
    Err(_) => {
      stop_flag.store(true, Ordering::Relaxed);
      return json!({"ok": false, "error": "输出设备 5 秒无响应"}).to_string();
    }
  }

  let cap = audio_platform::start_capture(source.clone(), in_prod, stop_flag.clone(), tx_cap);
  match rx_cap.recv_timeout(Duration::from_secs(5)) {
    Ok(Ok((ch, _rate))) => {
      if let Ok(mut st) = shared.status.lock() {
        st.source_channels = ch;
        if ch < 8 {
          st.error = Some(format!(
            "源端点是 {ch} 声道，不是 8 声道 —— 方向信息不存在，推子只会改总音量。\n\
             把该端点（VB-CABLE 之类）在 声音设置 → 配置扬声器 里改成 7.1 环绕。"
          ));
        } else {
          st.error = None;
        }
      }
    }
    Ok(Err(e)) => {
      stop_flag.store(true, Ordering::Relaxed);
      let _ = std::thread::spawn(move || {
        let _ = ren.join();
      });
      return json!({"ok": false, "error": format!("源设备打不开：{e}")}).to_string();
    }
    Err(_) => {
      stop_flag.store(true, Ordering::Relaxed);
      return json!({"ok": false, "error": "源设备 5 秒无响应"}).to_string();
    }
  }

  let sh2 = shared.clone();
  let st2 = stop_flag.clone();
  let proc = std::thread::Builder::new()
    .name("pv-proc".into())
    .spawn(move || engine::run_processor(sh2, in_cons, out_prod, st2))
    .expect("spawn processor");

  let src_label = source.clone().unwrap_or_else(|| "系统默认输出".into());
  let snk_label = sink.clone().unwrap_or_else(|| "系统默认输出".into());
  if let Ok(mut g) = RT.lock() {
    *g = Some(Runtime {
      stop: stop_flag,
      threads: vec![cap, proc, ren],
      source_label: src_label,
      sink_label: snk_label,
    });
  }
  json!({"ok": true}).to_string()
}

fn refresh() -> String {
  if let Ok(mut g) = EPS.lock() {
    g.clear();
  }
  let n = endpoints_cached().len();
  json!({"ok": true, "count": n}).to_string()
}

fn stop() -> String {
  let rt = RT.lock().ok().and_then(|mut g| g.take());
  match rt {
    Some(rt) => {
      rt.stop.store(true, Ordering::Relaxed);
      for h in rt.threads {
        let _ = h.join();
      }
      json!({"ok": true}).to_string()
    }
    None => json!({"ok": true, "note": "本来就没在跑"}).to_string(),
  }
}

const INDEX: &str = include_str!("web/index.html");
const APP_JS: &str = include_str!("web/app.js");
const STYLE: &str = include_str!("web/style.css");
