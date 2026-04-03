use std::collections::HashSet;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Stdout};
use std::process::Command;
use std::time::{Duration, Instant};

use chrono::Local;
use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table, Wrap};

#[derive(Debug, Clone, Copy)]
struct CpuSnapshot {
    total: u64,
    idle: u64,
}

#[derive(Debug, Clone, Copy)]
struct NetSnapshot {
    recv: u64,
    sent: u64,
}

#[derive(Debug, Clone)]
struct GpuStat {
    name: String,
    util: Option<f64>,
    mem_used_mb: Option<u64>,
    mem_total_mb: Option<u64>,
    temp_c: Option<u64>,
}

/// 进程采样结构，用于计算 CPU 差值占比
#[derive(Debug, Clone)]
struct ProcessSample {
    pid: i32,
    start_time: u64,
    total_time: u64,
    rss_bytes: u64,
    cmdline: String,
}

/// 进程历史 CPU 时间快照
#[derive(Debug, Clone, Copy)]
struct ProcessCpuSnapshot {
    start_time: u64,
    total_time: u64,
}

/// Top 进程展示项
#[derive(Debug, Clone)]
struct ProcessInfo {
    pid: i32,
    cpu_usage: f64,
    rss_bytes: u64,
    cmdline: String,
}

#[derive(Debug)]
struct AppState {
    cpu_model: String,
    memory_model: String,
    cpu_usage: f64,
    mem_used_kb: u64,
    mem_total_kb: u64,
    net_recv_rate_bps: f64,
    net_sent_rate_bps: f64,
    gpus: Vec<GpuStat>,
    gpu_message: Option<String>,
    top_processes: Vec<ProcessInfo>,
    process_message: Option<String>,
    last_updated: Option<Instant>,
    prev_cpu: Option<CpuSnapshot>,
    prev_net: Option<NetSnapshot>,
    prev_proc: HashMap<i32, ProcessCpuSnapshot>,
}

impl AppState {
    fn new() -> Self {
        let cpu_model = read_cpu_model().unwrap_or_else(|_| "Unknown CPU".to_string());
        let memory_model = read_memory_model().unwrap_or_else(|_| {
            "Unknown RAM (try dmidecode as root)".to_string()
        });

        Self {
            cpu_model,
            memory_model,
            cpu_usage: 0.0,
            mem_used_kb: 0,
            mem_total_kb: 0,
            net_recv_rate_bps: 0.0,
            net_sent_rate_bps: 0.0,
            gpus: Vec::new(),
            gpu_message: None,
            top_processes: Vec::new(),
            process_message: None,
            last_updated: None,
            prev_cpu: None,
            prev_net: None,
            prev_proc: HashMap::new(),
        }
    }

    fn refresh(&mut self) {
        let now = Instant::now();
        let elapsed = self
            .last_updated
            .map(|last| now.saturating_duration_since(last))
            .unwrap_or(Duration::from_secs(1));

        let mut cpu_total_delta = 0_u64;
        if let Ok(current_cpu) = read_cpu_snapshot() {
            if let Some(prev) = self.prev_cpu {
                let total_delta = current_cpu.total.saturating_sub(prev.total);
                let idle_delta = current_cpu.idle.saturating_sub(prev.idle);
                if total_delta > 0 {
                    self.cpu_usage = (1.0 - (idle_delta as f64 / total_delta as f64)) * 100.0;
                    cpu_total_delta = total_delta;
                }
            }
            self.prev_cpu = Some(current_cpu);
        }

        if let Ok((used_kb, total_kb)) = read_memory_kb() {
            self.mem_used_kb = used_kb;
            self.mem_total_kb = total_kb;
        }

        if let Ok(current_net) = read_network_totals() {
            if let Some(prev) = self.prev_net {
                let seconds = elapsed.as_secs_f64().max(0.001);
                let recv_delta = current_net.recv.saturating_sub(prev.recv);
                let sent_delta = current_net.sent.saturating_sub(prev.sent);
                self.net_recv_rate_bps = recv_delta as f64 / seconds;
                self.net_sent_rate_bps = sent_delta as f64 / seconds;
            }
            self.prev_net = Some(current_net);
        }

        match read_gpu_stats() {
            Ok(gpus) if !gpus.is_empty() => {
                self.gpus = gpus;
                self.gpu_message = None;
            }
            Ok(_) => {
                self.gpus.clear();
                self.gpu_message = Some("No GPU detected".to_string());
            }
            Err(err) => {
                self.gpus.clear();
                self.gpu_message = Some(format!("GPU read err: {}", err));
            }
        }

        self.refresh_top_processes(cpu_total_delta);
        self.last_updated = Some(now);
    }

