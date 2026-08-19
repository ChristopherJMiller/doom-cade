//! `arcade-attract` — the fullscreen attract/leaderboard/initials UI
//! (SPEC §10/§11). Thin eframe shell over the logic in `attract` (lib.rs):
//! decodes panel keys into [`Input`]s, drives [`step`], paints the state.
//!
//! Handoff: on [`AttractState::Done`] prints exactly one line
//! `ARCADE_INITIALS ABC` to stdout, flushes, exits 0. Never exits
//! otherwise.

use std::io::Write as _;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use attract::{
    env_flag, leaderboard_url_from_env, reel_screen, spaced, spawn_fetcher, step, AttractState,
    BoardCategory, BoardsCache, FetchResult, Input, ReelScreen, CHARSET, INITIALS_LEN,
};
use eframe::egui::{
    self, Align2, Color32, FontFamily, FontId, Pos2, Rect, RichText, Stroke, StrokeKind,
};

// --- DOOM status-bar palette (original values, no IWAD assets) -----------
const BG: Color32 = Color32::from_rgb(16, 12, 10);
const PANEL: Color32 = Color32::from_rgb(26, 18, 13);
const BLOOD: Color32 = Color32::from_rgb(154, 24, 16);
const BLOOD_BRIGHT: Color32 = Color32::from_rgb(224, 44, 28);
const OCHRE: Color32 = Color32::from_rgb(191, 151, 80);
const BROWN: Color32 = Color32::from_rgb(92, 60, 36);
const OFFWHITE: Color32 = Color32::from_rgb(232, 220, 196);
const GOLD: Color32 = Color32::from_rgb(255, 200, 60);
const DIM: Color32 = Color32::from_rgb(138, 122, 100);

fn main() -> eframe::Result<()> {
    let windowed = env_flag("ARCADE_WINDOWED");
    let unverified = env_flag("ARCADE_IWAD_UNVERIFIED");
    let rx = spawn_fetcher(leaderboard_url_from_env());

    let viewport = if windowed {
        egui::ViewportBuilder::default()
            .with_title("DOOM ARCADE")
            .with_inner_size([1280.0, 720.0])
    } else {
        egui::ViewportBuilder::default()
            .with_title("DOOM ARCADE")
            .with_fullscreen(true)
            .with_decorations(false)
    };
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "arcade-attract",
        options,
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);
            setup_style(&cc.egui_ctx);
            Ok(Box::new(AttractApp::new(rx, unverified)))
        }),
    )
}

/// Loads the bundled Anta face (OFL 1.1, `assets/fonts/anta/`) as the
/// primary proportional font — the same display face the leaderboard web
/// view embeds, so the cabinet screen and the LAN page share one identity.
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "anta".to_owned(),
        egui::FontData::from_static(include_bytes!(
            "../../../assets/fonts/anta/Anta-Regular.ttf"
        ))
        .into(),
    );
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "anta".to_owned());
    ctx.set_fonts(fonts);
}

fn setup_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    use egui::TextStyle;
    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(64.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(32.0, FontFamily::Proportional)),
        (
            TextStyle::Monospace,
            FontId::new(32.0, FontFamily::Monospace),
        ),
        (
            TextStyle::Button,
            FontId::new(32.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(20.0, FontFamily::Proportional),
        ),
    ]
    .into();
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = BG;
    style.visuals.override_text_color = Some(OFFWHITE);
    style.spacing.item_spacing = egui::vec2(16.0, 12.0);
    ctx.set_style(style);
}

struct AttractApp {
    state: Option<AttractState>,
    cache: BoardsCache,
    rx: Receiver<FetchResult>,
    /// Previous frame's Ctrl state, for rising-edge Confirm detection
    /// (Ctrl is a modifier in egui, not a `Key`).
    prev_ctrl: bool,
    unverified: bool,
}

impl AttractApp {
    fn new(rx: Receiver<FetchResult>, unverified: bool) -> Self {
        Self {
            state: Some(AttractState::initial(Instant::now())),
            cache: BoardsCache::default(),
            rx,
            prev_ctrl: false,
            unverified,
        }
    }

