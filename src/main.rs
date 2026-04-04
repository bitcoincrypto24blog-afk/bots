//! ws-latency-monitor
//! ─────────────────────────────────────────────────────────────────────────────
//! Measures real-time WebSocket round-trip latency (ms) for:
//!   • Binance     wss://stream.binance.com:9443
//!   • Coinbase    wss://advanced-trade-ws.coinbase.com
//!   • Polymarket  wss://ws-subscriptions-clob.polymarket.com
//!   • Mexico      wss://wbs.mexc.com
//!
//! Metrics per exchange (rolling window):
//!   min · avg · max · p50 · p95 · p99 · jitter (std-dev) · packet loss %
//!
//! Cloud-ready:
//!   • All config via env-vars or CLI flags
//!   • Structured log lines to stdout (RUST_LOG=info)
//!   • ANSI colours auto-disabled when stdout is not a TTY
//!   • Automatic reconnect with exponential back-off
//!
//! Usage:
//!   cargo run --release
//!   cargo run --release -- --interval 500 --report 5 --window 200
//!   PING_INTERVAL_MS=500 REPORT_INTERVAL_SEC=5 cargo run --release

use anyhow::{anyhow, Result};
use chrono::Utc;
use clap::Parser;
use colored::Colorize;
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, time};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

// ─────────────────────────────────────────────────────────────────────────────
// Configuration (CLI flags ↔ env-vars)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug, Clone)]
#[command(
    name    = "ws-latency-monitor",
    version = "1.0.0",
    about   = "Real-time WebSocket latency monitor — Binance · Coinbase · Polymarket · Mexico"
)]
struct Config {
    /// WebSocket ping interval in milliseconds
    #[arg(short, long, default_value = "1000", env = "PING_INTERVAL_MS")]
    interval: u64,

    /// Stats report interval in seconds
    #[arg(short, long, default_value = "10", env = "REPORT_INTERVAL_SEC")]
    report: u64,

    /// Rolling sample window size
    #[arg(short, long, default_value = "100", env = "WINDOW_SIZE")]
    window: usize,

    /// Ping timeout in milliseconds (counts as packet loss above this)
    #[arg(long, default_value = "5000", env = "PING_TIMEOUT_MS")]
    timeout: u64,

