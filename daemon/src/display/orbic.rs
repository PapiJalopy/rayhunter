use crate::config;
use crate::display::DisplayState;
use crate::display::generic_framebuffer::{self, Dimensions, GenericFramebuffer};
use async_trait::async_trait;
use log::{debug, warn};

use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

const FB_PATH: &str = "/dev/fb0";

// Orbic display sysfs controls discovered on device.
// These are used to re-enable the display after the stock UI sleeps it.
const SYSFS_BASE: &str = "/sys/devices/78b6000.spi/spi_master/spi1/spi1.0";
const SYSFS_SLEEP_MODE: &str = "/sys/devices/78b6000.spi/spi_master/spi1/spi1.0/sleep_mode";
const SYSFS_BL_GPIO: &str = "/sys/devices/78b6000.spi/spi_master/spi1/spi1.0/bl_gpio";
const SYSFS_DISPLAY_ON: &str = "/sys/devices/78b6000.spi/spi_master/spi1/spi1.0/display_on";

// Power management autosleep control (this is what fixed the battery-only flicker).
const SYSFS_AUTOSLEEP: &str = "/sys/power/autosleep";

async fn read_sysfs_bool(path: &str) -> Option<bool> {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => match s.trim() {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        },
        Err(_) => None,
    }
}

async fn read_sysfs_trimmed(path: &str) -> Option<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => Some(s.trim().to_string()),
        Err(_) => None,
    }
}

async fn write_sysfs_str(path: &str, value: &str) {
    let mut v = String::from(value);
    if !v.ends_with('\n') {
        v.push('\n');
    }

    if let Err(e) = tokio::fs::write(path, v.as_bytes()).await {
        warn!("failed writing {:?} to {}: {}", value, path, e);
    }
}

async fn write_sysfs_one(path: &str) {
    write_sysfs_str(path, "1").await
}

fn spawn_keep_screen_on(task_tracker: &TaskTracker, shutdown_token: CancellationToken) {
    task_tracker.spawn(async move {
        // If the expected sysfs does not exist, do nothing.
        if tokio::fs::metadata(SYSFS_BASE).await.is_err() {
            warn!("keep_screen_on enabled, but Orbic sysfs path not found: {SYSFS_BASE}");
            return;
        }

        // Capture original autosleep so we can restore it on shutdown (best-effort).
        // Typical values are "mem" or "off".
        let original_autosleep = read_sysfs_trimmed(SYSFS_AUTOSLEEP).await;

        // Poll frequency to catch sleeping.
        const POLL_MS: u64 = 500;

        loop {
            if shutdown_token.is_cancelled() {
                break;
            }

            // Critical fix: prevent the system from entering suspend/resume loops on battery.
            // autosleep=off stops constant redraw while on battery power due to wake and sleep sources.
            let autosleep = read_sysfs_trimmed(SYSFS_AUTOSLEEP).await;
            if autosleep.as_deref() != Some("off") {
                debug!("keep_screen_on: forcing autosleep=off (was {:?})", autosleep);
                write_sysfs_str(SYSFS_AUTOSLEEP, "off").await;
            }

            // On Orbic, sleep_mode=0 and bl_gpio=0 indicates the display is asleep.
            let sleep_mode = read_sysfs_bool(SYSFS_SLEEP_MODE).await;
            let bl_gpio = read_sysfs_bool(SYSFS_BL_GPIO).await;

            let should_wake = matches!(sleep_mode, Some(false)) || matches!(bl_gpio, Some(false));

            if should_wake {
                debug!(
                    "keep_screen_on: waking display (sleep_mode={:?}, bl_gpio={:?})",
                    sleep_mode, bl_gpio
                );

                // Observed wake sequence:
                // 1) display_on=1 (usually stays 1, but set anyway)
                // 2) bl_gpio=1 (backlight)
                // 3) sleep_mode=1 (resume UI)
                write_sysfs_one(SYSFS_DISPLAY_ON).await;
                write_sysfs_one(SYSFS_BL_GPIO).await;
                write_sysfs_one(SYSFS_SLEEP_MODE).await;
            }

            tokio::time::sleep(std::time::Duration::from_millis(POLL_MS)).await;
        }

        // Best-effort restore of autosleep on shutdown.
        if let Some(orig) = original_autosleep {
            // Only restore if we actually captured something meaningful.
            // (If it was already "off", restoring "off" is harmless.)
            debug!("keep_screen_on: restoring autosleep to {:?}", orig);
            write_sysfs_str(SYSFS_AUTOSLEEP, &orig).await;
        }
    });
}

#[derive(Copy, Clone, Default)]
struct Framebuffer;

#[async_trait]
impl GenericFramebuffer for Framebuffer {
    fn dimensions(&self) -> Dimensions {
        // TODO actually poll for this, maybe w/ fbset?
        Dimensions {
            height: 128,
            width: 128,
        }
    }

    async fn write_buffer(&mut self, buffer: Vec<(u8, u8, u8)>) {
        let mut raw_buffer = Vec::with_capacity(buffer.len() * 2);
        for (r, g, b) in buffer {
            let mut rgb565: u16 = (r as u16 & 0b11111000) << 8;
            rgb565 |= (g as u16 & 0b11111100) << 3;
            rgb565 |= (b as u16) >> 3;
            raw_buffer.extend(rgb565.to_le_bytes());
        }

        tokio::fs::write(FB_PATH, &raw_buffer).await.unwrap();
    }
}

pub fn update_ui(
    task_tracker: &TaskTracker,
    config: &config::Config,
    shutdown_token: CancellationToken,
    ui_update_rx: Receiver<DisplayState>,
) {
    if config.keep_screen_on {
        spawn_keep_screen_on(task_tracker, shutdown_token.clone());
    }

    generic_framebuffer::update_ui(
        task_tracker,
        config,
        Framebuffer,
        shutdown_token,
        ui_update_rx,
    )
}
