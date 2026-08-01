use crate::app::{App, GraphStyle, SessionStats};
use crate::backend::GpuSnapshot;
use crate::theme::{ColorMode, UiTheme};
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
    // Retain enough history for the full frame width so wide terminals can
    // fill their graphs, scaled by what the active glyph set can actually
    // draw — braille packs 2 samples per column, block and ascii draw one
    // (`draw_waveform_cells`), so a flat doubling would make every device
    // under `--graphs ascii` hold twice the samples any graph can ever show.
    // Assigned, not accumulated — a high-water mark would pin every GPU's
    // four history vectors to the widest size the terminal ever had.
    app.history_need = area.width as usize * samples_per_column(app.graph_style);
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

/// Samples one terminal column of graph can display. Braille encodes two
/// dot columns per cell (`draw_waveform` widens its window to `cols * 2`);
/// block and ascii spend a whole cell on one sample. Sizing history retention
/// by anything else either starves wide braille graphs or hoards samples the
/// other styles can never draw.
fn samples_per_column(style: GraphStyle) -> usize {
    if style == GraphStyle::Braille { 2 } else { 1 }
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

/// Rows a stack of cards occupies.
///
/// Summed in `u32` for the same reason `proc_pane_height` computes there:
/// `CARD_MIN` rows per unfolded card wraps `u16` past 8191 cards, and the
/// wrapped total reads as "everything fits" — the pane would then hand every
/// card a `Fill` constraint in an area far too small for them.
fn stacked_height(heights: impl IntoIterator<Item = u16>) -> u32 {
    heights.into_iter().map(u32::from).sum()
}

/// How many whole cards fit in `height`, walking the stack from the top of
/// the scroll window.
///
/// The running total is a `u32` because `used + h` overflows `u16` once
/// `height` is within `CARD_MIN` of `u16::MAX`, which a programmatic PTY
/// resize can reach (`PtySize::rows` is a `u16`); the wrapped sum compares
/// small and the loop keeps admitting cards past the bottom of the pane.
fn cards_that_fit(heights: impl IntoIterator<Item = u16>, height: u16) -> usize {
    let mut shown = 0usize;
    let mut used = 0u32;
    for h in heights {
        if used + u32::from(h) > height as u32 {
            break;
        }
        used += u32::from(h);
        shown += 1;
    }
    shown
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

    let height_of = |app: &App, i: usize| -> u16 { if app.is_folded(i) { 1 } else { CARD_MIN } };
    let n = app.gpus.len();
    let needed = stacked_height((0..n).map(|i| height_of(app, i)));

    if needed <= area.height as u32 {
        // Everything fits: unfolded cards stretch to share the space.
        app.gpu_scroll = 0;
        let rows = Layout::vertical((0..n).map(|i| {
            if app.is_folded(i) {
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
        let visible_span =
            stacked_height((app.gpu_scroll..=app.selected).map(|i| height_of(app, i)));
        if visible_span <= area.height as u32 || app.gpu_scroll >= app.selected {
            break;
        }
        app.gpu_scroll += 1;
    }

    // How many whole cards fit at their minimum height...
    let shown = cards_that_fit((app.gpu_scroll..n).map(|i| height_of(app, i)), area.height);
    let shown = shown.max(1);

    // ...then let that window stretch to fill the area — no dead gap.
    let cards = Rect {
        width: area.width.saturating_sub(1),
        ..area
    };
    let window: Vec<usize> = (app.gpu_scroll..(app.gpu_scroll + shown).min(n)).collect();
    let rows = Layout::vertical(window.iter().map(|&i| {
        if app.is_folded(i) {
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
        Span::styled(format!("GPU {}  ", pct_or_na(gpu.utilization_pct)), {
            if gpu.utilization_pct.is_some() {
                t.spark_util
            } else {
                t.dim
            }
        }),
        Span::styled(format!("MEM {}  ", vram_value(gpu)), {
            if gpu.vram_pct().is_some() {
                Style::new().fg(t.accent)
            } else {
                t.dim
            }
        }),
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

/// A percentage the backend could read, or `n/a`. Padded to the same four
/// columns either way so a card whose sensor comes and goes doesn't jitter.
fn pct_or_na(v: Option<f64>) -> String {
    v.map(|p| format!("{p:>3.0}%"))
        .unwrap_or_else(|| " n/a".to_string())
}

fn bytes_or_na(b: Option<u64>) -> String {
    b.map(human_bytes).unwrap_or_else(|| "n/a".to_string())
}

/// The MEM readout. A device that publishes neither figure (mainline i915, a
/// PDH adapter with no counter instance) says so once instead of claiming an
/// empty `0M/0M` pool.
fn vram_value(gpu: &GpuSnapshot) -> String {
    match (gpu.vram_used_bytes, gpu.vram_total_bytes) {
        (None, None) => "n/a".to_string(),
        (used, total) => format!("{}/{}", bytes_or_na(used), bytes_or_na(total)),
    }
}

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
/// newest sample at the right edge, `None` for a sample the backend could not
/// read.
///
/// The left pad is `None` too, because history that has not been collected yet
/// is not a reading of 0 — the same claim this signature exists to stop making.
/// So a freshly started gpur on a wide terminal draws a mostly dim graph that
/// fills in with color from the right as samples accumulate. That is intended:
/// the graph shows how much of the window it can actually speak for.
fn windowed(data: &[Option<u64>], i: usize, window: usize) -> Option<u64> {
    if data.len() >= window {
        data[data.len() - window + i]
    } else {
        let pad = window - data.len();
        if i < pad { None } else { data[i - pad] }
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
    if app.is_folded(idx) {
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

    // Session-stats line only when the card has breathing room AND at least
    // one of its terms was ever measured — an all-empty row is noise.
    let session_line = (inner.height >= 7)
        .then(|| app.session_at(idx))
        .flatten()
        .and_then(|s| session_line(s, t));
    let show_session = session_line.is_some();
    let [util_row, vram_row, spark_row, session_row, info_row] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(if show_session { 1 } else { 0 }),
        Constraint::Length(1),
    ])
    .areas(inner);

    if let Some(line) = session_line {
        frame.render_widget(Paragraph::new(line), session_row);
    }

    let hist = app.history_at(idx);
    draw_meter(
        frame,
        util_row,
        Meter {
            label: "GPU ",
            frac: gpu.utilization_pct.map(|u| u / 100.0),
            value: format!(" {} ", pct_or_na(gpu.utilization_pct)),
            stops: &t.util_stops,
        },
        t,
        app.graph_style,
    );
    draw_meter(
        frame,
        vram_row,
        Meter {
            label: "MEM ",
            frac: gpu.vram_pct().map(|p| p / 100.0),
            value: format!(" {} ", vram_value(gpu)),
            stops: &t.vram_stops,
        },
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

/// `session peak 87%  72°C  310W   avg 42%  180W`, each term dropped when its
/// sensor never reported. A card with no thermal sensor printing `0°C` is a
/// fabricated reading, not a cold GPU. `None` when nothing was measured at all.
fn session_line(s: &SessionStats, t: &UiTheme) -> Option<Line<'static>> {
    let fmt = |v: Option<f64>, unit: &str| v.map(|v| format!("{v:>3.0}{unit}"));
    let peak: Vec<String> = [
        fmt(s.max_util_pct, "%"),
        fmt(s.max_temp_c, "°C"),
        fmt(s.max_power_w, "W"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let avg: Vec<String> = [fmt(s.avg_util_pct(), "%"), fmt(s.avg_power_w(), "W")]
        .into_iter()
        .flatten()
        .collect();
    if peak.is_empty() && avg.is_empty() {
        return None;
    }
    let mut spans = vec![Span::styled(" session ", t.dim)];
    if !peak.is_empty() {
        spans.push(Span::styled(
            format!("peak {}   ", peak.join("  ")),
            Style::new().fg(t.fg),
        ));
    }
    if !avg.is_empty() {
        spans.push(Span::styled(format!("avg {}", avg.join("  ")), t.dim));
    }
    Some(Line::from(spans))
}

/// The half of [`draw_meter`]'s arguments that says what one particular bar
/// reads: its caption, its fill, its readout, its colors. The GPU and MEM
/// meters differ in nothing but these four, and as a positional run they were
/// four adjacent arguments a careless edit could transpose without the
/// compiler noticing — `label` and `value` are both strings, and swapping the
/// two `stops` slices silently paints VRAM in the utilization gradient.
/// Naming them at the call site is the point.
///
/// The theme and the glyph style stay positional alongside `frame`/`area`
/// instead of joining this struct: they are ambient context every drawing
/// function in this file already trails (`draw_waveform(.., t, style)`), and
/// both meters pass the identical pair, so folding them in would repeat two
/// noise fields per call site and break the shape of the neighbours.
struct Meter<'a> {
    label: &'a str,
    /// Fill level, 0.0..=1.0 — see `draw_meter` for what `None` means and why
    /// it is not 0.0.
    frac: Option<f64>,
    /// Right-hand readout, pre-padded by the caller; its width is subtracted
    /// from the track so the bar and the number do not fight over columns.
    value: String,
    /// Gradient stops for the filled glyphs, borrowed from the same theme that
    /// arrives as `t`. Which of `util_stops`/`vram_stops` a meter wants is
    /// per-meter data, so it cannot be recovered from the theme alone.
    stops: &'a [(u8, u8, u8)],
}

/// btop-style meter: `LABEL ■■■■■■■■····  42%` with a position gradient over
/// the filled squares. `frac` of `None` is "the backend cannot read this" and
/// draws no track at all — a full-width track of empty glyphs is exactly the
/// confident "0%" this is meant to stop rendering.
fn draw_meter(frame: &mut Frame, area: Rect, meter: Meter<'_>, t: &UiTheme, style: GraphStyle) {
    if area.height == 0 {
        return;
    }
    let Meter {
        label,
        frac,
        value,
        stops,
    } = meter;
    let Some(frac) = frac else {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(label.to_string(), Style::new().fg(t.fg)),
                Span::styled(value.trim_start().to_string(), t.dim),
            ])),
            area,
        );
        return;
    };
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
///
/// An unknown sample renders as a gap: a space in block and ascii, no dots in
/// braille. The waveform can dim an unknown column, but the caller draws these
/// five cells wholly in `t.dim`, so absence has nowhere to go but the glyph. A
/// measured 0 keeps its baseline glyph (`▁`, `_`, or a bottom dot row), which
/// is what makes the two readable apart.
fn mini_spark(data: &[Option<u64>], max: u64, style: GraphStyle) -> String {
    const CELLS: usize = 5;
    let max = max.max(1);
    if style != GraphStyle::Braille {
        return (0..CELLS)
            .map(|c| {
                let Some(v) = windowed(data, c, CELLS).map(|v| v.min(max)) else {
                    return ' ';
                };
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
            // An unknown sub-column contributes no dots, so a cell whose
            // samples are all unknown ends at `bits == 0` and is drawn as a
            // space — a known sample always sets at least one bit.
            let Some(v) = windowed(data, c * 2 + s, n) else {
                continue;
            };
            let dots = ((v.min(max) as usize * 4).div_ceil(max as usize)).clamp(1, 4);
            for d in 0..dots {
                bits |= bit_col[3 - d];
            }
        }
        if bits == 0 {
            out.push(' ');
        } else {
            out.push(char::from_u32(BRAILLE_BASE + bits as u32).unwrap_or('⠀'));
        }
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
/// `_`: a line with nothing on it. Two callers reach for it, and neither
/// indexes it by fill level — the ramp's 1..=4 are the measured heights.
/// `mini_spark` draws it for a measured 0 (an all-zero sparkline still shows
/// a line, and its unknowns are spaces). The waveform draws it for an
/// *unknown* sample, whose measured 0 is the `.` sliver at level 1.
///
/// So `_` means "measured 0" in one widget and "nothing read" in the other.
/// They are never adjacent, and within each widget the contrast is the one
/// that matters there; the alternative was giving up the distinction in
/// whichever widget lost the glyph.
const ASCII_RAMP: [char; 5] = ['_', '.', '-', '+', '#'];
/// The waveform's mark for a sample that could not be read, in the braille and
/// block glyph sets. It belongs to neither value ramp, so it cannot be misread
/// as a magnitude, and it is already this UI's glyph for "nothing here" —
/// `draw_meter` paints the empty part of a non-ascii track with it.
///
/// Ascii keeps `ASCII_RAMP[0]` instead: `--graphs ascii` is chosen by people
/// whose terminal or font may not have `·` at all, which is the same reason
/// they are not being handed braille.
const UNKNOWN_MARK: char = '·';

/// A down-growing bar of `eighths`, using only the partial blocks that grow
/// from the *top* of a cell — Unicode has just two, `▔` (⅛) and `▀` (½), plus
/// the full block.
///
/// The waveform's down half normally sidesteps that gap with the complement
/// trick, painting the bar in the background and the hole in the foreground.
/// That needs two colors, so under [`ColorMode::Mono`] it collapses and draws
/// the cell inverted. This is the fallback for that case: three levels instead
/// of eight, rounded to the nearest available, which loses resolution but keeps
/// the bar pointing the way it grows. Ties round down, so a bar is understated
/// rather than overstated.
fn upper_block(eighths: usize) -> char {
    match eighths {
        0..=2 => '▔',
        3..=6 => '▀',
        _ => '█',
    }
}
const BRAILLE_BASE: u32 = 0x2800;
/// Braille dot bit for (sub-column, dot-row counted from cell top).
const DOT_BITS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];

/// Walk the mirrored-waveform grid and call `cell` once per terminal row:
/// top half from `up_data` growing up from the midline, bottom half from
/// `down_data` growing down. Owns the shared geometry (row `y`, gradient
/// `color`, the gpu%/vram% edge labels); the glyph-specific per-column fill
/// lives in the closure. `cell` receives (buf, row data, half-height rows,
/// distance-from-midline cy, row y, color, half).
///
/// `color` is the gradient this row would use for a measured sample; a column
/// with nothing to measure is the closure's business, since only it knows how
/// its samples map onto cells, and it substitutes `t.dim` there.
fn waveform_halves(
    frame: &mut Frame,
    area: Rect,
    up_data: &[Option<u64>],
    down_data: &[Option<u64>],
    t: &UiTheme,
    mut cell: impl FnMut(
        &mut ratatui::buffer::Buffer,
        &[Option<u64>],
        usize,
        usize,
        u16,
        ratatui::style::Color,
        usize,
    ),
) {
    let top_rows = (area.height / 2) as usize;
    let bot_rows = area.height as usize - top_rows;
    let buf = frame.buffer_mut();
    for half in 0..2 {
        let (rows, data, stops) = if half == 0 {
            (top_rows, up_data, &t.util_stops[..])
        } else {
            (bot_rows, down_data, &t.vram_stops[..])
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
///
/// An unknown sample occupies the same minimum-sliver cell — minimum height
/// because we do not know the value and must not imply one, a sliver rather
/// than a gap so the trace stays continuous and a run of unknowns reads as "no
/// data here" rather than as a hole in the widget — but it draws its own mark
/// there (`UNKNOWN_MARK`, or `ASCII_RAMP[0]` in ascii) in `t.dim`, never a
/// glyph from the value ramp.
///
/// The glyph is the signal and the dimming reinforces it, not the other way
/// round: color and `Modifier::DIM` are both lost under `ColorMode::Mono`, in
/// a terminal that ignores DIM, and in a screenshot or a copy-paste of the
/// screen, and an unreadable sample must not become an idle GPU in any of
/// those. So a measured 0 and an unknown differ in `symbol()` alone, in every
/// glyph set and both halves.
fn draw_waveform(
    frame: &mut Frame,
    area: Rect,
    up_data: &[Option<u64>],
    down_data: &[Option<u64>],
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
                // A ratatui cell carries one glyph and one foreground, but
                // braille packs two samples into it: mark the cell unknown
                // only when every sample in it is unknown. A known sample must
                // never be erased or greyed out, so a mixed cell keeps its
                // dots and its gradient — the marked run then covers exactly
                // the stretch where nothing at all was read.
                let mut known = false;
                for (s, bit_col) in DOT_BITS.iter().enumerate() {
                    let sample = windowed(data, cx * 2 + s, n);
                    known |= sample.is_some();
                    // Unknown occupies the minimum sliver: `dots_for` already
                    // floors at 1, so a 0 stands in for "no value to imply".
                    // Which cell that lands in is all this decides — an
                    // all-unknown cell is redrawn as the marker below.
                    let dots = dots_for(sample.unwrap_or(0), rows);
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
                    if known {
                        cell.set_char(char::from_u32(BRAILLE_BASE + bits as u32).unwrap_or('⠀'));
                        cell.set_fg(color);
                    } else {
                        // Not a braille glyph at all: the dot pattern for an
                        // unknown sliver is the same one a measured 0 draws, so
                        // leaving it braille made the two identical the moment
                        // the styling was lost. See `UNKNOWN_MARK`.
                        cell.set_char(UNKNOWN_MARK);
                        cell.set_style(t.dim);
                    }
                }
            }
        },
    );
}

/// Block/ascii waveform: one sample per column. Block mode uses eighth
/// glyphs (down-growing partials via fg/bg swap since Unicode has no lower
/// upper-partials); ascii uses a `.-+#` coverage ramp. An unknown sample draws
/// its own mark in the minimum-sliver cell instead of a ramp glyph, dimmed;
/// see [`draw_waveform`].
fn draw_waveform_cells(
    frame: &mut Frame,
    area: Rect,
    up_data: &[Option<u64>],
    down_data: &[Option<u64>],
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
                let sample = windowed(data, cx, cols);
                // An unknown sample stands in as 0, which the `.max(1)` below
                // turns into the minimum sliver — the only height we are
                // entitled to draw without implying a value. Which cell that
                // lands in is all it decides; the glyph below says what it is.
                let v = sample.unwrap_or(0).min(100) as usize;
                let units = ((v * rows * unit) / 100).max(1);
                let in_cell = units.saturating_sub(cy * unit).min(unit);
                if in_cell == 0 {
                    continue;
                }
                let (ch, cell_style) = if sample.is_none() {
                    // Ascii is the one style that cannot take `UNKNOWN_MARK`:
                    // `--graphs ascii` exists for terminals whose font coverage
                    // cannot be trusted past ASCII, which is the whole reason
                    // to offer it. Its ramp is a coverage ramp rather than a
                    // positional one, so the bare baseline `_` is free to mean
                    // unknown against the `.` a measured 0 draws, in both
                    // halves.
                    let ch = if style == GraphStyle::Ascii {
                        ASCII_RAMP[0]
                    } else {
                        UNKNOWN_MARK
                    };
                    (ch, t.dim)
                } else {
                    match style {
                        GraphStyle::Block if half == 0 => {
                            (EIGHTHS[in_cell], Style::new().fg(color))
                        }
                        GraphStyle::Block if t.mode == ColorMode::Mono => {
                            // The complement trick below needs two distinct
                            // colors, and Mono has none: `paint` answers
                            // `Color::Reset` for the bar and for the page
                            // alike, so the "empty" complement was drawn in the
                            // default foreground and every cell in this half
                            // rendered as its own inverse — a measured 0 as a
                            // nearly full block. Fall back to the upper
                            // partials Unicode does have, which cost resolution
                            // but at least point the right way.
                            (upper_block(in_cell), Style::new().fg(color))
                        }
                        GraphStyle::Block => {
                            if in_cell == 8 {
                                ('█', Style::new().fg(color))
                            } else {
                                // Complement trick: paint the empty lower part
                                // in the background color over a bar-colored
                                // cell.
                                (EIGHTHS[8 - in_cell], Style::new().fg(bg).bg(color))
                            }
                        }
                        _ => (ASCII_RAMP[in_cell], Style::new().fg(color)),
                    }
                };
                if let Some(cell) = buf.cell_mut((area.x + cx as u16, y)) {
                    cell.set_char(ch);
                    cell.set_style(cell_style);
                }
            }
        },
    );
}

/// A percentage cell in the process table, or `N/A` when the figure could
/// not be read. One spelling for every unknown in this table: `N/A` is what
/// the GPU% column has always printed, and a second word for the same idea
/// in the next column along would read as a different kind of absence.
/// (The GPU cards above say `n/a`; that block has its own house style and
/// is never seen on the same line as these.)
fn proc_pct(v: Option<f64>) -> String {
    v.map(|v| format!("{v:>3.0}%"))
        .unwrap_or_else(|| "N/A".into())
}

/// A whole-MiB cell in the process table, `N/A` when unreadable. `0MiB` is
/// reserved for a figure that was read and came back empty.
fn proc_mib(v: Option<u64>) -> String {
    v.map(|b| format!("{}MiB", b / 1024 / 1024))
        .unwrap_or_else(|| "N/A".into())
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
                Cell::from(proc_pct(p.gpu_util_pct)),
                Cell::from(proc_mib(p.gpu_mem_bytes)),
                Cell::from(proc_pct(p.cpu_pct.map(f64::from))),
                Cell::from(proc_mib(p.host_mem_bytes)),
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn theme() -> UiTheme {
        crate::theme::load(None, crate::theme::detect_color_mode()).unwrap()
    }

    /// A backend the process-table test never polls: `draw_processes` reads
    /// `app.procs`, which the test populates directly.
    struct NoBackend;

    impl crate::backend::GpuBackend for NoBackend {
        fn name(&self) -> &'static str {
            "test"
        }
        fn poll(&mut self) -> anyhow::Result<Vec<GpuSnapshot>> {
            Ok(Vec::new())
        }
    }

    /// Render the process table over the given rows and return its lines.
    fn proc_table(rows: Vec<crate::app::ProcRow>) -> Vec<String> {
        const W: u16 = 100;
        const H: u16 = 6;
        let mut app = App::new(
            Box::new(NoBackend),
            theme(),
            crate::app::AppOptions {
                tick_ms: 1000,
                tick_explicit: false,
                history_len: 60,
                no_splash: true,
                graph_style: GraphStyle::Ascii,
                source: crate::backend::BackendSource::Live,
                log: None,
            },
        );
        app.all_procs = rows.clone();
        app.procs = rows;
        let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
        term.draw(|f| draw_processes(f, f.area(), &mut app))
            .unwrap();
        let buf = term.backend().buffer();
        (0..H)
            .map(|y| {
                (0..W)
                    .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()))
                    .collect()
            })
            .collect()
    }

    /// The whole point of C4: a figure the backend could not read must be
    /// visibly absent, and a figure it read as empty must not be. Both rows
    /// are rendered together so the two cannot converge on one spelling.
    #[test]
    fn proc_table_says_na_for_unreadable_columns_and_prints_a_real_zero() {
        let row = |pid, command: &str, gpu_mem, cpu, host_mem| crate::app::ProcRow {
            pid,
            gpu_index: 0,
            kind: crate::backend::ProcKind::Compute,
            gpu_util_pct: Some(0.0),
            gpu_mem_bytes: gpu_mem,
            user: "me".into(),
            cpu_pct: cpu,
            host_mem_bytes: host_mem,
            command: command.into(),
            container: None,
        };
        let lines = proc_table(vec![
            row(1, "unreadable", None, None, None),
            row(2, "measured", Some(0), Some(0.0), Some(0)),
        ]);
        let find = |needle: &str| {
            lines
                .iter()
                .find(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("no {needle} row in {lines:#?}"))
                .clone()
        };
        let unknown = find("unreadable");
        let zero = find("measured");

        // Three unreadable columns, three N/A cells — GPU MEM, CPU% and
        // HOST MEM. GPU% is measured here, so it is not one of them.
        assert_eq!(
            unknown.matches("N/A").count(),
            3,
            "unreadable columns did not all say N/A: {unknown:?}"
        );
        assert!(
            !unknown.contains("MiB"),
            "an unreadable figure still claimed a MiB total: {unknown:?}"
        );

        // The measured-empty row keeps its numbers, and must not borrow the
        // absent row's spelling.
        assert_eq!(
            zero.matches("0MiB").count(),
            2,
            "a measured-empty pool lost its 0MiB: {zero:?}"
        );
        assert!(
            zero.contains("0%") && !zero.contains("N/A"),
            "a measured zero rendered as unknown: {zero:?}"
        );
    }

    /// Render one meter into a single-row terminal and return what it drew.
    fn meter(gpu: &GpuSnapshot) -> String {
        let t = theme();
        let mut term = Terminal::new(TestBackend::new(40, 1)).unwrap();
        term.draw(|f| {
            draw_meter(
                f,
                f.area(),
                Meter {
                    label: "GPU ",
                    frac: gpu.utilization_pct.map(|u| u / 100.0),
                    value: format!(" {} ", pct_or_na(gpu.utilization_pct)),
                    stops: &t.util_stops,
                },
                &t,
                GraphStyle::Ascii,
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        (0..40)
            .filter_map(|x| buf.cell((x, 0)).map(|c| c.symbol().to_string()))
            .collect()
    }

    /// The core of finding 7: a backend that cannot read utilization must not
    /// be rendered as one reading 0%.
    #[test]
    fn unknown_utilization_renders_na_not_a_zero_meter() {
        let unknown = meter(&GpuSnapshot::default());
        assert!(unknown.contains("n/a"), "{unknown:?}");
        assert!(
            !unknown.contains('=') && !unknown.contains('.'),
            "unknown metric drew a meter track: {unknown:?}"
        );

        // A genuine 0% still gets its meter, and never says n/a.
        let idle = meter(&GpuSnapshot {
            utilization_pct: Some(0.0),
            ..Default::default()
        });
        assert!(idle.contains('.'), "idle GPU lost its meter: {idle:?}");
        assert!(idle.trim_end().ends_with("0%"), "{idle:?}");
        assert!(!idle.contains("n/a"), "{idle:?}");
    }

    /// `0M/0M` is a claim about an empty pool; absence has to say so instead.
    #[test]
    fn vram_readout_distinguishes_absent_from_empty() {
        assert_eq!(vram_value(&GpuSnapshot::default()), "n/a");
        assert_eq!(
            vram_value(&GpuSnapshot {
                vram_used_bytes: Some(0),
                vram_total_bytes: Some(0),
                ..Default::default()
            }),
            "0M/0M"
        );
        // One half known is still worth printing.
        assert_eq!(
            vram_value(&GpuSnapshot {
                vram_used_bytes: Some(2 << 30),
                ..Default::default()
            }),
            "2.0G/n/a"
        );
    }

    /// Finding 11: a sensorless card must omit the term, not peak at 0°C/0W.
    #[test]
    fn session_line_omits_sensors_that_never_reported() {
        let t = theme();
        let text = |s: &SessionStats| {
            session_line(s, &t).map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
        };

        // Utilization only — the Windows PDH / Apple / Intel iGPU case.
        let mut util_only = SessionStats::default();
        util_only.add(&GpuSnapshot {
            utilization_pct: Some(80.0),
            ..Default::default()
        });
        let line = text(&util_only).expect("a session line");
        assert!(line.contains("peak  80%"), "{line:?}");
        assert!(!line.contains("°C"), "fabricated a temperature: {line:?}");
        assert!(!line.contains('W'), "fabricated a power draw: {line:?}");

        // Nothing measured at all: no row rather than an empty one.
        let mut nothing = SessionStats::default();
        nothing.add(&GpuSnapshot::default());
        assert_eq!(text(&nothing), None);

        // Fully sensored cards keep every term.
        let mut full = SessionStats::default();
        full.add(&GpuSnapshot {
            utilization_pct: Some(80.0),
            temperature_c: Some(71.0),
            power_w: Some(300.0),
            ..Default::default()
        });
        let line = text(&full).expect("a session line");
        assert!(line.contains("71°C") && line.contains("300W"), "{line:?}");
    }

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
    fn card_stack_height_never_overflows() {
        // CARD_MIN per card wraps u16 above 8191 cards; a wrapped total reads
        // as "everything fits" and stretches every card into a pane that is
        // nowhere near big enough.
        let cards = 8192;
        let needed = stacked_height((0..cards).map(|_| CARD_MIN));
        assert_eq!(needed, cards as u32 * CARD_MIN as u32);
        assert!(needed > u16::MAX as u32, "no longer the overflow case");
        // Folded cards are a row each, and a mixed stack still totals exactly.
        assert_eq!(stacked_height([1, CARD_MIN, 1]), CARD_MIN as u32 + 2);
        assert_eq!(stacked_height([]), 0);
    }

    #[test]
    fn cards_that_fit_stops_at_the_pane_edge() {
        // The running total used to be a u16, so a pane within CARD_MIN rows
        // of u16::MAX wrapped it and kept admitting cards forever.
        assert_eq!(cards_that_fit([CARD_MIN; 4], u16::MAX), 4);
        assert_eq!(
            cards_that_fit((0..20_000).map(|_| CARD_MIN), u16::MAX),
            8191
        );
        // ...and the ordinary cases still stop where the rows run out.
        assert_eq!(cards_that_fit([CARD_MIN; 4], 20), 2);
        assert_eq!(cards_that_fit([1, 1, CARD_MIN], 8), 2);
        assert_eq!(cards_that_fit([CARD_MIN; 4], 0), 0);
    }

    #[test]
    fn history_retention_matches_what_the_glyph_set_can_draw() {
        // Only braille packs two samples into a column; retaining 2x under
        // block or ascii keeps samples no graph will ever read back.
        assert_eq!(samples_per_column(GraphStyle::Braille), 2);
        assert_eq!(samples_per_column(GraphStyle::Block), 1);
        assert_eq!(samples_per_column(GraphStyle::Ascii), 1);
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

    /// The baseline glyph has two callers with opposite meanings, and the
    /// reason is worth pinning down: `mini_spark` cannot dim anything (the
    /// caller styles it whole), so its unknown is a gap and `_` is left to
    /// mean a measured 0. The waveform cannot leave a gap (the trace has to
    /// stay continuous), so its unknown takes `_` and a measured 0 takes the
    /// level-1 sliver. Neither widget is ambiguous within itself.
    #[test]
    fn the_ascii_baseline_is_a_measured_zero_in_a_spark_and_unknown_in_a_waveform() {
        assert_eq!(ASCII_RAMP[0], '_');
        // mini_spark: a measured 0 draws the line, unknown draws nothing.
        assert_eq!(mini_spark(&[Some(0); 5], 100, GraphStyle::Ascii), "_____");
        assert_eq!(mini_spark(&[Some(100); 5], 100, GraphStyle::Ascii), "#####");
        assert_eq!(
            mini_spark(&[None::<u64>; 5], 100, GraphStyle::Ascii),
            "     "
        );

        // The waveform, the other way round.
        let t = color_theme();
        let data = [None, Some(0)];
        let grid = waveform_grid(&t, GraphStyle::Ascii, &data, &data, 2, 4);
        for y in [1, 2] {
            assert_eq!(grid[y][0].0, ASCII_RAMP[0].to_string(), "row {y}");
            assert_eq!(grid[y][1].0, ASCII_RAMP[1].to_string(), "row {y}");
        }
    }

    /// Backlog 6: history that has not been collected is not a reading of 0,
    /// and neither is a sample the backend could not read.
    #[test]
    fn windowed_pads_with_unknown_rather_than_zero() {
        let data = [Some(10), None, Some(0)];
        // Exactly one window's worth: every sample straight through.
        assert_eq!(
            (0..3).map(|i| windowed(&data, i, 3)).collect::<Vec<_>>(),
            vec![Some(10), None, Some(0)]
        );
        // Narrower window: right-aligned on the newest samples.
        assert_eq!(windowed(&data, 0, 2), None);
        assert_eq!(windowed(&data, 1, 2), Some(0));
        // Wider than the history: the left pad is unknown, not a flat zero
        // line, so a fresh gpur starts dim and colors in from the right.
        assert_eq!(
            (0..5).map(|i| windowed(&data, i, 5)).collect::<Vec<_>>(),
            vec![None, None, Some(10), None, Some(0)]
        );
        let empty: [Option<u64>; 0] = [];
        assert_eq!(windowed(&empty, 0, 3), None);
    }

    /// A theme pinned to truecolor. The waveform tests compare a cell's color
    /// against `t.dim`'s, and 16-color quantization can land the dim gray and
    /// a gradient stop on the same index, which would make the comparison
    /// meaningless rather than merely different.
    fn color_theme() -> UiTheme {
        crate::theme::load(None, crate::theme::ColorMode::Truecolor).unwrap()
    }

    /// Render a mirrored waveform and return (symbol, style) per cell, by row.
    fn waveform_grid(
        t: &UiTheme,
        style: GraphStyle,
        up: &[Option<u64>],
        down: &[Option<u64>],
        w: u16,
        h: u16,
    ) -> Vec<Vec<(String, Style)>> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw_waveform(f, f.area(), up, down, t, style))
            .unwrap();
        let buf = term.backend().buffer();
        (0..h)
            .map(|y| {
                (0..w)
                    .filter_map(|x| {
                        buf.cell((x, y))
                            .map(|c| (c.symbol().to_string(), c.style()))
                    })
                    .collect()
            })
            .collect()
    }

    /// The point of backlog 6, in its strongest form: a column the backend
    /// could not read and a column it read as 0 differ in `symbol()` — before
    /// any style is consulted — in both halves and all three glyph sets. The
    /// dim styling is then checked as reinforcement. An honest graph in one
    /// style and a fabricated zero in another would be no better than before.
    #[test]
    fn waveform_draws_unknown_and_a_measured_zero_as_different_glyphs() {
        let t = color_theme();
        let dim = t.dim.fg.expect("a truecolor dim style has a foreground");
        for style in [GraphStyle::Braille, GraphStyle::Block, GraphStyle::Ascii] {
            // Braille packs two samples per column, so it needs two per cell.
            let data: Vec<Option<u64>> = if style == GraphStyle::Braille {
                vec![None, None, Some(0), Some(0)]
            } else {
                vec![None, Some(0)]
            };
            // 4 rows: the edge labels sit on rows 0 and 3, the midline
            // slivers on row 1 (up half) and row 2 (down half).
            let grid = waveform_grid(&t, style, &data, &data, 2, 4);
            for y in [1, 2] {
                let (unknown_sym, unknown) = grid[y][0].clone();
                let (zero_sym, zero) = grid[y][1].clone();
                // The property that survives losing every style: whatever
                // terminal this lands on, the two columns are different
                // characters. Color and DIM are reinforcement, not the signal
                // — a screenshot, a copy-paste, a mono terminal or one that
                // ignores DIM all keep the distinction. Asserted first,
                // because it is the one the maintainer asked for.
                assert_ne!(
                    unknown_sym, zero_sym,
                    "{style:?} row {y}: unknown and a measured zero share a glyph, \
                     so they are indistinguishable without styling"
                );
                let expected = if style == GraphStyle::Ascii {
                    ASCII_RAMP[0]
                } else {
                    UNKNOWN_MARK
                };
                assert_eq!(unknown_sym, expected.to_string(), "{style:?} row {y}");
                // ...and the unknown mark belongs to no value ramp, so it can
                // never be read as a magnitude.
                assert!(
                    !ASCII_RAMP[1..].contains(&expected) && !EIGHTHS.contains(&expected),
                    "{style:?}: the unknown mark is a value glyph"
                );
                assert_eq!(
                    unknown.fg,
                    Some(dim),
                    "{style:?} row {y}: unknown column not dim ({unknown_sym:?})"
                );
                assert_ne!(
                    zero.fg,
                    Some(dim),
                    "{style:?} row {y}: a measured zero was greyed out ({zero_sym:?})"
                );
                assert_ne!(
                    zero.bg,
                    Some(dim),
                    "{style:?} row {y}: a measured zero was greyed out in the background"
                );
                // A sliver, not a gap: the trace stays continuous, so a run of
                // marks reads as "no data" rather than as a hole in the widget.
                assert_ne!(
                    unknown_sym.trim(),
                    "",
                    "{style:?} row {y}: unknown column drew nothing"
                );
                assert_ne!(zero_sym.trim(), "", "{style:?} row {y}: zero drew nothing");
            }
        }
    }

    /// `NO_COLOR=1` / `TERM=dumb`: `t.dim` is nothing but `Modifier::DIM`,
    /// which a Linux console or an old terminal may render as nothing at all.
    /// The same test as above with every color removed — if the distinction
    /// ever moves back into the styling, this is where it shows.
    /// The down half grows downward, so under Mono — where the complement
    /// trick has no second color to work with — a small value must still draw
    /// a small mark hanging from the top of the cell. It used to draw the
    /// complement in the default foreground instead, so a measured 0 filled
    /// seven eighths of its cell and the whole half read upside down.
    #[test]
    fn a_mono_block_graph_does_not_invert_its_bottom_half() {
        let t = crate::theme::load(None, crate::theme::ColorMode::Mono).unwrap();
        // One column, so every cell in a half belongs to the same sample.
        let quiet = [Some(0u64)];
        let busy = [Some(100u64)];

        // Six rows so the halves are three deep and the `vram%` caption on the
        // last row cannot be mistaken for graph output.
        let grid = waveform_grid(&t, GraphStyle::Block, &quiet, &quiet, 1, 6);
        // Row 3 is the first row below the midline: the sliver lives there.
        assert_eq!(
            grid[3][0].0,
            '▔'.to_string(),
            "a measured 0 filled the cell"
        );
        // ...and the row past it is untouched, not painted.
        assert_eq!(grid[4][0].0, " ", "the bar leaked past its own value");

        // A full-scale sample fills that same cell and keeps going, which is
        // what proves the glyph tracks the value rather than being a constant.
        let grid = waveform_grid(&t, GraphStyle::Block, &busy, &busy, 1, 6);
        assert_eq!(grid[3][0].0, '█'.to_string());
        assert_eq!(grid[4][0].0, '█'.to_string());

        // Colour modes that can afford the complement trick still use it, so
        // this stayed a Mono-only fallback.
        let color = waveform_grid(&color_theme(), GraphStyle::Block, &quiet, &quiet, 1, 6);
        assert_eq!(color[3][0].0, EIGHTHS[7].to_string());
    }

    #[test]
    fn upper_blocks_round_to_the_nearest_available_partial() {
        // Unicode gives us ⅛, ½ and full and nothing else that grows from the
        // top; ties round down so a bar is never overstated.
        assert_eq!(upper_block(1), '▔');
        assert_eq!(upper_block(2), '▔');
        assert_eq!(upper_block(3), '▀');
        assert_eq!(upper_block(6), '▀');
        assert_eq!(upper_block(7), '█');
        assert_eq!(upper_block(8), '█');
    }

    #[test]
    fn a_mono_waveform_still_separates_unknown_from_a_measured_zero() {
        let t = crate::theme::load(None, crate::theme::ColorMode::Mono).unwrap();
        assert_eq!(t.dim.fg, None, "mono dim has no color to lean on");
        for style in [GraphStyle::Braille, GraphStyle::Block, GraphStyle::Ascii] {
            let data: Vec<Option<u64>> = if style == GraphStyle::Braille {
                vec![None, None, Some(0), Some(0)]
            } else {
                vec![None, Some(0)]
            };
            let grid = waveform_grid(&t, style, &data, &data, 2, 4);
            for y in [1, 2] {
                let (unknown, zero) = (grid[y][0].0.clone(), grid[y][1].0.clone());
                assert_ne!(
                    unknown, zero,
                    "{style:?} row {y}: mono drew an unreadable sample and a \
                     measured zero as the same cell"
                );
                assert_eq!(
                    unknown,
                    if style == GraphStyle::Ascii {
                        ASCII_RAMP[0]
                    } else {
                        UNKNOWN_MARK
                    }
                    .to_string(),
                    "{style:?} row {y}"
                );
            }
        }
    }

    /// One glyph and one foreground per cell against two samples per cell:
    /// the cell becomes the unknown mark only when there is nothing known in
    /// it. A known sample must never be erased by its neighbour's absence.
    #[test]
    fn a_braille_cell_is_marked_unknown_only_when_all_its_samples_are() {
        let t = color_theme();
        let dim = t.dim.fg.expect("a truecolor dim style has a foreground");
        // Column 0: both unknown. Column 1: one of each. Column 2: both known.
        let data = vec![None, None, None, Some(50), Some(50), Some(50)];
        let grid = waveform_grid(&t, GraphStyle::Braille, &data, &data, 3, 4);
        for y in [1, 2] {
            let (all_unknown, mixed, known) =
                (grid[y][0].clone(), grid[y][1].clone(), grid[y][2].clone());
            assert_eq!(all_unknown.0, UNKNOWN_MARK.to_string(), "row {y}");
            assert_eq!(all_unknown.1.fg, Some(dim), "row {y}: all-unknown not dim");

            // The mixed cell keeps its dots: a braille glyph, not the mark,
            // and not blank — the known sample is still on the screen.
            assert_ne!(
                mixed.0,
                UNKNOWN_MARK.to_string(),
                "row {y}: mixed cell erased"
            );
            assert!(
                mixed
                    .0
                    .chars()
                    .all(|c| (BRAILLE_BASE..BRAILLE_BASE + 0x100).contains(&(c as u32))),
                "row {y}: mixed cell drew {:?}, not braille",
                mixed.0
            );
            assert_ne!(
                mixed.1.fg,
                Some(dim),
                "row {y}: a cell holding a known sample was greyed out"
            );

            assert_ne!(
                known.0,
                UNKNOWN_MARK.to_string(),
                "row {y}: known cell marked"
            );
            assert_ne!(known.1.fg, Some(dim), "row {y}: known cell went dim");
        }
    }

    /// The mini sparks are drawn wholly in `t.dim` by the caller, so dimming
    /// cannot say anything there: an unknown sample has to be a gap in the
    /// glyphs, and a measured zero has to keep its baseline glyph. That makes
    /// this widget glyph-only already — it needs no `UNKNOWN_MARK`, and it
    /// survives Mono and a copy-paste for the same reason the waveform now
    /// does. These are exact string comparisons, with no style in sight.
    #[test]
    fn mini_spark_leaves_a_gap_for_unknown_and_a_baseline_for_zero() {
        let data = [Some(0), None, Some(50), None, Some(100)];
        assert_eq!(mini_spark(&data, 100, GraphStyle::Ascii), "_ - #");
        assert_eq!(mini_spark(&data, 100, GraphStyle::Block), "▁ ▄ █");
        // Braille reads two samples per cell: cells 1 and 3 are wholly
        // unknown and blank, the rest keep their dots.
        let braille = [
            Some(0),
            Some(0),
            None,
            None,
            Some(100),
            Some(100),
            None,
            None,
            Some(0),
            Some(0),
        ];
        assert_eq!(mini_spark(&braille, 100, GraphStyle::Braille), "⣀ ⣿ ⣀");
    }
}
