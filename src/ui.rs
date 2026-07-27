use crate::app::{App, GraphStyle};
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
        if let Some(drv) = app.backend.driver_info() {
            head.push(Span::styled(format!("{drv} "), t.dim));
        }
        if app.paused {
            head.push(Span::styled("PAUSED ", t.temp_warn));
        }
        if let Some(err) = &app.poll_error {
            head.push(Span::styled(format!("⚠ {err} "), t.temp_crit));
        }
        if let Some(msg) = app.status_line() {
            head.push(Span::styled(format!("· {msg} "), t.spark_power));
        }
        frame.render_widget(Paragraph::new(Line::from(head)), header);
    }

    let proc_height = proc_pane_height(body.height, app.procs.len());
    let [gpus_area, proc_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(proc_height)]).areas(body);

    app.gpus_rect = gpus_area;
    app.proc_rect = proc_area;
    // Braille packs 2 samples per column; retain enough history for the
    // full frame width so wide terminals can fill their graphs. Assigned,
    // not accumulated — a high-water mark would pin every GPU's four
    // history vectors to the widest size the terminal ever had.
    app.history_need = area.width as usize * 2;
    draw_gpus(frame, gpus_area, app);
    draw_processes(frame, proc_area, app);

    let footer_line = if app.input_mode == crate::app::InputMode::Filter {
        Line::from(vec![
            Span::styled(" filter> ", app.theme.title),
            Span::styled(app.filter_input.clone(), Style::new().fg(app.theme.fg)),
            Span::styled("█", app.theme.title),
            Span::styled("  (Enter apply · empty clears · Esc cancel)", app.theme.dim),
        ])
    } else {
        Line::styled(crate::keys::footer_hints(), app.theme.dim)
    };
    frame.render_widget(Paragraph::new(footer_line), footer);

    draw_confirm_popup(frame, area, app);
    draw_help_popup(frame, area, app);
}

/// Rows the process pane gets: what it wants (one per row plus borders and
/// header), capped at 30% of the body, and never the entire body — the GPU
/// pane keeps a row so it can at least say "no GPUs reported".
///
/// Computed in `u32`: `body_height * 3` overflows `u16` above 21845 rows,
/// which a programmatic PTY resize can reach.
fn proc_pane_height(body_height: u16, procs: usize) -> u16 {
    let want = (procs as u64).saturating_add(3).min(u16::MAX as u64) as u16;
    let cap = ((body_height as u32 * 3) / 10).max(4).min(u16::MAX as u32) as u16;
    want.min(cap).min(body_height.saturating_sub(1))
}

