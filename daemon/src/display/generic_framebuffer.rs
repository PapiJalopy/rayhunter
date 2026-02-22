use async_trait::async_trait;
use image::{AnimationDecoder, DynamicImage, codecs::gif::GifDecoder, imageops::FilterType};
use std::io::Cursor;
use std::time::Duration;

use crate::config;
use crate::display::DisplayState;
use rayhunter::analysis::analyzer::EventType;

use log::{error, info};
use tokio::sync::mpsc::Receiver;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use include_dir::{Dir, include_dir};



fn image_for_state<'a>(
    state: DisplayState,
    eff_default: &'a [u8],
    eff_yellow: Option<&'a [u8]>,
    eff_orange: Option<&'a [u8]>,
    eff_red: Option<&'a [u8]>,
) -> &'a [u8] {
    match state {
        DisplayState::WarningDetected { event_type } => match event_type {
            EventType::Informational => eff_default,
            EventType::Low => eff_yellow.unwrap_or(eff_default),
            EventType::Medium => eff_orange.unwrap_or(eff_default),
            EventType::High => eff_red.unwrap_or(eff_default),
        },
        DisplayState::Paused | DisplayState::Recording => eff_default,
    }
}

// Startup self-test: cycle through the UI visuals once so you can verify both
// the status bar style (color/pattern) and the image swapping on hardware.
// Runs only for ui_level == 3.
async fn run_startup_selftest(
    fb: &mut impl GenericFramebuffer,
    colorblind_mode: bool,
    eff_default: &[u8],
    eff_yellow: Option<&[u8]>,
    eff_orange: Option<&[u8]>,
    eff_red: Option<&[u8]>,
) {
    // (state, duration_ms)
    // Visuals covered:
    // - White: Paused
    // - Green: Recording
    // - Yellow: Low
    // - Orange: Medium
    // - Red: High
    let sequence: &[(DisplayState, u64)] = &[
        (DisplayState::Paused, 700),
        (DisplayState::Recording, 700),
        (
            DisplayState::WarningDetected {
                event_type: EventType::Low,
            },
            700,
        ),
        (
            DisplayState::WarningDetected {
                event_type: EventType::Medium,
            },
            700,
        ),
        (
            DisplayState::WarningDetected {
                event_type: EventType::High,
            },
            700,
        ),
    ];

    for (state, ms) in sequence {
        let (color, pattern) = display_style_from_state(*state, colorblind_mode);

        // Draw the image for this state
        let img_to_draw = image_for_state(*state, eff_default, eff_yellow, eff_orange, eff_red);
        fb.draw_img(img_to_draw).await;

        // Draw the status bar for this state
        let status_bar_height = 8;
        fb.draw_patterned_line(color, status_bar_height, pattern)
            .await;

        tokio::time::sleep(Duration::from_millis(*ms)).await;
    }
}

const REFRESH_RATE: u64 = 1000; //how often in milliseconds to refresh the display

#[derive(Copy, Clone)]
pub struct Dimensions {
    pub height: u32,
    pub width: u32,
}

#[derive(Copy, Clone)]
pub enum LinePattern {
    Solid,
    Dashed, // _ _ _ _
    Dotted, // . . . .
}

#[allow(dead_code)]
#[derive(Copy, Clone)]
pub enum Color {
    Red,
    Green,
    Blue,
    White,
    Black,
    Cyan,
    Yellow,
    Pink,
    Orange,
}

impl Color {
    fn rgb(self) -> (u8, u8, u8) {
        match self {
            Color::Red => (0xff, 0, 0),
            Color::Green => (0, 0xff, 0),
            Color::Blue => (0, 0, 0xff),
            Color::White => (0xff, 0xff, 0xff),
            Color::Black => (0, 0, 0),
            Color::Cyan => (0, 0xff, 0xff),
            Color::Yellow => (0xff, 0xff, 0),
            Color::Pink => (0xfe, 0x24, 0xff),
            Color::Orange => (0xff, 0xa5, 0),
        }
    }
}

fn display_style_from_state(state: DisplayState, colorblind_mode: bool) -> (Color, LinePattern) {
    match state {
        DisplayState::Paused => (Color::White, LinePattern::Solid),
        DisplayState::Recording => {
            if colorblind_mode {
                (Color::Blue, LinePattern::Solid)
            } else {
                (Color::Green, LinePattern::Solid)
            }
        }
        DisplayState::WarningDetected { event_type } => match event_type {
            EventType::Informational => {
                if colorblind_mode {
                    (Color::Blue, LinePattern::Solid)
                } else {
                    (Color::Green, LinePattern::Solid)
                }
            }
            EventType::Low => (Color::Yellow, LinePattern::Dotted),
            EventType::Medium => (Color::Orange, LinePattern::Dashed),
            EventType::High => (Color::Red, LinePattern::Solid),
        },
    }
}

