use crate::app::App;
use crate::backend::GpuSnapshot;
use crate::theme::UiTheme;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Table,
};

/// Minimum rows for an unfolded GPU card (borders + meters + info + waveform).
const CARD_MIN: u16 = 8;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    frame.render_widget(
        Block::new().style(Style::new().bg(app.theme.bg).fg(app.theme.fg)),
        area,
    );

    if app.splash_active() {
        crate::splash::render(frame, area, app.started, &app.splash_path, &app.theme);
        return;
    }

    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);

    {
        let t = &app.theme;
        let mut head = vec![
            Span::styled(format!(" gpur v{} ", env!("CARGO_PKG_VERSION")), t.title),
            Span::styled(format!("[{}] ", app.backend.name()), t.dim),
            Span::styled(format!("{}ms ", app.tick_ms), t.dim),
        ];
        if app.paused {
            head.push(Span::styled("PAUSED ", t.temp_warn));
        }
        frame.render_widget(Paragraph::new(Line::from(head)), header);
    }

    // Process pane takes only what it needs, up to 30% of the body; the GPU
    // cards get the rest. Careful on tiny terminals: the cap can drop below
    // the 4-row minimum.
    let want = app.procs.len() as u16 + 3;
    let cap = ((body.height * 3) / 10).max(4);
    let proc_height = want.min(cap).min(body.height);
    let [gpus_area, proc_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(proc_height)]).areas(body);

    app.gpus_rect = gpus_area;
    app.proc_rect = proc_area;
    draw_gpus(frame, gpus_area, app);
    draw_processes(frame, proc_area, app);

    frame.render_widget(
        Paragraph::new(" q quit  ␣ pause  p procs  0-9 gpu (again folds)  j/k move  +/- poll rate")
            .style(app.theme.dim),
        footer,
    );
}

/// GPU card region. When every card fits it behaves like a plain vertical
/// split; when it overflows (many GPUs / small terminal) it becomes a
/// scrolled list of fixed-height cards with a scrollbar, keeping the
/// selected card visible.
fn draw_gpus(frame: &mut Frame, area: Rect, app: &mut App) {
    let t = &app.theme;
    if app.gpus.is_empty() {
        frame.render_widget(
            Paragraph::new("no GPUs reported by backend").style(t.dim),
            area,
        );
        return;
    }

    let height_of =
        |app: &App, i: usize| -> u16 { if app.folded.contains(&i) { 1 } else { CARD_MIN } };
    let n = app.gpus.len();
    let needed: u16 = (0..n).map(|i| height_of(app, i)).sum();

    if needed <= area.height {
        // Everything fits: unfolded cards stretch to share the space.
        app.gpu_scroll = 0;
        let rows = Layout::vertical((0..n).map(|i| {
            if app.folded.contains(&i) {
                Constraint::Length(1)
            } else {
                Constraint::Fill(1)
            }
        }))
        .split(area);
        app.card_rects = rows.iter().copied().zip(0..n).collect();
        for (i, gpu) in app.gpus.iter().enumerate() {
            if app.folded.contains(&i) {
                draw_gpu_folded(frame, rows[i], app, gpu, i);
            } else {
                draw_gpu(frame, rows[i], app, gpu, i);
            }
        }
        return;
    }

    // Overflow: scroll whole cards so the selection stays visible.
    app.gpu_scroll = app.gpu_scroll.min(n - 1).min(app.selected);
    loop {
        let visible_span: u16 = (app.gpu_scroll..=app.selected)
            .map(|i| height_of(app, i))
            .sum();
        if visible_span <= area.height || app.gpu_scroll >= app.selected {
            break;
        }
        app.gpu_scroll += 1;
    }

    // How many whole cards fit at their minimum height...
    let mut shown = 0usize;
    let mut used = 0u16;
    for i in app.gpu_scroll..n {
        let h = height_of(app, i);
        if used + h > area.height {
            break;
        }
        used += h;
        shown += 1;
    }
    let shown = shown.max(1);

    // ...then let that window stretch to fill the area — no dead gap.
    let cards = Rect {
        width: area.width.saturating_sub(1),
        ..area
    };
    let window: Vec<usize> = (app.gpu_scroll..(app.gpu_scroll + shown).min(n)).collect();
    let rows = Layout::vertical(window.iter().map(|i| {
        if app.folded.contains(i) {
            Constraint::Length(1)
        } else {
            Constraint::Fill(1)
        }
    }))
    .split(cards);
    app.card_rects = rows.iter().copied().zip(window.iter().copied()).collect();
    for (slot, &i) in rows.iter().zip(&window) {
        let gpu = &app.gpus[i];
        if app.folded.contains(&i) {
            draw_gpu_folded(frame, *slot, app, gpu, i);
        } else {
            draw_gpu(frame, *slot, app, gpu, i);
        }
    }

    let max_scroll = n.saturating_sub(shown);
    let mut sb = ScrollbarState::new(max_scroll).position(app.gpu_scroll.min(max_scroll));
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .style(app.theme.dim),
        area,
        &mut sb,
    );
}

