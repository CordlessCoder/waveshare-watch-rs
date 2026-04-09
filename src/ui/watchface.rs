// Watchface - renders to any DrawTarget (framebuffer or display)

use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Circle, PrimitiveStyle, Rectangle, RoundedRectangle};
use embedded_graphics::text::{Alignment, Text};

use crate::board;
use crate::ui::segments;

const SCREEN_CX: i32 = board::LCD_WIDTH as i32 / 2;
const TIME_Y: i32 = 60;
const TIME_DW: i32 = 36;
const TIME_DH: i32 = 64;
const TIME_GAP: i32 = 6;
const TIME_CW: i32 = 14;
const TIME_TOTAL_W: i32 = 6 * TIME_DW + 2 * TIME_CW + 7 * TIME_GAP;
const TIME_PAD: i32 = 4;
const BATTERY_Y: i32 = 175;
const BATTERY_PAD_Y: i32 = 4;
const BATTERY_REGION_W: i32 = 240;
const BATTERY_REGION_H: i32 = 92;
const GYRO_CX: i32 = 205;
const GYRO_CY: i32 = 370;
const GYRO_R: i32 = 50;
const BALL_R: i32 = 8;
const GYRO_FLUSH_PAD: i32 = 2;

#[derive(Clone, Copy, Debug)]
pub struct FlushRegion {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl FlushRegion {
    const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self {
            x: x as u16,
            y: y as u16,
            w: w as u16,
            h: h as u16,
        }
    }

