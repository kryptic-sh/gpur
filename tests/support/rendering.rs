use super::*;

// --- graph glyph sets ----------------------------------------------------
//
// `--graphs` picks the glyph set every graph draws with. The negative half of
// each test below is the one that carries weight: a style that silently fell
// back to braille would satisfy any assertion that only looked for its own
// glyphs, because braille cells are what the default already paints.

/// A cell from the braille block, whichever dots are set.
fn is_braille(c: char) -> bool {
    ('\u{2800}'..='\u{28ff}').contains(&c)
}

/// `ui::EIGHTHS` without its blank level, and the `ui::ASCII_RAMP` levels a
/// waveform can actually index — it skips empty cells, so the `_` baseline
/// belongs to `mini_spark` alone and never appears in a graph.
const EIGHTHS: &str = "▁▂▃▄▅▆▇█";
const ASCII_RAMP: &str = ".-+#";

/// Screen rows of the first card's waveform, top and bottom inclusive.
/// `waveform_halves` writes its own `gpu%` at the graph's top-left and
/// `mem%` at its bottom-left, so the labels bracket exactly the rows the
/// graph owns — and nothing else on screen is mistakable for them, the
/// process table's column being upper-case `GPU%`.
fn waveform_rows(t: &mut Tui) -> (u16, u16) {
    // The graph is drawn only once a card has history and the card has rows
    // to spare, so wait for the labels rather than assume the first frame.
    t.wait_for("a drawn waveform", |s| {
        s.contains("gpu%") && s.contains("mem%")
    });
    let screen = t.parser.screen();
    let top = (0..ROWS)
        .find(|&y| row_text(screen, y).contains("gpu%"))
        .expect("a gpu% label");
    let bot = (top..ROWS)
        .find(|&y| row_text(screen, y).contains("mem%"))
        .expect("a mem% label below the gpu% one");
    (top, bot)
}

/// Every glyph the first card's waveform drew. Whole rows go in as they are:
/// the two labels and the card border carry no character from any of the
/// three glyph sets, while the process commands and card captions that do
/// (`.`, `-`, `+`, `·`) all sit outside these rows.
fn waveform_glyphs(t: &Tui, (top, bot): (u16, u16)) -> String {
    let screen = t.parser.screen();
    (top..=bot).map(|y| row_text(screen, y)).collect()
}