    /// Panel key map (SPEC §10): ArrowUp/ArrowDown cycle, either Ctrl =
    /// confirm, Space = backspace, Enter = Start. Esc does nothing.
    fn collect_inputs(&mut self, ctx: &egui::Context) -> Vec<Input> {
        let mut inputs = Vec::new();
        ctx.input(|i| {
            for ev in &i.events {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    repeat,
                    ..
                } = ev
                {
                    match key {
                        // Repeats allowed: holding the stick spins the wheel.
                        egui::Key::ArrowUp => inputs.push(Input::Up),
                        egui::Key::ArrowDown => inputs.push(Input::Down),
                        egui::Key::Space if !repeat => inputs.push(Input::Backspace),
                        egui::Key::Enter if !repeat => inputs.push(Input::Start),
                        // Esc intentionally unmapped.
                        _ => {}
                    }
                }
            }
            let ctrl = i.modifiers.ctrl;
            if ctrl && !self.prev_ctrl {
                inputs.push(Input::Confirm);
            }
            self.prev_ctrl = ctrl;
        });
        inputs
    }

    fn draw(&self, ctx: &egui::Context) {
        let t = ctx.input(|i| i.time);
        let blink_on = t.fract() < 0.55; // 1 Hz

        if self.unverified {
            egui::TopBottomPanel::top("unverified-banner")
                .frame(egui::Frame::new().fill(BLOOD).inner_margin(10))
                .show_separator_line(false)
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(spaced("UNVERIFIED IWAD"))
                                .color(Color32::BLACK)
                                .size(26.0)
                                .strong(),
                        );
                    });
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(24))
            .show(ctx, |ui| {
                match self.state.as_ref().expect("state present while drawing") {
                    AttractState::Idle { board_idx, .. } => match reel_screen(*board_idx) {
                        ReelScreen::Board(category) => {
                            self.draw_board_screen(ui, category);
                            draw_footer_press_start(ui, blink_on);
                        }
                        ReelScreen::PressStart => draw_press_start(ui, blink_on),
                    },
                    AttractState::InitialsEntry {
                        chars, cursor_char, ..
                    } => draw_initials(ui, chars, *cursor_char, t),
                    // Transient: the shell exits before this can be seen.
                    AttractState::Done(_) => {}
                }
            });

        if self.cache.stale {
            draw_offline_pip(ctx);
        }
    }

    fn draw_board_screen(&self, ui: &mut egui::Ui, category: BoardCategory) {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(spaced(category.title()))
                    .color(BLOOD_BRIGHT)
                    .size(56.0)
                    .strong(),
            );
            ui.add_space(6.0);
            let (rule, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width() * 0.55, 5.0),
                egui::Sense::hover(),
            );
            ui.painter().rect_filled(rule, 0.0, OCHRE);
            ui.add_space(28.0);

            match self.cache.board(category) {
                Some(board) if !board.entries.is_empty() => {
                    egui::Grid::new("board-grid")
                        .num_columns(3)
                        .spacing([56.0, 14.0])
                        .show(ui, |ui| {
                            for entry in board.entries.iter().take(10) {
                                let top = entry.rank == 1;
                                let color = if top { GOLD } else { OFFWHITE };
                                let rank_color = if top { GOLD } else { OCHRE };
                                ui.label(
                                    RichText::new(format!("{:>2}", entry.rank))
                                        .monospace()
                                        .color(rank_color)
                                        .size(34.0),
                                );
                                ui.label(
                                    RichText::new(&entry.initials)
                                        .monospace()
                                        .color(color)
                                        .size(34.0)
                                        .strong(),
                                );
                                ui.label(
                                    RichText::new(format!("{:>10}", entry.value_display))
                                        .monospace()
                                        .color(color)
                                        .size(34.0),
                                );
                                ui.end_row();
                            }
                        });
                }
                Some(_) => {
                    ui.add_space(60.0);
                    ui.label(RichText::new("NO RUNS YET").color(DIM).size(40.0));
                }
                None => {
                    ui.add_space(60.0);
                    ui.label(RichText::new("AWAITING SCORES...").color(DIM).size(40.0));
                }
            }
        });
    }
}

impl eframe::App for AttractApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        while let Ok(result) = self.rx.try_recv() {
            self.cache.apply(result, now);
        }

        let inputs = self.collect_inputs(ctx);
        let mut state = self.state.take().expect("state always present");
        for input in inputs {
            state = step(state, Some(input), now);
        }
        state = step(state, None, now);

        if let AttractState::Done(initials) = &state {
            // The handoff contract (SPEC §11): exactly one line, flushed,
            // exit 0. Nothing else in this process writes to stdout.
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "ARCADE_INITIALS {initials}");
            let _ = out.flush();
            std::process::exit(0);
        }
        self.state = Some(state);

        self.draw(ctx);

        // Keep blink/dwell/timeout timers moving.
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

fn draw_press_start(ui: &mut egui::Ui, blink_on: bool) {
    let h = ui.available_height();
    ui.vertical_centered(|ui| {
        ui.add_space(h * 0.30);
        // Transparent when "off" so the layout never jumps.
        let color = if blink_on {
            BLOOD_BRIGHT
        } else {
            Color32::TRANSPARENT
        };
        ui.label(
            RichText::new(spaced("PRESS START"))
                .color(color)
                .size(96.0)
                .strong(),
        );
        ui.add_space(30.0);
        ui.label(
            RichText::new("3 LETTERS  ·  5 MAPS  ·  1 LIFE")
                .color(OCHRE)
                .size(30.0),
        );
    });
}