#[async_trait]
pub trait GenericFramebuffer: Send + 'static {
    fn dimensions(&self) -> Dimensions;

    async fn write_buffer(&mut self, buffer: Vec<(u8, u8, u8)>); // rgb, row-wise, left-to-right, top-to-bottom

    async fn write_dynamic_image(&mut self, img: DynamicImage) {
        let dimensions = self.dimensions();
        let mut width = img.width();
        let mut height = img.height();
        let resized_img: DynamicImage;
        if height > dimensions.height || width > dimensions.width {
            resized_img = img.resize(dimensions.width, dimensions.height, FilterType::CatmullRom);
            width = dimensions.width.min(resized_img.width());
            height = dimensions.height.min(resized_img.height());
        } else {
            resized_img = img;
        }

        // NOTE: This expects RGBA; if someone supplies an RGB PNG this will panic.
        let img_rgba8 = resized_img.as_rgba8().unwrap();
        let mut buf = Vec::with_capacity((height * width).try_into().unwrap());
        for y in 0..height {
            for x in 0..width {
                let px = img_rgba8.get_pixel(x, y);
                buf.push((px[0], px[1], px[2]));
            }
        }

        self.write_buffer(buf).await
    }

    async fn draw_gif(&mut self, img_buffer: &[u8]) {
        let cursor = Cursor::new(img_buffer);
        if let Ok(decoder) = GifDecoder::new(cursor) {
            let frames: Vec<_> = decoder
                .into_frames()
                .filter_map(|f| f.ok())
                .map(|frame| {
                    let (numerator, _) = frame.delay().numer_denom_ms();
                    let img = DynamicImage::from(frame.into_buffer());
                    (img, numerator as u64)
                })
                .collect();

            for (img, delay_ms) in frames {
                self.write_dynamic_image(img).await;
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
        }
    }

    async fn draw_img(&mut self, img_buffer: &[u8]) {
        let img = image::load_from_memory(img_buffer).unwrap();
        self.write_dynamic_image(img).await
    }

    async fn draw_line(&mut self, color: Color, height: u32) {
        self.draw_patterned_line(color, height, LinePattern::Solid)
            .await
    }

    async fn draw_patterned_line(&mut self, color: Color, height: u32, pattern: LinePattern) {
        let width = self.dimensions().width;
        let mut buffer = Vec::with_capacity((height * width).try_into().unwrap());

        for _row in 0..height {
            for col in 0..width {
                let should_draw = match pattern {
                    LinePattern::Solid => true,
                    LinePattern::Dashed => (col / 4) % 2 == 0, // 4 pixels on, 4 pixels off
                    LinePattern::Dotted => col % 4 == 0,       // 1 pixel on, 3 pixels off
                };

                if should_draw {
                    buffer.push(color.rgb());
                } else {
                    buffer.push((0, 0, 0)); // Black background
                }
            }
        }

        self.write_buffer(buffer).await
    }
}

pub fn update_ui(
    task_tracker: &TaskTracker,
    config: &config::Config,
    mut fb: impl GenericFramebuffer,
    shutdown_token: CancellationToken,
    mut ui_update_rx: Receiver<DisplayState>,
) {
    static IMAGE_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/images/");
    let display_level = config.ui_level;
    if display_level == 0 {
        info!("Invisible mode, not spawning UI.");
        return;
    }

    let colorblind_mode = config.colorblind_mode;
    let mut display_style = display_style_from_state(DisplayState::Recording, colorblind_mode);
    let mut current_state = DisplayState::Recording;

    task_tracker.spawn(async move {
        // Preload assets (all from include_dir, so no disk IO at runtime)
        let gif_img: Option<&[u8]> = if display_level == 2 {
            Some(
                IMAGE_DIR
                    .get_file("orca.gif")
                    .expect("failed to read orca.gif")
                    .contents(),
            )
        } else {
            None
        };

        let eff_default: Option<&[u8]> = if display_level == 3 {
            Some(
                IMAGE_DIR
                    .get_file("eff.png")
                    .expect("failed to read eff.png")
                    .contents(),
            )
        } else {
            None
        };

        let eff_yellow: Option<&[u8]> = if display_level == 3 {
            IMAGE_DIR.get_file("eff_yellow.png").map(|f| f.contents())
        } else {
            None
        };

        let eff_orange: Option<&[u8]> = if display_level == 3 {
            IMAGE_DIR.get_file("eff_orange.png").map(|f| f.contents())
        } else {
            None
        };

        let eff_red: Option<&[u8]> = if display_level == 3 {
            IMAGE_DIR.get_file("eff_red.png").map(|f| f.contents())
        } else {
            None
        };

        // ---- Startup self-test (ui_level 3 only) ----
        if display_level == 3 {
            run_startup_selftest(
                &mut fb,
                colorblind_mode,
                eff_default.expect("eff.png not loaded"),
                eff_yellow,
                eff_orange,
                eff_red,
            )
            .await;

            // After self-test, return to normal appearance
            current_state = DisplayState::Recording;
            display_style = display_style_from_state(DisplayState::Recording, colorblind_mode);
        }
        // --------------------------------------------

        loop {
            if shutdown_token.is_cancelled() {
                info!("received UI shutdown");
                break;
            }
            match ui_update_rx.try_recv() {
                Ok(state) => {
                    current_state = state;
                    display_style = display_style_from_state(state, colorblind_mode);
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
                Err(e) => error!("error receiving framebuffer update message: {e}"),
            }

            let mut status_bar_height = 8;
            match display_level {
                2 => fb.draw_gif(gif_img.expect("orca.gif not loaded")).await,
                3 => {
                    let img_to_draw = image_for_state(
                        current_state,
                        eff_default.expect("eff.png not loaded"),
                        eff_yellow,
                        eff_orange,
                        eff_red,
                    );
                    fb.draw_img(img_to_draw).await
                }
                4 => {
                    status_bar_height = fb.dimensions().height;
                }
                128 => {
                    fb.draw_line(Color::Cyan, 128).await;
                    fb.draw_line(Color::Pink, 102).await;
                    fb.draw_line(Color::White, 76).await;
                    fb.draw_line(Color::Pink, 50).await;
                    fb.draw_line(Color::Cyan, 25).await;
                }
                // this branch is for ui_level 1, which is also the default if an
                // unknown value is used
                _ => {}
            };
            let (color, pattern) = display_style;
            fb.draw_patterned_line(color, status_bar_height, pattern)
                .await;
            tokio::time::sleep(Duration::from_millis(REFRESH_RATE)).await;
        }
    });
}