    /// 刷新 CPU Top 进程列表（按 CPU 使用率倒序）
    fn refresh_top_processes(&mut self, cpu_total_delta: u64) {
        let samples = match read_process_samples() {
            Ok(v) => v,
            Err(err) => {
                self.top_processes.clear();
                self.process_message = Some(format!("Proc read err: {err}"));
                return;
            }
        };

        let mut next_prev = HashMap::with_capacity(samples.len());
        let mut items = Vec::with_capacity(samples.len());

        for sample in samples {
            let cpu_usage = if cpu_total_delta > 0 {
                if let Some(prev) = self.prev_proc.get(&sample.pid) {
                    if prev.start_time == sample.start_time {
                        let proc_delta = sample.total_time.saturating_sub(prev.total_time);
                        (proc_delta as f64 / cpu_total_delta as f64) * 100.0
                    } else {
                        0.0
                    }
                } else {
                    0.0
                }
            } else {
                0.0
            };

            next_prev.insert(
                sample.pid,
                ProcessCpuSnapshot {
                    start_time: sample.start_time,
                    total_time: sample.total_time,
                },
            );
            items.push(ProcessInfo {
                pid: sample.pid,
                cpu_usage,
                rss_bytes: sample.rss_bytes,
                cmdline: sample.cmdline,
            });
        }

        items.sort_by(|a, b| {
            b.cpu_usage
                .partial_cmp(&a.cpu_usage)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.rss_bytes.cmp(&a.rss_bytes))
        });
        items.truncate(10);

        self.prev_proc = next_prev;
        self.top_processes = items;
        self.process_message = None;
    }

    fn mem_usage_percent(&self) -> f64 {
        if self.mem_total_kb == 0 {
            0.0
        } else {
            (self.mem_used_kb as f64 / self.mem_total_kb as f64) * 100.0
        }
    }

    fn avg_gpu_usage(&self) -> f64 {
        if self.gpus.is_empty() {
            return 0.0;
        }
        let mut sum = 0.0;
        let mut count = 0.0;
        for gpu in &self.gpus {
            if let Some(util) = gpu.util {
                sum += util;
                count += 1.0;
            }
        }
        if count == 0.0 { 0.0 } else { sum / count }
    }

    fn net_activity_percent(&self) -> f64 {
        let total_bps = self.net_recv_rate_bps + self.net_sent_rate_bps;
        let one_gigabit_per_sec = 125.0 * 1024.0 * 1024.0;
        (total_bps / one_gigabit_per_sec * 100.0).clamp(0.0, 100.0)
    }
}

/// 扫描 /proc，读取进程 CPU 时间、RSS 与命令行
fn read_process_samples() -> io::Result<Vec<ProcessSample>> {
    let mut samples = Vec::new();

    for entry in fs::read_dir("/proc")? {
        let entry = match entry {
            Ok(v) => v,
            Err(_) => continue,
        };
        let file_name = entry.file_name();
        let pid_str = file_name.to_string_lossy();
        if !pid_str.as_bytes().iter().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let pid = match pid_str.parse::<i32>() {
            Ok(v) => v,
            Err(_) => continue,
        };

        let stat_path = format!("/proc/{pid}/stat");
        let stat_text = match fs::read_to_string(&stat_path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let (comm, start_time, total_time, rss_pages) = match parse_proc_stat(&stat_text) {
            Some(v) => v,
            None => continue,
        };
        let cmdline = read_process_cmdline(pid).unwrap_or(comm);
        let rss_bytes = rss_pages.saturating_mul(4096);
        samples.push(ProcessSample {
            pid,
            start_time,
            total_time,
            rss_bytes,
            cmdline,
        });
    }

    Ok(samples)
}

/// 解析 /proc/[pid]/stat，提取 comm/start_time/utime+stime/rss
fn parse_proc_stat(stat_text: &str) -> Option<(String, u64, u64, u64)> {
    let open = stat_text.find('(')?;
    let close = stat_text.rfind(')')?;
    if close <= open {
        return None;
    }
    let comm = stat_text[open + 1..close].to_string();
    let tail = stat_text.get(close + 2..)?;
    let fields: Vec<&str> = tail.split_whitespace().collect();
    if fields.len() <= 21 {
        return None;
    }
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    let start_time = fields.get(19)?.parse::<u64>().ok()?;
    let rss_pages = fields.get(21)?.parse::<u64>().ok()?;
    Some((comm, start_time, utime.saturating_add(stime), rss_pages))
}

/// 读取 /proc/[pid]/cmdline，并把 '\0' 转为空格
fn read_process_cmdline(pid: i32) -> Option<String> {
    let path = format!("/proc/{pid}/cmdline");
    let bytes = fs::read(path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let text = bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).to_string())
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() { None } else { Some(text) }
}