/// One-line summary for a folded GPU card: `▸ 0·name  GPU 3%  MEM 8G/24G ...`
fn draw_gpu_folded(frame: &mut Frame, area: Rect, app: &App, gpu: &GpuSnapshot, idx: usize) {
    let t = &app.theme;
    let selected = idx == app.selected;
    let marker = if selected { t.border_selected } else { t.dim };
    let mut line = vec![
        Span::styled(" ▸ ", marker),
        Span::styled(format!("{idx}·{}  ", gpu.name), t.title),
        Span::styled(format!("GPU {:>3.0}%  ", gpu.utilization_pct), t.spark_util),
        Span::styled(
            format!(
                "MEM {}/{}  ",
                human_bytes(gpu.vram_used_bytes),
                human_bytes(gpu.vram_total_bytes)
            ),
            Style::new().fg(t.accent),
        ),
    ];
    if let Some(c) = gpu.temperature_c {
        line.push(Span::styled(format!("{c:.0}°C  "), t.temp_style(c)));
    }
    if let Some(w) = gpu.power_w {
        line.push(Span::styled(format!("{w:.0}W  "), t.spark_power));
    }
    frame.render_widget(Paragraph::new(Line::from(line)), area);
}

/// btop-style border caption: `┐ text ┌` sitting in the border line.
fn caption<'a>(text: String, text_style: Style, border: Style) -> Line<'a> {
    Line::from(vec![
        Span::styled("┐", border),
        Span::styled(text, text_style),
        Span::styled("┌", border),
    ])
}

fn draw_gpu(frame: &mut Frame, area: Rect, app: &App, gpu: &GpuSnapshot, idx: usize) {
    let t = &app.theme;
    let selected = idx == app.selected;
    let border = if selected {
        t.border_selected
    } else {
        t.border
    };

    let right_text = if gpu.integrated {
        "integrated".to_string()
    } else {
        match (gpu.pcie_gen, gpu.pcie_width) {
            (Some(g), Some(w)) => format!("PCIe {g}.0@{w}x"),
            _ => String::new(),
        }
    };
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(caption(format!("{idx}·{}", gpu.name), t.title, border));
    if !right_text.is_empty() {
        block = block.title_top(caption(right_text, t.dim, border).right_aligned());
    }
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

    let hist = app.history.get(idx);
    draw_meter(
        frame,
        util_row,
        "GPU ",
        gpu.utilization_pct / 100.0,
        format!(" {:>3.0}% ", gpu.utilization_pct),
        &t.util_stops(),
        t,
    );
    draw_meter(
        frame,
        vram_row,
        "MEM ",
        gpu.vram_pct() / 100.0,
        format!(
            " {}/{} ",
            human_bytes(gpu.vram_used_bytes),
            human_bytes(gpu.vram_total_bytes)
        ),
        &t.vram_stops(),
        t,
    );

    if spark_row.height >= 2
        && let Some(hist) = hist
    {
        draw_waveform(frame, spark_row, &hist.util, &hist.vram, t);
    }

    let mut info: Vec<Span> = vec![Span::raw(" ")];
    if let Some(c) = gpu.temperature_c {
        if let Some(h) = hist {
            info.push(Span::styled(mini_spark(&h.temp, 100), t.dim));
        }
        info.push(Span::styled(format!(" {c:.0}°C  "), t.temp_style(c)));
    }
    if let Some(w) = gpu.power_w {
        let max_w = gpu.power_limit_w.unwrap_or(0.0).max(w).max(1.0) as u64;
        if let Some(h) = hist {
            info.push(Span::styled(mini_spark(&h.power, max_w), t.dim));
        }
        let limit = gpu
            .power_limit_w
            .map(|l| format!("/{l:.0}"))
            .unwrap_or_default();
        info.push(Span::styled(format!(" {w:.0}{limit}W  "), t.spark_power));
    }
    if let (Some(rx), Some(tx)) = (gpu.pcie_rx_kbs, gpu.pcie_tx_kbs) {
        info.push(Span::styled(format!("▼{} ▲{}  ", kbs(rx), kbs(tx)), t.dim));
    }
    if let Some(f) = gpu.fan_pct {
        info.push(Span::styled(format!("fan {f:.0}%  "), t.dim));
    }
    if let Some(c) = gpu.clock_mhz {
        info.push(Span::styled(format!("core {c}MHz  "), t.dim));
    }
    if let Some(m) = gpu.mem_clock_mhz {
        info.push(Span::styled(format!("mem {m}MHz  "), t.dim));
    }
    if let Some(mb) = gpu.mem_util_pct {
        info.push(Span::styled(format!("membus {mb:.0}%  "), t.dim));
    }
    frame.render_widget(Paragraph::new(Line::from(info)), info_row);
}