/// The default style, and the contrast the other two are read against.
#[test]
fn the_default_graph_style_draws_braille_dots() {
    let mut t = Tui::spawn(&[]);
    let rows = waveform_rows(&mut t);
    let glyphs = waveform_glyphs(&t, rows);
    assert!(
        glyphs.chars().any(is_braille),
        "no braille in the waveform:\n{glyphs}"
    );
    assert!(
        !glyphs
            .chars()
            .any(|c| EIGHTHS.contains(c) || ASCII_RAMP.contains(c)),
        "the braille waveform drew a cell glyph:\n{glyphs}"
    );
    assert!(
        meter_line(&t.screen_text(), "GPU ", "■·").is_some(),
        "meters are not on the unicode glyphs:\n{}",
        t.screen_text()
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

/// `--graphs block` must reach `draw_waveform_cells`' block arm — including
/// the fg/bg swap it draws the down-growing half with, which is where the
/// colour-quantization bug lived.
#[test]
fn block_graphs_draw_eighth_blocks_and_swap_colours_below_the_midline() {
    let mut t = Tui::spawn(&["--graphs", "block"]);
    let rows = waveform_rows(&mut t);
    let glyphs = waveform_glyphs(&t, rows);
    assert!(
        glyphs.chars().any(|c| EIGHTHS.contains(c)),
        "no eighth blocks in the waveform:\n{glyphs}"
    );
    assert!(
        !glyphs
            .chars()
            .any(|c| is_braille(c) || ASCII_RAMP.contains(c)),
        "the block waveform drew another style's glyph:\n{glyphs}"
    );
    let screen_text = t.screen_text();
    // `mini_spark` branches on the same setting, and it is the only other
    // braille in the UI — so one sweep of the whole screen catches it.
    assert!(
        !screen_text.chars().any(is_braille),
        "braille outside the waveform: a graph ignored --graphs block:\n{screen_text}"
    );
    assert!(
        meter_line(&screen_text, "GPU ", "■·").is_some(),
        "block mode changed the meter glyphs, which only ascii does:\n{screen_text}"
    );

    // Unicode has no upper-partial block, so the down-growing half paints the
    // *empty* part of a cell in the theme background over a bar-coloured one.
    // Prove that reaches the terminal: a cell below the midline whose
    // foreground is the page background, over a background that is not.
    let (top, bot) = rows;
    let screen = t.parser.screen();
    let page_bg = screen.cell(0, 0).expect("the top-left cell").bgcolor();
    // `waveform_halves` gives the up half `height / 2` rows and the rest to
    // the down half, so the down half starts here.
    let height = bot - top + 1;
    let midline = top + height / 2;
    let swapped = (midline..=bot).any(|y| {
        (0..COLS).any(|x| {
            screen
                .cell(y, x)
                .is_some_and(|c| c.fgcolor() == page_bg && c.bgcolor() != page_bg)
        })
    });
    assert!(
        swapped,
        "no fg/bg-swapped cell below the midline; the down half is not \
         drawing its partial cells:\n{glyphs}"
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

/// `--graphs ascii` is the setting for a terminal with no block or braille
/// coverage at all, so nothing it draws may reach outside 7-bit ascii — the
/// meters included, which are `draw_meter`'s own branch on the style.
#[test]
fn ascii_graphs_draw_the_ascii_ramp_and_plain_meters() {
    let mut t = Tui::spawn(&["--graphs", "ascii"]);
    let rows = waveform_rows(&mut t);
    let glyphs = waveform_glyphs(&t, rows);
    assert!(
        glyphs.chars().any(|c| ASCII_RAMP.contains(c)),
        "no ascii ramp in the waveform:\n{glyphs}"
    );
    assert!(
        !glyphs.chars().any(|c| is_braille(c) || EIGHTHS.contains(c)),
        "the ascii waveform drew a unicode glyph:\n{glyphs}"
    );
    let s = t.screen_text();
    assert!(
        !s.chars().any(is_braille),
        "braille outside the waveform: a graph ignored --graphs ascii:\n{s}"
    );
    let util = meter_line(&s, "GPU ", "=.").expect("an ascii GPU meter");
    let glyph_count = util.chars().filter(|c| "=.".contains(*c)).count();
    assert!(glyph_count >= 40, "GPU meter has no ascii bar: {util:?}");
    assert!(
        !s.contains('■'),
        "the meters kept their unicode fill glyph under --graphs ascii:\n{s}"
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

// --- colour modes --------------------------------------------------------
//
// Unix PTYs preserve the app's SGR encoding, so raw bytes prove which colour
// mode it chose. ConPTY can re-encode those sequences: on Windows assert the
// displayed cell colours and selection attributes instead.

/// The parameter list of every SGR (`ESC [ … m`) sequence in a byte stream.
#[cfg(unix)]
fn sgr_sequences(raw: &str) -> Vec<Vec<u16>> {
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some(i) = rest.find("\x1b[") {
        let tail = &rest[i + 2..];
        let end = tail
            .find(|c: char| !c.is_ascii_digit() && c != ';')
            .unwrap_or(tail.len());
        if tail[end..].starts_with('m') {
            out.push(
                tail[..end]
                    .split(';')
                    .filter_map(|p| p.parse().ok())
                    .collect(),
            );
        }
        rest = &tail[end..];
    }
    out
}

/// The colours those sequences set, tagged the way SGR spells them: `38`/`48`
/// introduce an extended colour whose next parameter picks the encoding — `5`
/// for one palette index, `2` for three rgb components — and 30-37 / 40-47
/// plus their bright ranges are direct palette codes. `39`/`49` are "back to
/// the terminal's own default", which is what mono paints with and is not a
/// colour; every other parameter is an attribute (bold, dim, reverse).
#[cfg(unix)]
fn colour_sgr(raw: &str) -> Vec<String> {
    let mut found = Vec::new();
    for params in sgr_sequences(raw) {
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                p @ (38 | 48) => {
                    let encoding = params.get(i + 1).copied().unwrap_or_default();
                    found.push(format!("{p};{encoding}"));
                    i += if encoding == 2 { 5 } else { 3 };
                }
                p @ (30..=37 | 40..=47 | 90..=97 | 100..=107) => {
                    found.push(p.to_string());
                    i += 1;
                }
                _ => i += 1,
            }
        }
    }
    found
}

fn mono_process_table_ready(screen: &str, pid: u32) -> bool {
    // The mock's own process is selected first by descending GPU memory.
    screen.contains("COMMAND") && screen.lines().find_map(row_pid) == Some(pid)
}

#[test]
fn mono_process_table_waits_for_fragmented_selected_row() {
    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    parser.process(b"Mock GPU 0\r\nPID USER COMMAND");
    assert!(!mono_process_table_ready(
        &parser.screen().contents(),
        12345
    ));
    parser.process(b"\r\n\x1b[7m12345 runneradmi 0 Compute 40 3064");
    assert!(!mono_process_table_ready(
        &parser.screen().contents(),
        12345
    ));
    parser.process(b"MiB\x1b[0m");
    assert!(mono_process_table_ready(&parser.screen().contents(), 12345));
    assert!(!mono_process_table_ready(&parser.screen().contents(), 1234));
    #[cfg(windows)]
    assert_mono_screen(parser.screen());
}

/// `NO_COLOR=1` has to reach the terminal as no colour at all — not as a
/// muted palette. The app still has to be usable, so this asserts its content
/// is drawn rather than that it drew nothing.
#[test]
fn no_color_renders_the_dashboard_with_visible_selection() {
    let mut t = Tui::spawn_with_env(&[], &[("NO_COLOR", Some("1"))]);
    t.wait_for("cards", |s| {
        s.contains("Mock GPU 0") && s.contains("Mock GPU 1")
    });
    let pid = t.child.process_id().expect("child pid");
    t.wait_for("selected process row", |s| mono_process_table_ready(s, pid));
    t.wait_for("meters", |s| meter_line(s, "GPU ", "■·").is_some());
    assert!(
        t.screen_text().contains("gpur v"),
        "banner missing under NO_COLOR:\n{}",
        t.screen_text()
    );
    #[cfg(windows)]
    assert_mono_screen(t.parser.screen());
    t.send("q");
    assert!(t.wait_exit().success());

    #[cfg(unix)]
    {
        let raw = String::from_utf8_lossy(&t.raw).into_owned();
        let colours = colour_sgr(&raw);
        assert!(
            colours.is_empty(),
            "NO_COLOR still emitted colour: {colours:?}"
        );
        // Mono is not "no styling": with no background to highlight with, the
        // theme substitutes reverse video for the cursor row. Losing that would
        // leave the selection invisible on exactly the terminals this mode is for.
        assert!(
            raw.contains("\x1b[7m"),
            "mono drew no reverse video, so the selection is unmarked"
        );
    }
}

/// The other route into mono, and the one that has to beat a `COLORTERM` the
/// environment still advertises — `detect_color_mode` reads `TERM=dumb`
/// first.
#[test]
fn term_dumb_renders_the_dashboard_in_mono() {
    let mut t = Tui::spawn_with_env(&[], &[("TERM", Some("dumb"))]);
    t.wait_for("cards", |s| s.contains("Mock GPU 0"));
    let pid = t.child.process_id().expect("child pid");
    t.wait_for("selected process row", |s| mono_process_table_ready(s, pid));
    #[cfg(windows)]
    assert_mono_screen(t.parser.screen());
    t.send("q");
    assert!(t.wait_exit().success());

    #[cfg(unix)]
    {
        let colours = colour_sgr(&String::from_utf8_lossy(&t.raw));
        assert!(
            colours.is_empty(),
            "TERM=dumb still emitted colour (COLORTERM won): {colours:?}"
        );
    }
}

/// A terminal that never claimed truecolor gets the 256-colour palette. This
/// is the regression that matters: a 24-bit escape sent to a 256-colour
/// terminal is exactly what `theme::paint`'s quantization exists to prevent,
/// and it is invisible in every other test because the suite pins
/// `COLORTERM=truecolor`.
#[test]
fn without_colorterm_the_app_quantizes_to_the_256_colour_palette() {
    let mut t = Tui::spawn_with_env(&[], &[("COLORTERM", None)]);
    t.wait_for("cards", |s| s.contains("Mock GPU 0"));
    t.wait_for("meters", |s| meter_line(s, "GPU ", "■·").is_some());
    #[cfg(windows)]
    {
        // ConPTY may re-encode palette colours as RGB. Assert the displayed
        // quantized values, not transparent forwarding of the app's SGR.
        let card = t
            .parser
            .screen()
            .cell(row_y(&t, "0·Mock GPU 0"), 0)
            .unwrap();
        assert!(
            matches!(
                card.fgcolor(),
                vt100::Color::Idx(183) | vt100::Color::Rgb(215, 175, 255)
            ),
            "unquantized foreground: {:?}",
            card.fgcolor()
        );
        assert!(
            matches!(
                card.bgcolor(),
                vt100::Color::Idx(59) | vt100::Color::Rgb(95, 95, 95)
            ),
            "unquantized background: {:?}",
            card.bgcolor()
        );
        for y in 0..ROWS {
            for x in 0..COLS {
                let cell = t.parser.screen().cell(y, x).unwrap();
                for color in [cell.fgcolor(), cell.bgcolor()] {
                    if let vt100::Color::Rgb(r, g, b) = color {
                        let levels = [0, 95, 135, 175, 215, 255];
                        let cube = [r, g, b].iter().all(|v| levels.contains(v));
                        let gray = r == g && g == b && (8..=238).contains(&r) && (r - 8) % 10 == 0;
                        assert!(cube || gray, "non-palette colour at {x},{y}: {color:?}");
                    }
                }
            }
        }
    }
    t.send("q");
    assert!(t.wait_exit().success());

    #[cfg(unix)]
    {
        let colours = colour_sgr(&String::from_utf8_lossy(&t.raw));
        let truecolor: Vec<&String> = colours
            .iter()
            .filter(|c| *c == "38;2" || *c == "48;2")
            .collect();
        assert!(
            truecolor.is_empty(),
            "24-bit colour on a terminal that only advertised 256: {} sequences",
            truecolor.len()
        );
        // Both halves of the frame are painted, so demand both: an app that only
        // ever set a foreground would pass a foreground-only assertion while its
        // backgrounds went out unquantized.
        assert!(
            colours.iter().any(|c| c == "38;5"),
            "no 256-palette foreground was ever set: {colours:?}"
        );
        assert!(
            colours.iter().any(|c| c == "48;5"),
            "no 256-palette background was ever set: {colours:?}"
        );
    }
}

#[cfg(windows)]
fn assert_mono_screen(screen: &vt100::Screen) {
    let mut reversed = false;
    for y in 0..ROWS {
        for x in 0..COLS {
            let cell = screen.cell(y, x).unwrap();
            assert_eq!(
                cell.fgcolor(),
                vt100::Color::Default,
                "foreground at {x},{y}"
            );
            assert_eq!(
                cell.bgcolor(),
                vt100::Color::Default,
                "background at {x},{y}"
            );
            reversed |= cell.inverse();
        }
    }
    assert!(reversed, "mono selection has no reverse video");
}