fn read_cpu_model() -> io::Result<String> {
    let content = fs::read_to_string("/proc/cpuinfo")?;
    for line in content.lines() {
        if let Some(model) = line.strip_prefix("model name\t:") {
            let value = model.trim();
            if !value.is_empty() {
                return Ok(value.to_string());
            }
        }
    }
    Err(io::Error::other("CPU model field not found"))
}

fn read_memory_model() -> io::Result<String> {
    if let Ok(model) = read_memory_model_dmidecode() {
        return Ok(model);
    }
    Err(io::Error::other("Cannot read RAM model"))
}

fn read_memory_model_dmidecode() -> io::Result<String> {
    let output = Command::new("dmidecode").args(["-t", "memory"]).output()?;
    if !output.status.success() {
        return Err(io::Error::other("dmidecode failed"));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut modules = HashSet::new();
    let mut current_size: Option<String> = None;
    let mut current_kind: Option<String> = None;
    let mut current_speed: Option<String> = None;
    let mut current_part: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "Memory Device" {
            push_memory_module(
                &mut modules,
                &mut current_size,
                &mut current_kind,
                &mut current_speed,
                &mut current_part,
            );
            continue;
        }
        if let Some(v) = trimmed.strip_prefix("Size:") {
            let value = v.trim();
            if value != "No Module Installed" && value != "Unknown" {
                current_size = Some(value.to_string());
            }
        } else if let Some(v) = trimmed.strip_prefix("Type:") {
            let value = v.trim();
            if value != "Unknown" {
                current_kind = Some(value.to_string());
            }
        } else if let Some(v) = trimmed.strip_prefix("Speed:") {
            let value = v.trim();
            if value != "Unknown" {
                current_speed = Some(value.to_string());
            }
        } else if let Some(v) = trimmed.strip_prefix("Part Number:") {
            let value = v.trim();
            if !value.is_empty() && value != "Unknown" {
                current_part = Some(value.to_string());
            }
        }
    }
    push_memory_module(
        &mut modules,
        &mut current_size,
        &mut current_kind,
        &mut current_speed,
        &mut current_part,
    );

    if modules.is_empty() {
        return Err(io::Error::other("No RAM modules parsed"));
    }

    let mut list: Vec<_> = modules.into_iter().collect();
    list.sort();
    Ok(format!("{} slot(s): {}", list.len(), list.join(" | ")))
}

fn push_memory_module(
    modules: &mut HashSet<String>,
    size: &mut Option<String>,
    kind: &mut Option<String>,
    speed: &mut Option<String>,
    part: &mut Option<String>,
) {
    if let Some(size_val) = size.take() {
        let mut fields = vec![size_val];
        if let Some(v) = kind.take() {
            fields.push(v);
        }
        if let Some(v) = speed.take() {
            fields.push(v);
        }
        if let Some(v) = part.take() {
            fields.push(v);
        }
        modules.insert(fields.join(" "));
    } else {
        let _ = kind.take();
        let _ = speed.take();
        let _ = part.take();
    }
}

fn read_cpu_snapshot() -> io::Result<CpuSnapshot> {
    let content = fs::read_to_string("/proc/stat")?;
    let cpu_line = content
        .lines()
        .find(|line| line.starts_with("cpu "))
        .ok_or_else(|| io::Error::other("cpu line missing"))?;
    let mut parts = cpu_line.split_whitespace().skip(1);
    let user: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let nice: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let system: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let idle: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let iowait: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let irq: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let softirq: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let steal: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);

    let idle_all = idle + iowait;
    let total = user + nice + system + idle + iowait + irq + softirq + steal;

    Ok(CpuSnapshot {
        total,
        idle: idle_all,
    })
}