    fn union(self, other: Self) -> Self {
        let x1 = (self.x as i32).min(other.x as i32);
        let y1 = (self.y as i32).min(other.y as i32);
        let x2 = (self.x as i32 + self.w as i32).max(other.x as i32 + other.w as i32);
        let y2 = (self.y as i32 + self.h as i32).max(other.y as i32 + other.h as i32);
        Self::new(x1, y1, x2 - x1, y2 - y1)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RenderOutcome {
    pub full_redraw: bool,
    pub time_region: Option<FlushRegion>,
    pub battery_region: Option<FlushRegion>,
    pub gyro_region: Option<FlushRegion>,
}

pub struct WatchFace {
    hours: u8, minutes: u8, seconds: u8,
    battery_percent: u8, battery_voltage: u16, is_charging: bool,
    accel_x: i16, accel_y: i16, accel_z: i16,
    prev_ball_x: i32, prev_ball_y: i32,
    day: u8, month: u8, year: u8,
    full_redraw: bool, time_changed: bool, battery_changed: bool, gyro_changed: bool,
    pub wifi_connected: bool,
    pub gyro_enabled: bool,
}

impl WatchFace {
    pub fn new() -> Self {
        Self {
            hours: 0, minutes: 0, seconds: 0,
            battery_percent: 0, battery_voltage: 0, is_charging: false,
            accel_x: 0, accel_y: 0, accel_z: 0,
            prev_ball_x: GYRO_CX, prev_ball_y: GYRO_CY,
            day: 6, month: 4, year: 26,
            full_redraw: true, time_changed: false, battery_changed: false, gyro_changed: false,
            wifi_connected: false,
            gyro_enabled: false, // off by default to save battery
        }
    }

    pub fn update_time(&mut self, h: u8, m: u8, s: u8) {
        if self.hours != h || self.minutes != m || self.seconds != s {
            self.hours = h; self.minutes = m; self.seconds = s;
            self.time_changed = true;
        }
    }

    pub fn update_date(&mut self, day: u8, month: u8, year: u8) {
        self.day = day; self.month = month; self.year = year;
    }

    pub fn update_battery(&mut self, pct: u8, mv: u16, chg: bool) {
        if self.battery_percent != pct || self.battery_voltage != mv || self.is_charging != chg {
            self.battery_percent = pct;
            self.battery_voltage = mv;
            self.is_charging = chg;
            self.battery_changed = true;
        }
    }

    pub fn update_accel(&mut self, x: f32, y: f32, z: f32) {
        self.accel_x = (x * 100.0) as i16;
        self.accel_y = (y * 100.0) as i16;
        self.accel_z = (z * 100.0) as i16;
        let (nx, ny) = Self::projected_ball_position(self.accel_x, self.accel_y);
        if (nx - self.prev_ball_x).unsigned_abs() >= 2 || (ny - self.prev_ball_y).unsigned_abs() >= 2 {
            self.gyro_changed = true;
        }
    }

    pub fn force_redraw(&mut self) { self.full_redraw = true; }

    /// Toggle gyroscope display. Returns new state.
    pub fn toggle_gyro(&mut self) -> bool {
        self.gyro_enabled = !self.gyro_enabled;
        self.full_redraw = true;
        self.gyro_enabled
    }

    /// Check if tap is in gyro zone
    pub fn is_gyro_zone(y: u16) -> bool {
        y as i32 >= GYRO_CY - GYRO_R - 20 && (y as i32) <= GYRO_CY + GYRO_R + 20
    }

    pub fn needs_render(&self) -> bool {
        self.full_redraw || self.time_changed || self.battery_changed || self.gyro_changed
    }

    /// Always-On-Display renderer.
    /// Strategy:
    ///   * Pure black background → on AMOLED these pixels are physically OFF (zero current).
    ///   * Only HH:MM is drawn (no seconds), in dim white using the same 7-segment font.
    ///   * Tiny battery percentage in the corner.
    ///   * Vertical position is shifted by `(minutes % 8) - 4` pixels to avoid pixel
    ///     burn-in over months of always-on use, mimicking what Apple Watch does.
    pub fn render_aod<D: DrawTarget<Color = Rgb565>>(&mut self, d: &mut D) -> Result<(), D::Error> {
        let w = board::LCD_WIDTH as i32;
        let h = board::LCD_HEIGHT as i32;

        // Full clear to black — this is the cheapest possible AMOLED state.
        Rectangle::new(Point::zero(), Size::new(w as u32, h as u32))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(d)?;

        // Anti burn-in: shift the time block by a few pixels based on the current minute.
        let shift_x = ((self.minutes as i32) % 9) - 4;
        let shift_y = ((self.minutes as i32 / 9) % 9) - 4;

        let cx = SCREEN_CX + shift_x;
        let cy = h / 2 - 32 + shift_y;

        // HH:MM only (no seconds, no extra widgets).
        // We use a slightly dimmed white (CSS_LIGHT_GRAY = ~0.8 brightness) to further reduce power
        // because each AMOLED sub-pixel scales current with luminance.
        let dim_white = Rgb565::new(20, 40, 20); // ~50% gray, looks white-ish on AMOLED but uses ~half the current

        // Draw HH:MM using the segment renderer. Pass 99 for seconds to indicate "skip seconds".
        // The segments::draw_time function draws all 8 chars; we'll use a custom call.
        segments::draw_hhmm(d, cx, cy, self.hours, self.minutes, dim_white, Rgb565::BLACK)?;

        // Tiny battery indicator at the bottom (3 chars max: "99%")
        let mut buf = [0u8; 4];
        let s = fmt_bat_short(&mut buf, self.battery_percent);
        let style = MonoTextStyle::new(&FONT_10X20, Rgb565::new(8, 16, 8));
        Text::with_alignment(s, Point::new(cx, cy + 110), style, Alignment::Center).draw(d)?;

        // Reset dirty flags so the normal renderer does a full redraw on wake.
        self.full_redraw = true;
        self.time_changed = false;
        self.battery_changed = false;
        self.gyro_changed = false;
        Ok(())
    }

    pub fn render<D: DrawTarget<Color = Rgb565>>(&mut self, d: &mut D) -> Result<RenderOutcome, D::Error> {
        if !self.full_redraw && !self.time_changed && !self.battery_changed && !self.gyro_changed {
            return Ok(RenderOutcome::default());
        }

        let w = board::LCD_WIDTH as i32;
        let h = board::LCD_HEIGHT as i32;
        let cx = SCREEN_CX;

        let cyan = MonoTextStyle::new(&FONT_10X20, Rgb565::CYAN);
        let yellow = MonoTextStyle::new(&FONT_10X20, Rgb565::YELLOW);
        let dim = MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_GRAY);

        if self.full_redraw {
            // Clear
            Rectangle::new(Point::zero(), Size::new(w as u32, h as u32))
                .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
                .draw(d)?;

            // WiFi indicator (well inside rounded screen area)
            let wifi_color = if self.wifi_connected { Rgb565::GREEN } else { Rgb565::RED };
            Circle::new(Point::new(60, 14), 8)
                .into_styled(PrimitiveStyle::with_fill(wifi_color))
                .draw(d)?;

            // Title
            Text::with_alignment("RUST WATCH", Point::new(cx, 38), cyan, Alignment::Center).draw(d)?;

            // Time (y=60, 64px tall, ends at y=124)
            segments::draw_time(d, cx, TIME_Y, self.hours, self.minutes, self.seconds,
                Rgb565::WHITE, Rgb565::BLACK)?;

            // Date FR under time
            let mut date_buf = [0u8; 12];
            let ds = fmt_date_fr(&mut date_buf, self.day, self.month, self.year);
            Text::with_alignment(ds, Point::new(cx, 150), dim, Alignment::Center).draw(d)?;

            // Battery bar + percentage (more space below date)
            self.draw_battery(d, cx, 175)?;

            // Gyro section (only when enabled)
            if self.gyro_enabled {
                Circle::new(Point::new(GYRO_CX - GYRO_R, GYRO_CY - GYRO_R), (GYRO_R * 2) as u32)
                    .into_styled(PrimitiveStyle::with_stroke(Rgb565::CSS_DARK_GRAY, 2))
                    .draw(d)?;
                Text::with_alignment("GYRO", Point::new(GYRO_CX, GYRO_CY - GYRO_R - 10), dim, Alignment::Center).draw(d)?;
                self.draw_gyro_ball(d)?;
            } else {
                Text::with_alignment("TAP FOR GYRO", Point::new(GYRO_CX, GYRO_CY), dim, Alignment::Center).draw(d)?;
            }

            // Footer
            Text::with_alignment("100% Rust // ESP32-S3", Point::new(cx, h - 15), yellow, Alignment::Center).draw(d)?;

            self.full_redraw = false;
            self.time_changed = false;
            self.battery_changed = false;
            self.gyro_changed = false;
            return Ok(RenderOutcome {
                full_redraw: true,
                ..RenderOutcome::default()
            });
        }

        let mut outcome = RenderOutcome::default();

        if self.time_changed {
            Rectangle::new(
                Point::new(Self::time_region().x as i32, Self::time_region().y as i32),
                Size::new(Self::time_region().w as u32, Self::time_region().h as u32),
            )
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(d)?;
            segments::draw_time(d, cx, TIME_Y, self.hours, self.minutes, self.seconds,
                Rgb565::WHITE, Rgb565::BLACK)?;
            self.time_changed = false;
            outcome.time_region = Some(Self::time_region());
        }

        if self.battery_changed {
            Rectangle::new(
                Point::new(Self::battery_region().x as i32, Self::battery_region().y as i32),
                Size::new(Self::battery_region().w as u32, Self::battery_region().h as u32),
            )
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(d)?;
            self.draw_battery(d, cx, BATTERY_Y)?;
            self.battery_changed = false;
            outcome.battery_region = Some(Self::battery_region());
        }

        if self.gyro_changed && self.gyro_enabled {
            outcome.gyro_region = self.draw_gyro_ball(d)?;
            self.gyro_changed = false;
        }

        Ok(outcome)
    }

    fn draw_gyro_ball<D: DrawTarget<Color = Rgb565>>(&mut self, d: &mut D) -> Result<Option<FlushRegion>, D::Error> {
        let (nx, ny) = Self::projected_ball_position(self.accel_x, self.accel_y);

        if (nx - self.prev_ball_x).unsigned_abs() < 2 && (ny - self.prev_ball_y).unsigned_abs() < 2 {
            return Ok(None);
        }

        // Erase old
        Rectangle::new(
            Point::new(self.prev_ball_x - BALL_R, self.prev_ball_y - BALL_R),
            Size::new(BALL_R as u32 * 2, BALL_R as u32 * 2),
        ).into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK)).draw(d)?;