/// `?` overlay listing every binding; any key closes it.
fn draw_help_popup(frame: &mut Frame, area: Rect, app: &App) {
    if !app.show_help {
        return;
    }
    let t = &app.theme;
    let rows = crate::keys::help_rows();
    let key_w = rows
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(8) as u16;
    let popup = centered(area, key_w + 50, rows.len() as u16 + 2);
    frame.render_widget(ratatui::widgets::Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(t.border_selected)
        .title(caption("help".into(), t.title, t.border_selected))
        .title_top(caption("any key closes".into(), t.dim, t.border_selected).right_aligned());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, desc)| {
            Line::from(vec![
                Span::styled(format!(" {k:>width$}  ", width = key_w as usize), t.title),
                Span::styled(*desc, Style::new().fg(t.fg)),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Popup width for the widest of `lines`, plus borders and padding.
fn popup_width(lines: &[&str]) -> u16 {
    let widest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    widest.saturating_add(6).min(u16::MAX as usize) as u16
}

/// Centered y/N dialog for a pending kill.
fn draw_confirm_popup(frame: &mut Frame, area: Rect, app: &App) {
    let Some(k) = &app.pending_kill else {
        return;
    };
    let (pid, cmd) = (k.pid, &k.command);
    let t = &app.theme;
    let sig = if k.force { "SIGKILL" } else { "SIGTERM" };
    let text = format!("send {sig} to {pid}?");
    // Width in columns, not bytes: a non-ASCII command path would otherwise
    // size the dialog several cells too wide per multibyte char.
    let popup = centered(area, popup_width(&[&text, cmd]), 5);
    frame.render_widget(ratatui::widgets::Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(t.temp_crit)
        .title(caption("confirm".into(), t.temp_crit, t.temp_crit));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let lines = vec![
        Line::styled(text, Style::new().fg(t.fg)),
        Line::styled(cmd.clone(), t.dim),
        Line::from(vec![
            Span::styled("y", t.temp_crit),
            Span::styled(" confirm · ", t.dim),
            Span::styled("any other key", Style::new().fg(t.fg)),
            Span::styled(" cancels", t.dim),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
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
        for i in 0..n {
            draw_card(frame, rows[i], app, i);
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
        draw_card(frame, *slot, app, i);
    }

    let max_scroll = n.saturating_sub(shown);
    draw_scrollbar(
        frame,
        area,
        app.gpu_scroll,
        max_scroll + 1,
        shown,
        app.theme.dim,
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
        line.push(temp_span(t, c, "", "  "));
    }
    if let Some(w) = gpu.power_w {
        // Folded rows have no room for the limit.
        line.push(power_span(t, w, None, "", "  "));
    }
    if let Some(reason) = &gpu.throttle {
        line.push(throttle_span(t, reason));
    }
    frame.render_widget(Paragraph::new(Line::from(line)), area);
}

// Readouts both the folded and the full card draw. Format and style live
// here so they cannot drift apart; only the surrounding padding, which
// differs between the two layouts, is caller-supplied.

/// Temperature, colored by the warn/crit thresholds.
fn temp_span(t: &UiTheme, c: f64, lead: &str, trail: &str) -> Span<'static> {
    Span::styled(format!("{lead}{c:.0}°C{trail}"), t.temp_style(c))
}

/// Power draw, with the board limit when the backend reports one.
fn power_span(t: &UiTheme, w: f64, limit: Option<f64>, lead: &str, trail: &str) -> Span<'static> {
    let limit = limit.map(|l| format!("/{l:.0}")).unwrap_or_default();
    Span::styled(format!("{lead}{w:.0}{limit}W{trail}"), t.spark_power)
}

/// Throttle badge. The space after ⚠ is deliberate — terminals render the
/// glyph at ambiguous width and it collides with the reason without it.
fn throttle_span(t: &UiTheme, reason: &str) -> Span<'static> {
    Span::styled(format!("⚠ {reason}  "), t.temp_crit)
}

/// btop-style border caption: `┐ text ┌` sitting in the border line.
fn caption<'a>(text: String, text_style: Style, border: Style) -> Line<'a> {
    Line::from(vec![
        Span::styled("┐", border),
        Span::styled(text, text_style),
        Span::styled("┌", border),
    ])
}

/// Sample index `i` of a right-aligned window of `window` values over `data`:
/// newest sample at the right edge, missing history reads as 0.
fn windowed(data: &[u64], i: usize, window: usize) -> u64 {
    if data.len() >= window {
        data[data.len() - window + i]
    } else {
        let pad = window - data.len();
        if i < pad { 0 } else { data[i - pad] }
    }
}

/// A `w`×`h` rect centered in `area`, clamped to it.
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(w) / 2,
        area.y + area.height.saturating_sub(h) / 2,
        w,
        h,
    )
}

/// Vertical scrollbar for a windowed list. `positions` is max_scroll+1 (the
/// ratatui thumb only reaches the track end at content_length-1); `viewport`
/// is the item count shown.
fn draw_scrollbar(
    frame: &mut Frame,
    area: Rect,
    pos: usize,
    positions: usize,
    viewport: usize,
    style: Style,
) {
    let mut sb = ScrollbarState::new(positions)
        .position(pos)
        .viewport_content_length(viewport);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .style(style),
        area,
        &mut sb,
    );
}

/// Draw GPU card `idx` into `area`, folded or full per app state.
fn draw_card(frame: &mut Frame, area: Rect, app: &App, idx: usize) {
    let gpu = &app.gpus[idx];
    if app.folded.contains(&idx) {
        draw_gpu_folded(frame, area, app, gpu, idx);
    } else {
        draw_gpu(frame, area, app, gpu, idx);
    }
}

fn draw_gpu(frame: &mut Frame, area: Rect, app: &App, gpu: &GpuSnapshot, idx: usize) {
    let t = &app.theme;
    let selected = idx == app.selected;
    let border = if selected {
        t.border_selected
    } else {
        t.border
    };

    // PCIe caption; a link running below its max (bad riser, wrong slot,
    // power saving stuck) gets a yellow "(max …)" flag.
    let mut right_spans: Vec<Span> = Vec::new();
    if gpu.integrated {
        right_spans.push(Span::styled("integrated", t.dim));
    } else if let (Some(g), Some(w)) = (gpu.pcie_gen, gpu.pcie_width) {
        right_spans.push(Span::styled(format!("PCIe {g}.0@{w}x"), t.dim));
        if let (Some(mg), Some(mw)) = (gpu.pcie_max_gen, gpu.pcie_max_width)
            && (g < mg || w < mw)
        {
            right_spans.push(Span::styled(format!(" (max {mg}.0@{mw}x)"), t.temp_warn));
        }
    }
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(caption(format!("{idx}·{}", gpu.name), t.title, border));
    if !right_spans.is_empty() {
        let mut line = vec![Span::styled("┐", border)];
        line.extend(right_spans);
        line.push(Span::styled("┌", border));
        block = block.title_top(Line::from(line).right_aligned());
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    // Session-stats line only when the card has breathing room.
    let show_session = inner.height >= 7 && app.session.get(idx).is_some();
    let [util_row, vram_row, spark_row, session_row, info_row] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(if show_session { 1 } else { 0 }),
        Constraint::Length(1),
    ])
    .areas(inner);

    if show_session && let Some(sess) = app.session.get(idx) {
        let line = Line::from(vec![
            Span::styled(" session ", t.dim),
            Span::styled(
                format!(
                    "peak {:>3.0}%  {:>3.0}°C  {:>3.0}W   ",
                    sess.max_util_pct, sess.max_temp_c, sess.max_power_w
                ),
                Style::new().fg(t.fg),
            ),
            Span::styled(
                format!(
                    "avg {:>3.0}%  {:>3.0}W",
                    sess.avg_util_pct(),
                    sess.avg_power_w()
                ),
                t.dim,
            ),
        ]);
        frame.render_widget(Paragraph::new(line), session_row);
    }

    let hist = app.history.get(idx);
    draw_meter(
        frame,
        util_row,
        "GPU ",
        gpu.utilization_pct / 100.0,
        format!(" {:>3.0}% ", gpu.utilization_pct),
        &t.util_stops(),
        t,
        app.graph_style,
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
        app.graph_style,
    );

    if spark_row.height >= 2
        && let Some(hist) = hist
    {
        draw_waveform(frame, spark_row, &hist.util, &hist.vram, t, app.graph_style);
    }

    let mut info: Vec<Span> = vec![Span::raw(" ")];
    if let Some(reason) = &gpu.throttle {
        info.push(throttle_span(t, reason));
    }
    if let Some(c) = gpu.temperature_c {
        if let Some(h) = hist {
            info.push(Span::styled(
                mini_spark(&h.temp, 100, app.graph_style),
                t.dim,
            ));
        }
        info.push(temp_span(t, c, " ", " "));
        if let Some(j) = gpu.temp_junction_c {
            info.push(Span::styled(format!("junc {j:.0}° "), t.dim));
        }
        if let Some(m) = gpu.temp_mem_c {
            info.push(Span::styled(format!("mem {m:.0}° "), t.dim));
        }
        info.push(Span::raw(" "));
    }
    if let Some(w) = gpu.power_w {
        let max_w = gpu.power_limit_w.unwrap_or(0.0).max(w).max(1.0) as u64;
        if let Some(h) = hist {
            info.push(Span::styled(
                mini_spark(&h.power, max_w, app.graph_style),
                t.dim,
            ));
        }
        info.push(power_span(t, w, gpu.power_limit_w, " ", "  "));
    }
    if let (Some(rx), Some(tx)) = (gpu.pcie_rx_kbs, gpu.pcie_tx_kbs) {
        info.push(Span::styled(
            format!("▼ {} ▲ {}  ", kbs(rx), kbs(tx)),
            t.dim,
        ));
    }
    if let Some(f) = gpu.fan_pct {
        let rpm = gpu.fan_rpm.map(|r| format!(" {r}rpm")).unwrap_or_default();
        info.push(Span::styled(format!("fan {f:.0}%{rpm}  "), t.dim));
    } else if let Some(r) = gpu.fan_rpm {
        info.push(Span::styled(format!("fan {r}rpm  "), t.dim));
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
    if let Some(v) = gpu.video_util_pct {
        info.push(Span::styled(format!("video {v:.0}%  "), t.dim));
    }
    if let Some(e) = gpu.enc_util_pct {
        info.push(Span::styled(format!("enc {e:.0}%  "), t.dim));
    }
    if let Some(d) = gpu.dec_util_pct {
        info.push(Span::styled(format!("dec {d:.0}%  "), t.dim));
    }
    if let Some(mv) = gpu.volt_mv {
        info.push(Span::styled(format!("{:.2}V  ", mv as f64 / 1000.0), t.dim));
    }
    if let (Some(u_), Some(t_)) = (gpu.gtt_used_bytes, gpu.gtt_total_bytes) {
        info.push(Span::styled(
            format!("gtt {}/{}  ", human_bytes(u_), human_bytes(t_)),
            t.dim,
        ));
    }
    if let Some(p) = &gpu.perf_level {
        info.push(Span::styled(format!("perf {p}  "), t.temp_warn));
    }
    frame.render_widget(Paragraph::new(Line::from(info)), info_row);
}

/// btop-style meter: `LABEL ■■■■■■■■····  42%` with a position gradient over
/// the filled squares.
#[allow(clippy::too_many_arguments)]
fn draw_meter(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    frac: f64,
    value: String,
    stops: &[(u8, u8, u8)],
    t: &UiTheme,
    style: GraphStyle,
) {
    if area.height == 0 {
        return;
    }
    let (fill, empty) = match style {
        GraphStyle::Ascii => ("=", "."),
        _ => ("■", "·"),
    };
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
                fill,
                Style::new().fg(crate::theme::gradient(stops, pos, t.mode)),
            ));
        } else {
            spans.push(Span::styled(empty, t.dim));
        }
    }
    spans.push(Span::styled(value, Style::new().fg(t.fg)));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Five-cell inline sparkline of recent samples, scaled to `max` — the