fn read_memory_kb() -> io::Result<(u64, u64)> {
    let content = fs::read_to_string("/proc/meminfo")?;
    let mut total_kb = 0_u64;
    let mut available_kb = 0_u64;

    for line in content.lines() {
        if let Some(value) = line.strip_prefix("MemTotal:") {
            total_kb = value
                .split_whitespace()
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("MemAvailable:") {
            available_kb = value
                .split_whitespace()
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
        }
    }

    let used_kb = total_kb.saturating_sub(available_kb);
    Ok((used_kb, total_kb))
}

fn read_network_totals() -> io::Result<NetSnapshot> {
    let content = fs::read_to_string("/proc/net/dev")?;
    let mut recv_total = 0_u64;
    let mut sent_total = 0_u64;

    for line in content.lines().skip(2) {
        let Some((iface, data)) = line.split_once(':') else {
            continue;
        };
        if iface.trim() == "lo" {
            continue;
        }
        let fields: Vec<&str> = data.split_whitespace().collect();
        if fields.len() >= 16 {
            recv_total = recv_total.saturating_add(fields[0].parse::<u64>().unwrap_or(0));
            sent_total = sent_total.saturating_add(fields[8].parse::<u64>().unwrap_or(0));
        }
    }

    Ok(NetSnapshot {
        recv: recv_total,
        sent: sent_total,
    })
}

fn read_gpu_stats() -> io::Result<Vec<GpuStat>> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu",
            "--format=csv,noheader,nounits",
        ])
        .output();

    let output = match output {
        Ok(out) => out,
        Err(_) => {
            return Err(io::Error::other(
                "nvidia-smi not found (non-NVIDIA, skip)",
            ));
        }
    };

    if !output.status.success() {
        return Err(io::Error::other("nvidia-smi failed"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut gpus = Vec::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split(',').map(|p| p.trim()).collect();
        if parts.len() < 5 {
            continue;
        }
        gpus.push(GpuStat {
            name: parts[0].to_string(),
            util: parts[1].parse::<f64>().ok(),
            mem_used_mb: parts[2].parse::<u64>().ok(),
            mem_total_mb: parts[3].parse::<u64>().ok(),
            temp_c: parts[4].parse::<u64>().ok(),
        });
    }

    Ok(gpus)
}

fn bytes_to_human(bytes: f64) -> String {
    let units = ["B/s", "KB/s", "MB/s", "GB/s", "TB/s"];
    let mut value = bytes.max(0.0);
    let mut idx = 0_usize;

    while value >= 1024.0 && idx < units.len() - 1 {
        value /= 1024.0;
        idx += 1;
    }

    format!("{value:.2} {}", units[idx])
}

fn kib_to_human(kib: u64) -> String {
    let mib = kib as f64 / 1024.0;
    let gib = mib / 1024.0;
    if gib >= 1.0 {
        format!("{gib:.2} GiB")
    } else {
        format!("{mib:.0} MiB")
    }
}

fn bytes_to_human_size(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut idx = 0_usize;
    while value >= 1024.0 && idx < units.len() - 1 {
        value /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{} {}", bytes, units[idx])
    } else {
        format!("{value:.1} {}", units[idx])
    }
}

fn main() -> io::Result<()> {
    let mut terminal = setup_terminal()?;
    let result = run_app(&mut terminal);
    restore_terminal(&mut terminal)?;
    result
}

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // 进入 TUI 前先清空当前终端内容，效果等价于一次 clear。
    execute!(stdout, Clear(ClearType::All), EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    // 退出时离开备用屏幕后再清屏，确保终端残留内容被清空。
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        Clear(ClearType::All),
        MoveTo(0, 0)
    )?;
    terminal.show_cursor()
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    let mut app = AppState::new();
    app.refresh();
    let mut last_tick = Instant::now();
    let tick_rate = Duration::from_millis(1000);

    loop {
        terminal.draw(|f| draw_ui(f, &app))?;

        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? && let Event::Key(key) = event::read()? {
            if key.code == KeyCode::Char('q') || key.code == KeyCode::Esc {
                return Ok(());
            }
        }

        if last_tick.elapsed() >= tick_rate {
            app.refresh();
            last_tick = Instant::now();
        }
    }
}

fn draw_ui(frame: &mut ratatui::Frame<'_>, app: &AppState) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([Constraint::Length(3), Constraint::Min(10)])
        .split(frame.area());

    let lower = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(17), Constraint::Min(8)])
        .split(root[1]);

    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(34), Constraint::Min(40)])
        .split(lower[0]);

    draw_header(frame, root[0]);
    draw_left_utilization(frame, top[0], app);
    draw_right_details(frame, top[1], app);
    draw_process_table(frame, lower[1], app);
}