        // Draw new
        Rectangle::new(
            Point::new(nx - BALL_R, ny - BALL_R),
            Size::new(BALL_R as u32 * 2, BALL_R as u32 * 2),
        ).into_styled(PrimitiveStyle::with_fill(Rgb565::GREEN)).draw(d)?;

        let dirty = Self::ball_region(self.prev_ball_x, self.prev_ball_y)
            .union(Self::ball_region(nx, ny));
        self.prev_ball_x = nx;
        self.prev_ball_y = ny;
        Ok(Some(dirty))
    }

    fn draw_battery<D: DrawTarget<Color = Rgb565>>(&self, d: &mut D, cx: i32, y: i32) -> Result<(), D::Error> {
        let bw = 200i32; let bh = 20i32; let bx = cx - bw/2;

        RoundedRectangle::with_equal_corners(
            Rectangle::new(Point::new(bx, y), Size::new(bw as u32, bh as u32)),
            Size::new(4, 4),
        ).into_styled(PrimitiveStyle::with_stroke(Rgb565::WHITE, 2)).draw(d)?;

        let fw = ((self.battery_percent as i32).min(100) * (bw - 6)) / 100;
        let fc = if self.battery_percent > 50 { Rgb565::GREEN }
            else if self.battery_percent > 20 { Rgb565::YELLOW }
            else { Rgb565::RED };

        if fw > 0 {
            Rectangle::new(Point::new(bx+3, y+3), Size::new(fw as u32, (bh-6) as u32))
                .into_styled(PrimitiveStyle::with_fill(fc)).draw(d)?;
        }

        let mut buf = [0u8; 16];
        let s = fmt_batt(&mut buf, self.battery_percent, self.is_charging);
        let st = if self.is_charging {
            MonoTextStyle::new(&FONT_10X20, Rgb565::GREEN)
        } else {
            MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE)
        };
        Text::with_alignment(s, Point::new(cx, y + bh + 25), st, Alignment::Center).draw(d)?;
        Ok(())
    }

    pub fn time_region() -> FlushRegion {
        FlushRegion::new(
            SCREEN_CX - TIME_TOTAL_W / 2 - TIME_PAD,
            TIME_Y - TIME_PAD,
            TIME_TOTAL_W + TIME_PAD * 2,
            TIME_DH + TIME_PAD * 2,
        )
    }

    pub fn battery_region() -> FlushRegion {
        FlushRegion::new(
            SCREEN_CX - BATTERY_REGION_W / 2,
            BATTERY_Y - BATTERY_PAD_Y,
            BATTERY_REGION_W,
            BATTERY_REGION_H + BATTERY_PAD_Y * 2,
        )
    }

    fn ball_region(x: i32, y: i32) -> FlushRegion {
        FlushRegion::new(
            x - BALL_R - GYRO_FLUSH_PAD,
            y - BALL_R - GYRO_FLUSH_PAD,
            BALL_R * 2 + GYRO_FLUSH_PAD * 2,
            BALL_R * 2 + GYRO_FLUSH_PAD * 2,
        )
    }

    fn projected_ball_position(accel_x: i16, accel_y: i16) -> (i32, i32) {
        let max_off = GYRO_R - BALL_R - 4;
        let bx = (-(accel_y as i32) * max_off / 100).clamp(-max_off, max_off);
        let by = ((accel_x as i32) * max_off / 100).clamp(-max_off, max_off);
        (GYRO_CX + bx, GYRO_CY + by)
    }
}