fn draw_footer_press_start(ui: &mut egui::Ui, blink_on: bool) {
    let rect = ui.max_rect();
    let color = if blink_on {
        OFFWHITE
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().text(
        Pos2::new(rect.center().x, rect.bottom() - 10.0),
        Align2::CENTER_BOTTOM,
        spaced("PRESS START"),
        FontId::new(28.0, FontFamily::Proportional),
        color,
    );
}

fn draw_initials(ui: &mut egui::Ui, chars: &str, cursor_char: usize, t: f64) {
    let h = ui.available_height();
    // 1 Hz pulse for the active slot.
    let pulse = ((t * std::f64::consts::TAU).sin() * 0.5 + 0.5) as f32;

    ui.vertical_centered(|ui| {
        ui.add_space(h * 0.10);
        ui.label(
            RichText::new(spaced("ENTER YOUR INITIALS"))
                .color(BLOOD_BRIGHT)
                .size(48.0)
                .strong(),
        );
        ui.add_space(h * 0.08);

        // Three giant slots.
        let slot = egui::vec2(150.0, 190.0);
        let gap = 44.0;
        let tri_h = 26.0;
        let tri_half_w = 20.0;
        let tri_gap = 16.0;
        let total = egui::vec2(
            slot.x * INITIALS_LEN as f32 + gap * (INITIALS_LEN - 1) as f32,
            slot.y + 2.0 * (tri_h + tri_gap),
        );
        let (rect, _) = ui.allocate_exact_size(total, egui::Sense::hover());
        let painter = ui.painter();
        let active_idx = chars.len().min(INITIALS_LEN - 1);

        for i in 0..INITIALS_LEN {
            let x = rect.left() + i as f32 * (slot.x + gap);
            let slot_rect = Rect::from_min_size(Pos2::new(x, rect.top() + tri_h + tri_gap), slot);
            let is_active = i == active_idx && chars.len() < INITIALS_LEN;
            let stroke_color = if is_active {
                lerp_color(OCHRE, GOLD, pulse)
            } else {
                BROWN
            };
            painter.rect_filled(slot_rect, 6.0, PANEL);
            painter.rect_stroke(
                slot_rect,
                6.0,
                Stroke::new(4.0_f32, stroke_color),
                StrokeKind::Inside,
            );

            let (ch, color) = if i < chars.len() {
                (chars.as_bytes()[i] as char, OFFWHITE)
            } else if is_active {
                (CHARSET[cursor_char], lerp_color(OFFWHITE, GOLD, pulse))
            } else {
                ('_', DIM)
            };
            painter.text(
                slot_rect.center(),
                Align2::CENTER_CENTER,
                ch,
                FontId::new(120.0, FontFamily::Monospace),
                color,
            );

            // Up/down arrow hints on the active slot.
            if is_active {
                let cx = slot_rect.center().x;
                let tri_color = lerp_color(OCHRE, GOLD, pulse);
                let top = slot_rect.top() - tri_gap;
                painter.add(egui::Shape::convex_polygon(
                    vec![
                        Pos2::new(cx, top - tri_h),
                        Pos2::new(cx + tri_half_w, top),
                        Pos2::new(cx - tri_half_w, top),
                    ],
                    tri_color,
                    Stroke::NONE,
                ));
                let bottom = slot_rect.bottom() + tri_gap;
                painter.add(egui::Shape::convex_polygon(
                    vec![
                        Pos2::new(cx - tri_half_w, bottom),
                        Pos2::new(cx + tri_half_w, bottom),
                        Pos2::new(cx, bottom + tri_h),
                    ],
                    tri_color,
                    Stroke::NONE,
                ));
            }
        }

        ui.add_space(h * 0.08);
        ui.label(
            RichText::new("UP / DOWN — LETTER      FIRE — LOCK IN      USE — UNDO")
                .color(OCHRE)
                .size(26.0),
        );
    });
}

/// Small OFFLINE pip in the bottom-right corner when the last boards fetch
/// failed (cached data keeps rendering).
fn draw_offline_pip(ctx: &egui::Context) {
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("offline-pip"),
    ));
    let screen = ctx.screen_rect();
    let text_rect = painter.text(
        Pos2::new(screen.right() - 20.0, screen.bottom() - 14.0),
        Align2::RIGHT_BOTTOM,
        "OFFLINE",
        FontId::new(20.0, FontFamily::Proportional),
        DIM,
    );
    painter.circle_filled(
        Pos2::new(text_rect.left() - 14.0, text_rect.center().y),
        6.0,
        BLOOD_BRIGHT,
    );
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}