fn draw_header(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let line = Line::from(vec![
        Span::styled(
            "Linux TUI Monitor  ",
            Style::default().fg(Color::Cyan),
        ),
        Span::raw(format!("Time: {now}  |  q/ESC to quit")),
    ]);
    frame.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn draw_left_utilization(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(area);

    render_gauge(
        frame,
        chunks[0],
        "CPU",
        app.cpu_usage,
        format!("{:.1}%", app.cpu_usage),
        Color::Green,
    );
    render_gauge(
        frame,
        chunks[1],
        "MEM",
        app.mem_usage_percent(),
        format!("{:.1}%", app.mem_usage_percent()),
        Color::Yellow,
    );
    render_gauge(
        frame,
        chunks[2],
        "GPU",
        app.avg_gpu_usage(),
        format!("{:.1}%", app.avg_gpu_usage()),
        Color::Magenta,
    );
    render_gauge(
        frame,
        chunks[3],
        "NET",
        app.net_activity_percent(),
        format!("{:.1}%", app.net_activity_percent()),
        Color::Blue,
    );

    frame.render_widget(
        Paragraph::new("NET normalized to 1Gbps").block(Block::default().borders(Borders::ALL).title("Note")),
        chunks[4],
    );
}

fn render_gauge(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    percent: f64,
    label: String,
    color: Color,
) {
    let ratio = (percent / 100.0).clamp(0.0, 1.0);
    let gauge = Gauge::default()
        .block(Block::default().title(title).borders(Borders::ALL))
        .gauge_style(Style::default().fg(color))
        .ratio(ratio)
        .label(label);
    frame.render_widget(gauge, area);
}

fn draw_right_details(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Length(5), Constraint::Min(8)])
        .split(area);

    let base_lines = vec![
        Line::from(format!("CPU: {}", app.cpu_model)),
        Line::from(format!("RAM: {}", app.memory_model)),
        Line::from(format!(
            "RAM Used: {} / {}",
            kib_to_human(app.mem_used_kb),
            kib_to_human(app.mem_total_kb)
        )),
    ];
    frame.render_widget(
        Paragraph::new(base_lines)
            .block(Block::default().title("HW Info").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        chunks[0],
    );

    let network_lines = vec![
        Line::from(format!("DL: {}", bytes_to_human(app.net_recv_rate_bps))),
        Line::from(format!("UL: {}", bytes_to_human(app.net_sent_rate_bps))),
    ];
    frame.render_widget(
        Paragraph::new(network_lines).block(Block::default().title("Net").borders(Borders::ALL)),
        chunks[1],
    );

    draw_gpu(frame, chunks[2], app);
}

fn draw_gpu(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    let lines = if let Some(msg) = &app.gpu_message {
        vec![Line::from(msg.as_str())]
    } else {
        let mut content = Vec::new();
        for (idx, gpu) in app.gpus.iter().enumerate() {
            content.push(Line::from(Span::styled(
                format!("[GPU {idx}] {}", gpu.name),
                Style::default().fg(Color::Magenta),
            )));
            content.push(Line::from(format!(
                "Util: {:.1}%  Temp: {}°C  VRAM: {} / {} MiB",
                gpu.util.unwrap_or(0.0),
                gpu.temp_c.unwrap_or(0),
                gpu.mem_used_mb.unwrap_or(0),
                gpu.mem_total_mb.unwrap_or(0)
            )));
            content.push(Line::from(""));
        }
        if content.is_empty() {
            vec![Line::from("No GPU info")]
        } else {
            content
        }
    };

    frame.render_widget(Paragraph::new(lines).block(Block::default().title("GPU").borders(Borders::ALL)), area);
}

/// 绘制 CPU 使用率 Top10 进程列表
fn draw_process_table(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    if let Some(msg) = &app.process_message {
        frame.render_widget(
            Paragraph::new(msg.as_str())
                .block(
                    Block::default()
                        .title("CPU Top10")
                        .borders(Borders::ALL),
                ),
            area,
        );
        return;
    }

    let header = Row::new(vec![
        Cell::from("PID"),
        Cell::from("CPU%"),
        Cell::from("RAM"),
        Cell::from("CMD"),
    ])
    .style(Style::default().fg(Color::Cyan));

    let rows: Vec<Row> = app
        .top_processes
        .iter()
        .map(|p| {
            Row::new(vec![
                Cell::from(p.pid.to_string()),
                Cell::from(format!("{:.2}", p.cpu_usage)),
                Cell::from(bytes_to_human_size(p.rss_bytes)),
                Cell::from(p.cmdline.clone()),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(9),
            Constraint::Length(12),
            Constraint::Min(10),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title("CPU Top10")
            .borders(Borders::ALL),
    )
    .column_spacing(1);

    frame.render_widget(table, area);
}