/// btop-style meter: `LABEL ■■■■■■■■····  42%` with a position gradient over
/// the filled squares.
fn draw_meter(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    frac: f64,
    value: String,
    stops: &[(u8, u8, u8)],
    t: &UiTheme,
) {
    if area.height == 0 {
        return;
    }
    let mut spans = vec![Span::styled(label.to_string(), Style::new().fg(t.fg))];
    let meter_w = (area.width as usize)
        .saturating_sub(label.chars().count() + value.chars().count())
        .max(1);
    let filled = (frac.clamp(0.0, 1.0) * meter_w as f64).round() as usize;
    for i in 0..meter_w {
        let pos = if meter_w > 1 {
            i as f64 / (meter_w - 1) as f64
        } else {
            0.0
        };
        if i < filled {
            spans.push(Span::styled(
                "■",
                Style::new().fg(crate::theme::gradient(stops, pos)),
            ));
        } else {
            spans.push(Span::styled("·", t.dim));
        }
    }
    spans.push(Span::styled(value, Style::new().fg(t.fg)));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Five-cell inline braille sparkline of the last ten samples, scaled to
/// `max` — the `⣀⣀⣀⣠⣤` blips btop puts next to temps and power draws.
fn mini_spark(data: &[u64], max: u64) -> String {
    const CELLS: usize = 5;
    let n = CELLS * 2;
    let max = max.max(1);
    let mut out = String::with_capacity(CELLS * 3);
    for c in 0..CELLS {
        let mut bits = 0u8;
        for (s, bit_col) in DOT_BITS.iter().enumerate() {
            let i = c * 2 + s;
            let v = if data.len() >= n {
                data[data.len() - n + i]
            } else {
                let pad = n - data.len();
                if i < pad { 0 } else { data[i - pad] }
            };
            let dots = ((v.min(max) as usize * 4).div_ceil(max as usize)).clamp(1, 4);
            for d in 0..dots {
                bits |= bit_col[3 - d];
            }
        }
        out.push(char::from_u32(BRAILLE_BASE + bits as u32).unwrap_or('⠀'));
    }
    out
}

fn human_bytes(b: u64) -> String {
    let g = b as f64 / (1024.0 * 1024.0 * 1024.0);
    if g >= 10.0 {
        format!("{g:.0}G")
    } else if g >= 1.0 {
        format!("{g:.1}G")
    } else {
        format!("{}M", b / 1024 / 1024)
    }
}

const BRAILLE_BASE: u32 = 0x2800;
/// Braille dot bit for (sub-column, dot-row counted from cell top).
const DOT_BITS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];