/// `⣀⣀⣀⣠⣤` blips btop puts next to temps and power draws. Follows the
/// configured glyph set.
fn mini_spark(data: &[u64], max: u64, style: GraphStyle) -> String {
    const CELLS: usize = 5;
    let max = max.max(1);
    if style != GraphStyle::Braille {
        return (0..CELLS)
            .map(|c| {
                let v = windowed(data, c, CELLS).min(max);
                if style == GraphStyle::Block {
                    let lvl = ((v as usize * 8).div_ceil(max as usize)).clamp(1, 8);
                    EIGHTHS[lvl]
                } else {
                    let lvl = ((v as usize * 4).div_ceil(max as usize)).clamp(0, 4);
                    ASCII_RAMP[lvl]
                }
            })
            .collect();
    }
    let n = CELLS * 2;
    let mut out = String::with_capacity(CELLS * 3);
    for c in 0..CELLS {
        let mut bits = 0u8;
        for (s, bit_col) in DOT_BITS.iter().enumerate() {
            let v = windowed(data, c * 2 + s, n);
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

/// Lower-block glyphs by filled eighths (index 0..=8).
const EIGHTHS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// Ascii coverage ramp indexed by fill level 0..=4. Level 0 is the baseline
/// `_`, drawn by `mini_spark` so an all-zero sparkline still shows a line;
/// the waveform skips empty cells outright and never indexes it.
const ASCII_RAMP: [char; 5] = ['_', '.', '-', '+', '#'];
const BRAILLE_BASE: u32 = 0x2800;
/// Braille dot bit for (sub-column, dot-row counted from cell top).
const DOT_BITS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];

/// Walk the mirrored-waveform grid and call `cell` once per terminal row:
/// top half from `up_data` growing up from the midline, bottom half from
/// `down_data` growing down. Owns the shared geometry (row `y`, gradient
/// `color`, the gpu%/vram% edge labels); the glyph-specific per-column fill
/// lives in the closure. `cell` receives (buf, row data, half-height rows,
/// distance-from-midline cy, row y, color, half).
fn waveform_halves(
    frame: &mut Frame,
    area: Rect,
    up_data: &[u64],
    down_data: &[u64],
    t: &UiTheme,
    mut cell: impl FnMut(
        &mut ratatui::buffer::Buffer,
        &[u64],
        usize,
        usize,
        u16,
        ratatui::style::Color,
        usize,
    ),
) {
    let top_rows = (area.height / 2) as usize;
    let bot_rows = area.height as usize - top_rows;
    let up_stops = t.util_stops();
    let down_stops = t.vram_stops();
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
            let color = crate::theme::gradient(stops, frac, t.mode);
            cell(buf, data, rows, cy, y, color, half);
        }
    }
    buf.set_string(area.x, area.y, "gpu%", t.dim);
    buf.set_string(area.x, area.y + area.height - 1, "vram%", t.dim);
}

