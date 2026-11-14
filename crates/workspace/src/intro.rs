//! The startup intro, ported from WiSTV's `playIntro` (same timings and easing): the "WiS"
//! letters spring up over a vibrating string, "Code" slides in, then the overlay zooms out and
//! fades. A click or any key skips it.
//!
//! GPUI can't skew text or run SVG displacement filters, so the letters scale up from their
//! baseline instead of the original's skew and warp.

use std::{
    f32::consts::PI,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use gpui::{
    App, Context, EventEmitter, Font, FontWeight, Hsla, IntoElement, MouseButton, ParentElement,
    PathBuilder, Pixels, Point, Render, SharedString, Styled, Subscription, TextAlign, TextRun,
    Window, canvas, div, point, prelude::*, px, rgb,
};
use theme_settings::ThemeSettings;

use settings::Settings as _;

const VIEW_WIDTH: f32 = 640.;
const VIEW_HEIGHT: f32 = 260.;
const BASELINE: f32 = 150.;
const FONT_SIZE: f32 = 112.;
const STRING_Y: f32 = BASELINE + 24.;
const EXIT_AT: Duration = Duration::from_millis(2050);
const EXIT_LENGTH: Duration = Duration::from_millis(550);
const SKIPPED_EXIT_LENGTH: Duration = Duration::from_millis(280);
/// With reduced motion the animation starts from its settled frame and only fades out.
const REDUCED_MOTION_OFFSET_MS: f32 = 1400.;
const REDUCED_MOTION_EXIT_AT: Duration = Duration::from_millis(500);
const BACKGROUND: u32 = 0x06060a;

static SHOWN: AtomicBool = AtomicBool::new(false);

pub enum IntroEvent {
    Finished,
}

pub struct Intro {
    started_at: Instant,
    exit_at: Duration,
    exit_length: Duration,
    offset_ms: f32,
    finished: bool,
    _keystrokes: Subscription,
}

impl EventEmitter<IntroEvent> for Intro {}

impl Intro {
    /// The intro plays once per launch, in the first window that opens.
    pub fn claim_first_launch() -> bool {
        !SHOWN.swap(true, Ordering::Relaxed)
    }

    pub fn new(cx: &mut Context<Self>) -> Self {
        let reduce_motion = cx.reduce_motion();
        let keystrokes = cx.observe_keystrokes(|this, _, _, cx| this.skip(cx));
        Self {
            started_at: Instant::now(),
            exit_at: if reduce_motion {
                REDUCED_MOTION_EXIT_AT
            } else {
                EXIT_AT
            },
            exit_length: EXIT_LENGTH,
            offset_ms: if reduce_motion {
                REDUCED_MOTION_OFFSET_MS
            } else {
                0.
            },
            finished: false,
            _keystrokes: keystrokes,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_skipped(&self) -> bool {
        self.exit_length == SKIPPED_EXIT_LENGTH
    }

    #[cfg(test)]
    pub(crate) fn end_now_for_test(&mut self) {
        self.exit_at = Duration::ZERO;
        self.exit_length = Duration::from_millis(1);
        self.started_at = Instant::now() - Duration::from_secs(1);
    }

    fn skip(&mut self, cx: &mut Context<Self>) {
        let elapsed = self.started_at.elapsed();
        if elapsed < self.exit_at {
            self.exit_at = elapsed;
            self.exit_length = SKIPPED_EXIT_LENGTH;
            cx.notify();
        }
    }
}

fn clamp(value: f32) -> f32 {
    value.clamp(0., 1.)
}

fn ease_out(progress: f32) -> f32 {
    1. - (1. - progress).powi(3)
}

fn spring(progress: f32) -> f32 {
    if progress >= 1. {
        1.
    } else {
        1. - (-5. * progress).exp() * (9. * progress).cos()
    }
}

fn white(alpha: f32) -> Hsla {
    Hsla {
        h: 0.,
        s: 0.,
        l: 1.,
        a: alpha,
    }
}

fn shape(text: &str, font: &Font, size: Pixels, color: Hsla, window: &Window) -> gpui::ShapedLine {
    let run = TextRun {
        len: text.len(),
        font: font.clone(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(SharedString::from(text.to_string()), size, &[run], None)
}

fn paint_frame(
    bounds: gpui::Bounds<Pixels>,
    time: f32,
    exit_progress: f32,
    ui_font: &Font,
    window: &mut Window,
    cx: &mut App,
) {
    let scale = (bounds.size.width / px(VIEW_WIDTH)).min(bounds.size.height / px(VIEW_HEIGHT));
    let zoom = 1. + 0.07 * ease_out(exit_progress);
    let center = bounds.center();
    let to_screen = |x: f32, y: f32| -> Point<Pixels> {
        point(
            center.x + px((x - VIEW_WIDTH / 2.) * scale * zoom),
            center.y + px((y - VIEW_HEIGHT / 2.) * scale * zoom),
        )
    };
    let full_size = px(FONT_SIZE * scale * zoom);

    let bold = Font {
        weight: FontWeight::EXTRA_BOLD,
        ..ui_font.clone()
    };
    let regular = Font {
        weight: FontWeight::NORMAL,
        ..ui_font.clone()
    };
    let title = shape("WiS", &bold, px(FONT_SIZE), white(1.), window);
    let caption = shape("Code", &regular, px(FONT_SIZE), white(1.), window);
    let title_width = f32::from(title.width());
    let gap = FONT_SIZE * 0.24;
    let total = title_width + gap + f32::from(caption.width());
    let left = (VIEW_WIDTH - total) / 2.;

    let half = (total / 2. + 34.)
        * ease_out(clamp(time / 500.))
        * (1. - 0.55 * ease_out(clamp((time - 1300.) / 500.)));
    let amplitude = 20.
        * (PI * clamp((time - 100.) / 1300.)).sin()
        * (-(time - 500.).max(0.) / 450.).exp();
    let string_alpha = (1. - clamp((time - 1500.) / 450.)) * 0.92;
    if half > 0. && string_alpha > 0. {
        let mut builder = PathBuilder::stroke(px(2.4 * scale * zoom));
        let start = VIEW_WIDTH / 2. - half;
        let mut x = start;
        let mut first = true;
        while x <= VIEW_WIDTH / 2. + half + 0.1 {
            let u = (x - start) / (2. * half).max(1.);
            let y = STRING_Y
                + amplitude * (u * PI).sin() * (u * 5. * PI - time * 0.014).sin();
            if first {
                builder.move_to(to_screen(x, y));
                first = false;
            } else {
                builder.line_to(to_screen(x, y));
            }
            x += 5.;
        }
        if let Ok(path) = builder.build() {
            window.paint_path(path, white(string_alpha));
        }
    }

    for (index, letter) in ["W", "i", "S"].into_iter().enumerate() {
        let progress = clamp((time - 300. - index as f32 * 110.) / 750.);
        let alpha = clamp(progress * 4.);
        let size = spring(progress).max(0.02);
        if alpha <= 0. {
            continue;
        }
        let prefix_width = |length: usize| match length {
            0 => 0.,
            3 => title_width,
            _ => f32::from(shape(&"WiS"[..length], &bold, px(FONT_SIZE), white(1.), window).width()),
        };
        let letter_start = prefix_width(index);
        let letter_end = prefix_width(index + 1);
        let letter_center = left + (letter_start + letter_end) / 2.;
        let font_size = full_size * size;
        let line = shape(letter, &bold, font_size, white(alpha), window);
        let font_id = window.text_system().resolve_font(&bold);
        let ascent = window.text_system().ascent(font_id, font_size);
        let baseline = to_screen(letter_center, BASELINE);
        let origin = point(baseline.x - line.width() / 2., baseline.y - ascent);
        line.paint(origin, font_size, TextAlign::Left, None, window, cx)
            .ok();
    }

    let caption_progress = ease_out(clamp((time - 760.) / 600.));
    if caption_progress > 0. {
        let line = shape(
            "Code",
            &regular,
            full_size,
            white(0.6 * caption_progress),
            window,
        );
        let font_id = window.text_system().resolve_font(&regular);
        let ascent = window.text_system().ascent(font_id, full_size);
        let baseline = to_screen(
            left + title_width + gap + (1. - caption_progress) * 26.,
            BASELINE,
        );
        line.paint(
            point(baseline.x, baseline.y - ascent),
            full_size,
            TextAlign::Left,
            None,
            window,
            cx,
        )
        .ok();
    }
}

impl Render for Intro {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let elapsed = self.started_at.elapsed();
        let time = self.offset_ms + elapsed.as_secs_f32() * 1000.;
        let exit_progress = clamp(
            elapsed.saturating_sub(self.exit_at).as_secs_f32() / self.exit_length.as_secs_f32(),
        );
        if exit_progress >= 1. {
            if !self.finished {
                self.finished = true;
                cx.emit(IntroEvent::Finished);
            }
        } else {
            window.request_animation_frame();
        }
        let ui_font = ThemeSettings::get_global(cx).ui_font.clone();

        div()
            .id("intro")
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgb(BACKGROUND))
            .opacity(1. - ease_out(exit_progress))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.skip(cx)),
            )
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, cx| {
                        paint_frame(bounds, time, exit_progress, &ui_font, window, cx)
                    },
                )
                .size_full(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_intro_plays_once_per_launch() {
        assert!(Intro::claim_first_launch());
        assert!(!Intro::claim_first_launch());
    }

    #[test]
    fn test_easing_settles_at_one() {
        assert_eq!(ease_out(1.), 1.);
        assert_eq!(spring(1.), 1.);
        assert_eq!(spring(0.), 0.);
        assert_eq!(clamp(2.), 1.);
        assert_eq!(clamp(-1.), 0.);
    }
}