/// btop-style mirrored waveform: `up_data` (gpu%) grows upward from the
/// vertical midline, `down_data` (vram%) grows downward, both in braille
/// (2 samples per cell column, 4 dot rows per cell) with a color gradient
/// from the midline toward the edges. Zero values keep one dot row, so an
/// idle GPU still draws a thin center line.
fn draw_waveform(frame: &mut Frame, area: Rect, up_data: &[u64], down_data: &[u64], t: &UiTheme) {
    if area.height < 2 || area.width == 0 {
        return;
    }
    let top_rows = (area.height / 2) as usize;
    let bot_rows = area.height as usize - top_rows;
    let cols = area.width as usize;
    let n = cols * 2; // braille doubles horizontal resolution

    let up_stops = t.util_stops();
    let down_stops = t.vram_stops();

    // Newest sample at the right edge; missing history reads as 0.
    let sample = |data: &[u64], i: usize| -> u64 {
        if data.len() >= n {
            data[data.len() - n + i]
        } else {
            let pad = n - data.len();
            if i < pad { 0 } else { data[i - pad] }
        }
    };
    // Value -> dot rows in this half; min 1 keeps the midline alive at 0.
    let dots_for =
        |v: u64, rows: usize| -> usize { ((v.min(100) as usize * rows * 4) / 100).max(1) };

    let buf = frame.buffer_mut();
    for half in 0..2 {
        let (rows, data, stops) = if half == 0 {
            (top_rows, up_data, &up_stops[..])
        } else {
            (bot_rows, down_data, &down_stops[..])
        };
        for cy in 0..rows {
            // cy counts away from the midline in both halves.
            let y = if half == 0 {
                area.y + (top_rows - 1 - cy) as u16
            } else {
                area.y + (top_rows + cy) as u16
            };
            let frac = if rows > 1 {
                cy as f64 / (rows - 1) as f64
            } else {
                0.0
            };
            let color = crate::theme::gradient(stops, frac);
            for cx in 0..cols {
                let mut bits = 0u8;
                for (s, bit_col) in DOT_BITS.iter().enumerate() {
                    let dots = dots_for(sample(data, cx * 2 + s), rows);
                    let in_cell = dots.saturating_sub(cy * 4).min(4);
                    for d in 0..in_cell {
                        // Up half fills cells bottom-up, down half top-down.
                        let row_in_cell = if half == 0 { 3 - d } else { d };
                        bits |= bit_col[row_in_cell];
                    }
                }
                if bits != 0
                    && let Some(cell) = buf.cell_mut((area.x + cx as u16, y))
                {
                    cell.set_char(char::from_u32(BRAILLE_BASE + bits as u32).unwrap_or('⠀'));
                    cell.set_fg(color);
                }
            }
        }
    }

    buf.set_string(area.x, area.y, "gpu%", t.dim);
    buf.set_string(area.x, area.y + area.height - 1, "vram%", t.dim);
}

fn draw_processes(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.height < 3 {
        return;
    }
    let total = app.procs.len();
    let visible = (area.height.saturating_sub(3) as usize).min(total);
    let max_scroll = total - visible;
    app.proc_scroll = app.proc_scroll.min(max_scroll);
    let counter = if max_scroll > 0 {
        format!(
            "{}-{}/{total}",
            app.proc_scroll + 1,
            app.proc_scroll + visible
        )
    } else {
        format!("{total}")
    };
    let t = &app.theme;
    let border = if app.focus == crate::app::Focus::Procs {
        t.border_selected
    } else {
        t.border
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(caption("processes".into(), t.title, border))
        .title_top(caption(counter, t.dim, border).right_aligned())
        .border_style(border);

    if app.procs.is_empty() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new("no GPU processes visible (need same-user or root for fdinfo)")
                .style(t.dim),
            inner,
        );
        return;
    }

    let header = Row::new(
        [
            "PID", "USER", "DEV", "TYPE", "GPU%", "GPU MEM", "CPU%", "HOST MEM", "COMMAND",
        ]
        .into_iter()
        .map(Cell::from),
    )
    .style(t.title);

    let rows = app.procs[app.proc_scroll..app.proc_scroll + visible]
        .iter()
        .map(|p| {
            Row::new(vec![
                Cell::from(p.pid.to_string()),
                Cell::from(p.user.clone()),
                Cell::from(p.gpu_index.to_string()),
                Cell::from(p.kind.label()),
                Cell::from(
                    p.gpu_util_pct
                        .map(|u| format!("{u:>3.0}%"))
                        .unwrap_or_else(|| "N/A".into()),
                ),
                Cell::from(format!("{}MiB", p.gpu_mem_bytes / 1024 / 1024)),
                Cell::from(format!("{:>3.0}%", p.cpu_pct)),
                Cell::from(format!("{}MiB", p.host_mem_bytes / 1024 / 1024)),
                Cell::from(p.command.clone()),
            ])
        });

    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(3),
            Constraint::Length(8),
            Constraint::Length(5),
            Constraint::Length(9),
            Constraint::Length(5),
            Constraint::Length(9),
            Constraint::Fill(1),
        ],
    )
    .header(header)
    .block(block);
    frame.render_widget(table, area);

    if max_scroll > 0 {
        let mut sb = ScrollbarState::new(max_scroll).position(app.proc_scroll);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .style(app.theme.dim),
            area.inner(ratatui::layout::Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut sb,
        );
    }
}

/// KiB/s -> human rate, matching nvtop's per-direction PCIe readout.
fn kbs(v: u64) -> String {
    if v >= 1024 * 1024 {
        format!("{:.1}GiB/s", v as f64 / (1024.0 * 1024.0))
    } else if v >= 1024 {
        format!("{:.1}MiB/s", v as f64 / 1024.0)
    } else {
        format!("{v}KiB/s")
    }
}