fn fmt_date_fr<'a>(buf: &'a mut [u8; 12], d: u8, m: u8, y: u8) -> &'a str {
    // Format: "DD/MM/20YY"
    let mut p = 0;
    buf[p] = b'0' + d / 10; p += 1;
    buf[p] = b'0' + d % 10; p += 1;
    buf[p] = b'/'; p += 1;
    buf[p] = b'0' + m / 10; p += 1;
    buf[p] = b'0' + m % 10; p += 1;
    buf[p] = b'/'; p += 1;
    buf[p] = b'2'; p += 1;
    buf[p] = b'0'; p += 1;
    buf[p] = b'0' + y / 10; p += 1;
    buf[p] = b'0' + y % 10; p += 1;
    core::str::from_utf8(&buf[..p]).unwrap_or("??/??/????")
}

fn fmt_batt<'a>(buf: &'a mut [u8; 16], pct: u8, chg: bool) -> &'a str {
    let mut p = 0;
    if pct >= 100 { buf[p]=b'1'; p+=1; buf[p]=b'0'; p+=1; buf[p]=b'0'; p+=1; }
    else if pct >= 10 { buf[p]=b'0'+pct/10; p+=1; buf[p]=b'0'+pct%10; p+=1; }
    else { buf[p]=b'0'+pct; p+=1; }
    buf[p]=b'%'; p+=1;
    if chg { for &c in b" CHG" { buf[p]=c; p+=1; } }
    core::str::from_utf8(&buf[..p]).unwrap_or("?%")
}

fn fmt_bat_short<'a>(buf: &'a mut [u8; 4], pct: u8) -> &'a str {
    let mut p = 0;
    if pct >= 100 { buf[p]=b'1'; p+=1; buf[p]=b'0'; p+=1; buf[p]=b'0'; p+=1; }
    else if pct >= 10 { buf[p]=b'0'+pct/10; p+=1; buf[p]=b'0'+pct%10; p+=1; }
    else { buf[p]=b'0'+pct; p+=1; }
    buf[p]=b'%'; p+=1;
    core::str::from_utf8(&buf[..p]).unwrap_or("?%")
}

fn fmt_mv<'a>(buf: &'a mut [u8; 12], mv: u16) -> &'a str {
    let mut p = 0;
    if mv >= 1000 { buf[p]=b'0'+(mv/1000) as u8; p+=1; }
    buf[p]=b'0'+((mv/100)%10) as u8; p+=1;
    buf[p]=b'0'+((mv/10)%10) as u8; p+=1;
    buf[p]=b'0'+(mv%10) as u8; p+=1;
    for &c in b"mV" { buf[p]=c; p+=1; }
    core::str::from_utf8(&buf[..p]).unwrap_or("????mV")
}
