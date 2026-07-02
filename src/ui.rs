use crate::app::App;
use crate::backend::GpuSnapshot;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Gauge, Paragraph, Sparkline};

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let t = &app.theme;
    frame.render_widget(Block::new().style(Style::new().bg(t.bg).fg(t.fg)), area);

    if app.splash_active() {
        crate::splash::render(frame, area, app.started, &app.splash_path, t);
        return;
    }

    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);

    let mut head = vec![
        Span::styled(format!(" gpur v{} ", env!("CARGO_PKG_VERSION")), t.title),
        Span::styled(format!("[{}] ", app.backend.name()), t.dim),
        Span::styled(format!("{}ms ", app.tick_ms), t.dim),
    ];
    if app.paused {
        head.push(Span::styled("PAUSED ", t.temp_warn));
    }
    frame.render_widget(Paragraph::new(Line::from(head)), header);

    if app.gpus.is_empty() {
        frame.render_widget(
            Paragraph::new("no GPUs reported by backend").style(t.dim),
            body,
        );
    } else {
        let rows = Layout::vertical(
            app.gpus
                .iter()
                .map(|_| Constraint::Ratio(1, app.gpus.len() as u32)),
        )
        .split(body);
        for (i, gpu) in app.gpus.iter().enumerate() {
            draw_gpu(frame, rows[i], app, gpu, i);
        }
    }

    frame.render_widget(
        Paragraph::new(" q quit  p pause  j/k select  +/- poll rate").style(t.dim),
        footer,
    );
}

fn draw_gpu(frame: &mut Frame, area: Rect, app: &App, gpu: &GpuSnapshot, idx: usize) {
    let t = &app.theme;
    let selected = idx == app.selected;
    let block = Block::bordered()
        .title(Span::styled(format!(" {idx} · {} ", gpu.name), t.title))
        .border_style(if selected {
            t.border_selected
        } else {
            t.border
        });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let [util_row, vram_row, spark_row, info_row] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    frame.render_widget(
        Gauge::default()
            .ratio((gpu.utilization_pct / 100.0).clamp(0.0, 1.0))
            .label(format!("GPU {:>3.0}%", gpu.utilization_pct))
            .gauge_style(t.gauge_util)
            .use_unicode(true),
        util_row,
    );

    frame.render_widget(
        Gauge::default()
            .ratio((gpu.vram_pct() / 100.0).clamp(0.0, 1.0))
            .label(format!(
                "VRAM {:.1}/{:.1} GiB",
                gib(gpu.vram_used_bytes),
                gib(gpu.vram_total_bytes)
            ))
            .gauge_style(t.gauge_vram)
            .use_unicode(true),
        vram_row,
    );

    if spark_row.height > 0
        && let Some(hist) = app.history.get(idx)
    {
        let width = spark_row.width as usize;
        let data = tail(&hist.util, width);
        frame.render_widget(
            Sparkline::default().data(data).style(t.spark_util),
            spark_row,
        );
    }

    let mut info: Vec<Span> = Vec::new();
    if let Some(c) = gpu.temperature_c {
        info.push(Span::styled(format!(" {c:.0}°C "), t.temp_style(c)));
    }
    if let Some(w) = gpu.power_w {
        let limit = gpu
            .power_limit_w
            .map(|l| format!("/{l:.0}"))
            .unwrap_or_default();
        info.push(Span::styled(format!("⚡{w:.0}{limit}W "), t.spark_power));
    }
    if let Some(f) = gpu.fan_pct {
        info.push(Span::styled(format!("fan {f:.0}% "), t.dim));
    }
    if let Some(c) = gpu.clock_mhz {
        info.push(Span::styled(format!("core {c}MHz "), t.dim));
    }
    if let Some(m) = gpu.mem_clock_mhz {
        info.push(Span::styled(format!("mem {m}MHz "), t.dim));
    }
    frame.render_widget(Paragraph::new(Line::from(info)), info_row);
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

fn tail(data: &[u64], width: usize) -> &[u64] {
    &data[data.len().saturating_sub(width)..]
}
