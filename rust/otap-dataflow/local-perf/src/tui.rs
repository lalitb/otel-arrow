// Local performance harness — ratatui live dashboard
//
// Layout:
//
//   ┌────────────────────────────────────────────────────────────────────┐
//   │ df_engine local-perf  │  Scenario: <name>  │  Cores: N  │  HH:MM:SS │
//   ├─────────────────────────────────┬──────────────────────────────────┤
//   │  Throughput (msg/s)             │  Heap Memory                     │
//   │  ▁▂▃▄▅▆▇█ sparkline            │  allocated / resident sparkline   │
//   ├─────────────────────────────────┴──────────────────────────────────┤
//   │  Latency Percentiles                                               │
//   │  P50   P90   P99   P99.9   Max                                     │
//   ├────────────────────────────────────────────────────────────────────┤
//   │  Stats: Generated | Received | Dropped | Efficiency%              │
//   │  Leak:  slope | verdict                                            │
//   └────────────────────────────────────────────────────────────────────┘

use crate::memory::{LeakAnalysis, MemoryTracker, format_bytes, format_bytes_per_sec};
use crate::metrics::{MetricsSnapshot, ThroughputHistory};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table, Wrap},
};
use std::{
    io::{self, Stdout},
    time::Duration,
};

pub struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Tui {
    pub fn init() -> anyhow::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Tui { terminal })
    }

    pub fn restore(&mut self) -> anyhow::Result<()> {
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        self.terminal.show_cursor()?;
        Ok(())
    }

    /// Draw one frame. Returns `true` if the user pressed q/Ctrl-C to quit.
    pub fn draw(
        &mut self,
        scenario: &str,
        snap: &MetricsSnapshot,
        throughput_history: &ThroughputHistory,
        memory_tracker: &MemoryTracker,
        leak: Option<&LeakAnalysis>,
    ) -> anyhow::Result<bool> {
        // Check for quit key before drawing (non-blocking)
        if event::poll(Duration::from_millis(0))? {
            if let Event::Key(key) = event::read()? {
                match (key.code, key.modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        return Ok(true);
                    }
                    _ => {}
                }
            }
        }

        // Prepare sparkline data (u64 values — ratatui Sparkline uses u64)
        let tp_data: Vec<u64> = throughput_history
            .samples
            .iter()
            .map(|&v| v as u64)
            .collect();

        let alloc_data = memory_tracker.allocated_history();
        let resident_data = memory_tracker.resident_history();
        let mem_scale = alloc_data.iter().chain(resident_data.iter()).copied().max().unwrap_or(1);

        let elapsed_secs = snap.elapsed.as_secs();
        let hms = format!("{:02}:{:02}:{:02}", elapsed_secs / 3600, (elapsed_secs % 3600) / 60, elapsed_secs % 60);

        self.terminal.draw(|frame| {
            let area = frame.area();

            // ── Root layout ────────────────────────────────────────────────
            let root = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),   // header
                    Constraint::Min(8),      // charts (throughput + memory)
                    Constraint::Length(5),   // latency table
                    Constraint::Length(4),   // stats + leak
                ])
                .split(area);

            // ── Header ─────────────────────────────────────────────────────
            let header_text = vec![
                Line::from(vec![
                    Span::styled("⚙ df_engine local-perf", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    Span::raw("  │  "),
                    Span::styled(format!("Scenario: {}", scenario), Style::default().fg(Color::Yellow)),
                    Span::raw("  │  "),
                    Span::styled(format!("Cores: {}", snap.num_cores), Style::default().fg(Color::Green)),
                    Span::raw("  │  "),
                    Span::styled(hms, Style::default().fg(Color::White)),
                    Span::raw("  │  Press "),
                    Span::styled("q", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                    Span::raw(" to quit"),
                ]),
            ];
            frame.render_widget(
                Paragraph::new(header_text)
                    .block(Block::default().borders(Borders::ALL))
                    .alignment(Alignment::Left),
                root[0],
            );

            // ── Chart area: throughput (left) + memory (right) ─────────────
            let charts = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(root[1]);

            // Throughput sparkline
            let tp_max = throughput_history.max().max(1.0) as u64;
            let tp_label = format!(
                "{:.2}M msg/s  (peak: {:.2}M)",
                snap.throughput / 1_000_000.0,
                tp_max as f64 / 1_000_000.0
            );
            frame.render_widget(
                Sparkline::default()
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(format!(" Throughput — {} ", tp_label)),
                    )
                    .data(&tp_data)
                    .max(tp_max)
                    .style(Style::default().fg(Color::Green)),
                charts[0],
            );

            // Memory sparkline (allocated vs resident — overlay via color)
            let mem_label = if let Some(last) = alloc_data.last() {
                format!(
                    "alloc: {}  resident: {}",
                    format_bytes(*last as usize),
                    format_bytes(*resident_data.last().unwrap_or(&0) as usize)
                )
            } else {
                "no jemalloc data".to_string()
            };
            // Show allocated bytes sparkline
            let alloc_display = alloc_data.clone();
            frame.render_widget(
                Sparkline::default()
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(format!(" Heap Memory — {} ", mem_label)),
                    )
                    .data(&alloc_display)
                    .max(mem_scale.max(1))
                    .style(Style::default().fg(
                        // Color based on leak verdict
                        match leak {
                            Some(la) if la.verdict == crate::memory::LeakVerdict::Likely => Color::Red,
                            Some(la) if la.verdict == crate::memory::LeakVerdict::Suspicious => Color::Yellow,
                            _ => Color::Blue,
                        },
                    )),
                charts[1],
            );

            // ── Latency table ──────────────────────────────────────────────
            let latency_rows = vec![Row::new(vec![
                Cell::from(format!("{} µs", snap.p50_us)).style(Style::default().fg(Color::Green)),
                Cell::from(format!("{} µs", snap.p90_us)).style(Style::default().fg(Color::Yellow)),
                Cell::from(format!("{} µs", snap.p99_us)).style(Style::default().fg(Color::LightRed)),
                Cell::from(format!("{} µs", snap.p999_us)).style(Style::default().fg(Color::Red)),
                Cell::from(format!("{} µs", snap.max_us)).style(Style::default().fg(Color::Magenta)),
            ])];
            let latency_header = Row::new(vec!["P50", "P90", "P99", "P99.9", "Max"])
                .style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));
            frame.render_widget(
                Table::new(
                    latency_rows,
                    [
                        Constraint::Percentage(20),
                        Constraint::Percentage(20),
                        Constraint::Percentage(20),
                        Constraint::Percentage(20),
                        Constraint::Percentage(20),
                    ],
                )
                .header(latency_header)
                .block(Block::default().borders(Borders::ALL).title(" Latency ")),
                root[2],
            );

            // ── Stats + leak verdict ───────────────────────────────────────
            let efficiency = if snap.generated > 0 {
                snap.received * 100 / snap.generated
            } else {
                0
            };

            let leak_line = if let Some(la) = leak {
                format!(
                    "Memory leak: {} | slope {} | R²={:.3} | growth {}",
                    la.verdict,
                    format_bytes_per_sec(la.slope_bytes_per_sec),
                    la.r_squared,
                    format_bytes(la.net_growth_bytes)
                )
            } else {
                "Memory leak: insufficient data (need jemalloc feature)".to_string()
            };

            let stats_text = vec![
                Line::from(vec![
                    Span::raw("Generated: "),
                    Span::styled(format!("{}", snap.generated), Style::default().fg(Color::Cyan)),
                    Span::raw("  Received: "),
                    Span::styled(format!("{}", snap.received), Style::default().fg(Color::Green)),
                    Span::raw("  Dropped: "),
                    Span::styled(format!("{}", snap.dropped), Style::default().fg(Color::Red)),
                    Span::raw(format!("  Efficiency: {}%", efficiency)),
                ]),
                Line::from(Span::styled(
                    leak_line,
                    Style::default().fg(match leak {
                        Some(la) if la.verdict == crate::memory::LeakVerdict::Likely => Color::Red,
                        Some(la) if la.verdict == crate::memory::LeakVerdict::Suspicious => Color::Yellow,
                        _ => Color::Green,
                    }),
                )),
            ];
            frame.render_widget(
                Paragraph::new(stats_text)
                    .block(Block::default().borders(Borders::ALL).title(" Stats "))
                    .wrap(Wrap { trim: true }),
                root[3],
            );
        })?;

        Ok(false)
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