    /// Maximum reconnect attempts per exchange (0 = unlimited)
    #[arg(long, default_value = "0", env = "MAX_RECONNECTS")]
    max_reconnects: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// Exchange definitions
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Exchange {
    name:          &'static str,
    url:           &'static str,
    /// Optional JSON subscription payload sent immediately after connect
    subscribe_msg: Option<&'static str>,
}

fn exchange_list() -> Vec<Exchange> {
    vec![
        Exchange {
            name: "Binance",
            url:  "wss://stream.binance.com:9443/ws/btcusdt@aggTrade",
            // Subscribes automatically via stream URL; no extra message needed.
            subscribe_msg: None,
        },
        Exchange {
            name: "Coinbase",
            url:  "wss://advanced-trade-ws.coinbase.com",
            subscribe_msg: Some(
                r#"{"type":"subscribe","product_ids":["BTC-USD"],"channel":"heartbeats"}"#,
            ),
        },
        Exchange {
            name: "Polymarket",
            url:  "wss://ws-subscriptions-clob.polymarket.com/ws/",
            subscribe_msg: Some(r#"{"assets_ids":[],"type":"market"}"#),
        },
        Exchange {
            name: "Mexico",
            url:  "wss://wbs.mexc.com/ws",
            subscribe_msg: Some(
                r#"{"method":"SUBSCRIPTION","params":["spot@public.deals.v3.api@BTCUSDT"]}"#,
            ),
        },
    ]
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal event channel (worker → aggregator)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum Event {
    Latency      { exchange: &'static str, ms: f64 },
    Timeout      { exchange: &'static str },
    Connected    { exchange: &'static str },
    Disconnected { exchange: &'static str },
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-exchange metrics (rolling window)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct Metrics {
    samples:    VecDeque<f64>,
    window:     usize,
    total_sent: u64,   // pings sent
    timeouts:   u64,   // pings that timed out
    reconnects: u32,
    connected:  bool,
}

impl Metrics {
    fn new(window: usize) -> Self {
        Self {
            samples:    VecDeque::with_capacity(window),
            window,
            total_sent: 0,
            timeouts:   0,
            reconnects: 0,
            connected:  false,
        }
    }

    fn push(&mut self, ms: f64) {
        if self.samples.len() == self.window {
            self.samples.pop_front();
        }
        self.samples.push_back(ms);
        self.total_sent += 1;
    }

    fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    fn min(&self) -> f64 {
        self.samples.iter().cloned().fold(f64::INFINITY, f64::min)
    }

    fn max(&self) -> f64 {
        self.samples.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
    }

    fn avg(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        self.samples.iter().sum::<f64>() / self.samples.len() as f64
    }

    /// Returns the p-th percentile (0–100).
    fn percentile(&self, p: f64) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let mut s: Vec<f64> = self.samples.iter().cloned().collect();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let idx = ((p / 100.0) * (s.len() - 1) as f64).round() as usize;
        s[idx.min(s.len() - 1)]
    }

    /// Standard deviation of the sample window (network jitter).
    fn jitter(&self) -> f64 {
        if self.samples.len() < 2 {
            return 0.0;
        }
        let avg = self.avg();
        let var = self.samples.iter().map(|x| (x - avg).powi(2)).sum::<f64>()
            / self.samples.len() as f64;
        var.sqrt()
    }

    /// Packet loss percentage over total pings sent.
    fn loss_pct(&self) -> f64 {
        if self.total_sent == 0 {
            return 0.0;
        }
        self.timeouts as f64 / self.total_sent as f64 * 100.0
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Exchange worker — runs one WebSocket session, reconnects on error
// ─────────────────────────────────────────────────────────────────────────────

async fn run_exchange(
    exch:          Exchange,
    tx:            mpsc::Sender<Event>,
    ping_interval: Duration,
    timeout_ms:    u64,
    max_reconnects: u32,
) {
    let mut attempts = 0u32;

    loop {
        info!("[{}] connecting to {}", exch.name, exch.url);

        match ws_session(&exch, &tx, ping_interval, timeout_ms).await {
            Ok(_)  => warn!("[{}] session closed cleanly", exch.name),
            Err(e) => error!("[{}] session error: {e}", exch.name),
        }

        let _ = tx.send(Event::Disconnected { exchange: exch.name }).await;

        attempts += 1;
        if max_reconnects > 0 && attempts >= max_reconnects {
            error!(
                "[{}] reached max reconnects ({}), stopping",
                exch.name, max_reconnects
            );
            break;
        }

        // Exponential back-off: 500 ms · min(attempt, 20)
        let wait_ms = 500_u64 * (attempts.min(20) as u64);
        warn!(
            "[{}] reconnecting in {}ms (attempt {})",
            exch.name, wait_ms, attempts
        );
        time::sleep(Duration::from_millis(wait_ms)).await;
    }
}

/// Runs a single WebSocket session until it closes or errors.
async fn ws_session(
    exch:          &Exchange,
    tx:            &mpsc::Sender<Event>,
    ping_interval: Duration,
    timeout_ms:    u64,
) -> Result<()> {
    let url = url::Url::parse(exch.url)?;

    let (ws_stream, _) = connect_async(url)
        .await
        .map_err(|e| anyhow!("[{}] connect failed — {e}", exch.name))?;

    let _ = tx.send(Event::Connected { exchange: exch.name }).await;
    info!("[{}] ✓ connected", exch.name);

    let (mut sink, mut stream) = ws_stream.split();

    // Send subscription payload if the exchange requires it
    if let Some(msg) = exch.subscribe_msg {
        sink.send(Message::Text(msg.to_string())).await?;
    }

    let timeout        = Duration::from_millis(timeout_ms);
    let mut ticker     = time::interval(ping_interval);
    let mut pending_at: Option<Instant> = None;   // when did we send the last ping?

    loop {
        tokio::select! {
            biased;

            // ── Inbound frame ─────────────────────────────────────────────────
            frame = stream.next() => {
                match frame {
                    Some(Ok(Message::Pong(_))) => {
                        // Record latency from the in-flight ping
                        if let Some(sent) = pending_at.take() {
                            let ms = sent.elapsed().as_secs_f64() * 1_000.0;
                            let _ = tx.send(Event::Latency { exchange: exch.name, ms }).await;
                        }
                    }

                    Some(Ok(Message::Ping(data))) => {
                        // Server-side ping (Binance sends these every ~3 min)
                        sink.send(Message::Pong(data)).await?;
                    }

                    Some(Ok(Message::Close(_))) => {
                        return Err(anyhow!("[{}] server sent Close frame", exch.name));
                    }

                    Some(Err(e)) => {
                        return Err(anyhow!("[{}] stream error — {e}", exch.name));
                    }

                    None => {
                        return Err(anyhow!("[{}] stream ended unexpectedly", exch.name));
                    }

                    // Text / Binary market data — consume silently
                    _ => {}
                }
            }

            // ── Periodic ping ─────────────────────────────────────────────────
            _ = ticker.tick() => {
                // Did the previous ping time out?
                if let Some(sent) = pending_at {
                    if sent.elapsed() > timeout {
                        warn!("[{}] ping timeout (>{timeout_ms}ms)", exch.name);
                        let _ = tx.send(Event::Timeout { exchange: exch.name }).await;
                        pending_at = None;
                    }
                }

                // Send a new ping (empty payload; timestamp is tracked via Instant)
                sink.send(Message::Ping(vec![])).await?;
                pending_at = Some(Instant::now());
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Aggregator — collects events and drives the reporter
// ─────────────────────────────────────────────────────────────────────────────

async fn aggregator(
    mut rx:          mpsc::Receiver<Event>,
    report_interval: Duration,
    window:          usize,
) {
    const NAMES: [&str; 4] = ["Binance", "Coinbase", "Polymarket", "Mexico"];

    let mut metrics: HashMap<&str, Metrics> = NAMES
        .iter()
        .map(|&n| (n, Metrics::new(window)))
        .collect();

    let mut report_ticker = time::interval(report_interval);
    report_ticker.tick().await; // skip the first immediate tick

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(Event::Latency { exchange, ms }) => {
                        if let Some(m) = metrics.get_mut(exchange) {
                            m.push(ms);
                        }
                        print_live(exchange, ms);
                    }

                    Some(Event::Timeout { exchange }) => {
                        if let Some(m) = metrics.get_mut(exchange) {
                            m.total_sent += 1;
                            m.timeouts   += 1;
                        }
                        eprintln!("{}", format!("  ⚠  TIMEOUT  {exchange}").red().bold());
                    }

                    Some(Event::Connected { exchange }) => {
                        if let Some(m) = metrics.get_mut(exchange) {
                            m.connected = true;
                        }
                        println!("{}", format!("  ✓  {exchange} connected").green());
                    }

                    Some(Event::Disconnected { exchange }) => {
                        if let Some(m) = metrics.get_mut(exchange) {
                            m.connected  = false;
                            m.reconnects += 1;
                        }
                        println!("{}", format!("  ✗  {exchange} disconnected").red());
                    }

                    None => break,   // all senders dropped → shutdown
                }
            }

            _ = report_ticker.tick() => {
                print_report(&metrics);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Display helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Colour a latency value: green < 50 ms · yellow < 150 ms · red ≥ 150 ms
fn color_ms(ms: f64) -> String {
    let s = format!("{:>9.3} ms", ms);
    if ms < 50.0 {
        s.green().to_string()
    } else if ms < 150.0 {
        s.yellow().to_string()
    } else {
        s.red().to_string()
    }
}

/// Exchange name coloured by brand
fn color_name(name: &str) -> String {
    match name {
        "Binance"    => format!("{:>11}", name).yellow().bold().to_string(),
        "Coinbase"   => format!("{:>11}", name).blue().bold().to_string(),
        "Polymarket" => format!("{:>11}", name).magenta().bold().to_string(),
        "Mexico"     => format!("{:>11}", name).cyan().bold().to_string(),
        _            => format!("{:>11}", name).white().bold().to_string(),
    }
}

/// One-line live output for each pong received
fn print_live(exchange: &str, ms: f64) {
    let ts  = Utc::now().format("%H:%M:%S%.3f");
    let dot = if ms < 50.0 {
        "●".green()
    } else if ms < 150.0 {
        "◉".yellow()
    } else {
        "○".red()
    };
    println!("{ts}  {dot}  {}  {}", color_name(exchange), color_ms(ms));
}

/// Full statistics report printed every `report_interval` seconds
fn print_report(metrics: &HashMap<&str, Metrics>) {
    let now = Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
    let div = "═".repeat(58);

    println!("\n{}", div.cyan());
    println!(
        "{}",
        format!("  📊  LATENCY REPORT  ·  {now}").cyan().bold()
    );
    println!("{}", div.cyan());

    for name in &["Binance", "Coinbase", "Polymarket", "Mexico"] {
        let Some(m) = metrics.get(name) else { continue };

        let status = if m.connected {
            "🟢 LIVE".green().to_string()
        } else {
            "🔴 DOWN".red().to_string()
        };

        println!();
        println!(
            "  {}  {}   samples: {}   reconnects: {}",
            color_name(name),
            status,
            m.total_sent,
            m.reconnects
        );

        if m.is_empty() {
            println!("    {}", "No data yet…".dimmed());
            continue;
        }

        // Box drawing for readability on any cloud log viewer
        println!("  ┌──────────────────────────────────────────┐");
        println!("  │  Min    : {}", color_ms(m.min()));
        println!("  │  Avg    : {}", color_ms(m.avg()));
        println!("  │  Max    : {}", color_ms(m.max()));
        println!("  │  P50    : {}", color_ms(m.percentile(50.0)));
        println!("  │  P95    : {}", color_ms(m.percentile(95.0)));
        println!("  │  P99    : {}", color_ms(m.percentile(99.0)));
        println!("  │  Jitter : {}", color_ms(m.jitter()));
        println!(
            "  │  Loss   : {:>9.2} %",
            m.loss_pct()
        );
        println!("  └──────────────────────────────────────────┘");
    }

    println!("\n{}\n", div.cyan());
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::parse();

    // Logging: default to "warn" so exchange errors appear but not debug noise.
    // Set RUST_LOG=info to see connection events.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .compact()
        .init();

    println!(
        "{}",
        "╔══════════════════════════════════════════════════════════════════╗".cyan()
    );
    println!(
        "{}",
        "║  🚀  ws-latency-monitor  ·  Binance · Coinbase · Poly · Mexico  ║"
            .cyan()
            .bold()
    );
    println!(
        "{}",
        "╚══════════════════════════════════════════════════════════════════╝".cyan()
    );
    println!(
        "  Interval : {}ms  |  Window : {} samples  |  Report : {}s  |  Timeout : {}ms\n",
        cfg.interval, cfg.window, cfg.report, cfg.timeout
    );

    // Bounded channel — 4096 events buffered before back-pressure
    let (tx, rx) = mpsc::channel::<Event>(4096);

    let ping_iv    = Duration::from_millis(cfg.interval);
    let timeout_ms = cfg.timeout;
    let max_rc     = cfg.max_reconnects;

    // Spawn one Tokio task per exchange
    for exch in exchange_list() {
        let tx2 = tx.clone();
        tokio::spawn(async move {
            run_exchange(exch, tx2, ping_iv, timeout_ms, max_rc).await;
        });
    }

    // Drop the original sender so the channel closes when all workers exit
    drop(tx);

    // Aggregator runs on the main task until the channel closes
    aggregator(rx, Duration::from_secs(cfg.report), cfg.window).await;

    Ok(())
}