/// btop-style mirrored waveform: `up_data` (gpu%) grows upward from the
/// vertical midline, `down_data` (vram%) grows downward, with a color
/// gradient from the midline toward the edges. Zero values keep a minimum
/// sliver, so an idle GPU still draws a thin center line. The glyph set is
/// selectable: braille (2 samples/cell, 4 rows/cell), block eighths, or
/// pure ascii.
fn draw_waveform(
    frame: &mut Frame,
    area: Rect,
    up_data: &[u64],
    down_data: &[u64],
    t: &UiTheme,
    style: GraphStyle,
) {
    if area.height < 2 || area.width == 0 {
        return;
    }
    if style != GraphStyle::Braille {
        return draw_waveform_cells(frame, area, up_data, down_data, t, style);
    }
    let cols = area.width as usize;
    let n = cols * 2; // braille doubles horizontal resolution
    // Value -> dot rows in this half; min 1 keeps the midline alive at 0.
    let dots_for =
        |v: u64, rows: usize| -> usize { ((v.min(100) as usize * rows * 4) / 100).max(1) };

    waveform_halves(
        frame,
        area,
        up_data,
        down_data,
        t,
        |buf, data, rows, cy, y, color, half| {
            for cx in 0..cols {
                let mut bits = 0u8;
                for (s, bit_col) in DOT_BITS.iter().enumerate() {
                    let dots = dots_for(windowed(data, cx * 2 + s, n), rows);
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
        },
    );
}

/// Block/ascii waveform: one sample per column. Block mode uses eighth
/// glyphs (down-growing partials via fg/bg swap since Unicode has no lower
/// upper-partials); ascii uses a `.-+#` coverage ramp.
fn draw_waveform_cells(
    frame: &mut Frame,
    area: Rect,
    up_data: &[u64],
    down_data: &[u64],
    t: &UiTheme,
    style: GraphStyle,
) {
    let cols = area.width as usize;
    // Sub-units per cell: 8 block eighths or 4 ascii coverage steps.
    let unit = if style == GraphStyle::Block { 8 } else { 4 };
    // Quantize like every other color: t.bg is already painted, so deriving
    // from it would re-emit an Indexed color as 24-bit.
    let bg = crate::theme::paint(t.mode, t.bg_rgb);

    waveform_halves(
        frame,
        area,
        up_data,
        down_data,
        t,
        |buf, data, rows, cy, y, color, half| {
            for cx in 0..cols {
                let v = windowed(data, cx, cols).min(100) as usize;
                let units = ((v * rows * unit) / 100).max(1);
                let in_cell = units.saturating_sub(cy * unit).min(unit);
                if in_cell == 0 {
                    continue;
                }
                let (ch, cell_style) = match style {
                    GraphStyle::Block if half == 0 => (EIGHTHS[in_cell], Style::new().fg(color)),
                    GraphStyle::Block => {
                        if in_cell == 8 {
                            ('█', Style::new().fg(color))
                        } else {
                            // Complement trick: paint the empty lower part in
                            // the background color over a bar-colored cell.
                            (EIGHTHS[8 - in_cell], Style::new().fg(bg).bg(color))
                        }
                    }
                    _ => (ASCII_RAMP[in_cell], Style::new().fg(color)),
                };
                if let Some(cell) = buf.cell_mut((area.x + cx as u16, y)) {
                    cell.set_char(ch);
                    cell.set_style(cell_style);
                }
            }
        },
    );
}

fn draw_processes(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.height < 3 {
        app.proc_visible = 0;
        return;
    }
    let total = app.procs.len();
    let visible = (area.height.saturating_sub(3) as usize).min(total);
    // Click hit-tests bound against this rather than procs.len().
    app.proc_visible = visible;
    let max_scroll = total - visible;
    // Viewport follows the cursor row.
    app.proc_sel = app.proc_sel.min(total.saturating_sub(1));
    if app.proc_sel < app.proc_scroll {
        app.proc_scroll = app.proc_sel;
    } else if visible > 0 && app.proc_sel >= app.proc_scroll + visible {
        app.proc_scroll = app.proc_sel + 1 - visible;
    }
    app.proc_scroll = app.proc_scroll.min(max_scroll);
    let arrow = if app.sort_desc { "↓" } else { "↑" };
    let mut counter = format!("{}{arrow}", app.sort_by.label());
    if !app.filter.is_empty() {
        counter = format!("filter:{} · {counter}", app.filter);
    }
    // `visible == 0` (pane too short for a data row) would render an
    // inverted `1-0/24` range.
    if max_scroll > 0 && visible > 0 {
        counter.push_str(&format!(
            " · {}-{}/{total}",
            app.proc_scroll + 1,
            app.proc_scroll + visible
        ));
    } else {
        counter.push_str(&format!(" · {total}"));
    }
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
        // An active filter is the likelier cause than permissions, and
        // sending the user to root when their own filter emptied the list
        // is both wrong and (see the kill path) bad advice.
        let msg = if app.filter.is_empty() {
            "no GPU processes visible (need same-user or root for fdinfo)".to_string()
        } else {
            format!(
                "no processes match filter '{}' ({} hidden; press / to change)",
                app.filter,
                app.all_procs.len()
            )
        };
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new(msg).style(t.dim), inner);
        return;
    }

    let mark = |label: &str, is: bool| -> String {
        if is {
            format!("{label}{arrow}")
        } else {
            label.to_string()
        }
    };
    use crate::app::SortBy;
    // CONTAINER column only when any visible row is containerized.
    let show_container = app.procs.iter().any(|p| p.container.is_some());
    let mut header_cells = vec![
        mark("PID", app.sort_by == SortBy::Pid),
        "USER".into(),
        "DEV".into(),
        "TYPE".into(),
        mark("GPU%", app.sort_by == SortBy::GpuUtil),
        mark("GPU MEM", app.sort_by == SortBy::GpuMem),
        mark("CPU%", app.sort_by == SortBy::Cpu),
        mark("HOST MEM", app.sort_by == SortBy::HostMem),
    ];
    if show_container {
        header_cells.push("CONTAINER".into());
    }
    header_cells.push("COMMAND".into());
    let header = Row::new(header_cells.into_iter().map(Cell::from)).style(t.title);

    let proc_sel = app.proc_sel;
    let selection = t.selection;
    let rows = app.procs[app.proc_scroll..app.proc_scroll + visible]
        .iter()
        .enumerate()
        .map(|(vi, p)| {
            let row_style = if app.proc_scroll + vi == proc_sel {
                selection
            } else {
                Style::default()
            };
            let mut cells = vec![
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
            ];
            if show_container {
                cells.push(Cell::from(
                    p.container.clone().unwrap_or_else(|| "-".into()),
                ));
            }
            cells.push(Cell::from(p.command.clone()));
            Row::new(cells).style(row_style)
        });

    let mut widths = vec![
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(3),
        Constraint::Length(8),
        Constraint::Length(5),
        Constraint::Length(9),
        Constraint::Length(5),
        Constraint::Length(9),
    ];
    if show_container {
        widths.push(Constraint::Length(19));
    }
    widths.push(Constraint::Fill(1));
    let table = Table::new(rows, widths).header(header).block(block);
    frame.render_widget(table, area);

    if max_scroll > 0 {
        // Track spans the data rows only (skip borders + header line).
        let track = Rect::new(
            area.x,
            area.y + 2,
            area.width,
            area.height.saturating_sub(3),
        );
        draw_scrollbar(
            frame,
            track,
            app.proc_scroll,
            max_scroll + 1,
            visible,
            app.theme.dim,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_pane_height_never_overflows() {
        // body_height * 3 wraps u16 above 21845; PtySize::rows is a u16.
        for h in [21845, 21846, 30000, u16::MAX] {
            let p = proc_pane_height(h, 4);
            assert!(p < h, "gpu pane starved at {h}");
        }
        // ...and neither does the row count.
        assert!(proc_pane_height(100, usize::MAX) <= 30);
    }

    #[test]
    fn proc_pane_height_leaves_the_gpu_pane_a_row() {
        // Short terminals used to hand the whole body to the process pane,
        // leaving draw_gpus a zero-height rect.
        for h in 0..=8u16 {
            let p = proc_pane_height(h, 50);
            assert!(p <= h.saturating_sub(1), "no room left for GPUs at {h}");
        }
        assert_eq!(proc_pane_height(0, 3), 0);
        assert_eq!(proc_pane_height(1, 3), 0);
        assert_eq!(proc_pane_height(5, 3), 4);
    }

    #[test]
    fn proc_pane_height_caps_at_30_percent() {
        assert_eq!(proc_pane_height(100, 50), 30);
        // Wants less than the cap: takes only what it needs.
        assert_eq!(proc_pane_height(100, 5), 8);
    }

    #[test]
    fn popup_width_counts_chars_not_bytes() {
        // "kill 42?" plus a command path with multibyte chars.
        let ascii = popup_width(&["send SIGTERM to 42?", "/usr/bin/renderer"]);
        let wide = popup_width(&["send SIGTERM to 42?", "/usr/bin/rendérér"]);
        assert_eq!(ascii, 19 + 6);
        assert_eq!(wide, ascii, "accents must not widen the dialog");
        assert_eq!(popup_width(&[]), 6); // borders only
    }

    #[test]
    fn ascii_ramp_baseline_only_reachable_from_mini_spark() {
        // The waveform indexes 1..=4 (it skips empty cells); mini_spark
        // indexes 0..=4 and relies on the baseline glyph.
        assert_eq!(ASCII_RAMP[0], '_');
        assert_eq!(
            mini_spark(&[0, 0, 0, 0, 0], 100, GraphStyle::Ascii),
            "_____"
        );
        assert_eq!(
            mini_spark(&[100, 100, 100, 100, 100], 100, GraphStyle::Ascii),
            "#####"
        );
    }
}
