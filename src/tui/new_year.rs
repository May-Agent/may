//! New Year easter egg: a greeting above the input box on December 31 and
//! January 1 (the user's timezone) that plays character fireworks over the
//! chat area when selected.
//!
//! The animation is a pure function of its frame number. The tick event
//! (every 50 ms) advances the frame; painting only reads it, so the render
//! path does no work beyond writing cells into the frame buffer.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

/// The greeting line shown above the input box.
pub const GREETING: &str = "Happy New Year! Press Ctrl+F for fireworks";

/// How often the date is re-checked against the clock.
const RECHECK: Duration = Duration::from_secs(10);

/// The user's timezone for the date check: `[time] timezone`, else the host's.
///
/// # Panics
///
/// Panics when the configured name is not in the timezone database; the
/// config parser rejects such a name, so it cannot reach here.
#[must_use]
pub fn user_zone(time: &crate::config::TimeConfig) -> jiff::tz::TimeZone {
    time.timezone
        .as_ref()
        .map_or_else(jiff::tz::TimeZone::system, |name| {
            jiff::tz::TimeZone::get(name.as_str())
                .unwrap_or_else(|error| panic!("timezone '{name}' was validated: {error}"))
        })
}

/// Whether `now` falls on December 31 or January 1 in `zone`.
#[must_use]
pub fn is_new_year_day(now: jiff::Timestamp, zone: &jiff::tz::TimeZone) -> bool {
    let date = now.to_zoned(zone.clone()).date();
    matches!((date.month(), date.day()), (12, 31) | (1, 1))
}

/// Greeting and fireworks state of the app.
#[derive(Debug, Default)]
pub struct NewYearState {
    greeting: bool,
    checked: Option<Instant>,
    fireworks: Option<Fireworks>,
}

impl NewYearState {
    /// Whether the greeting line is shown.
    #[must_use]
    pub const fn greeting(&self) -> bool {
        self.greeting
    }

    /// The fireworks in progress, if any.
    #[must_use]
    pub const fn fireworks(&self) -> Option<&Fireworks> {
        self.fireworks.as_ref()
    }

    /// Whether the animation is running (the loop redraws every tick).
    #[must_use]
    pub const fn animating(&self) -> bool {
        self.fireworks.is_some()
    }

    /// Decide the greeting from `now`. Ending the greeting ends a running
    /// animation with it.
    pub fn set_date(&mut self, now: jiff::Timestamp, zone: &jiff::tz::TimeZone) {
        self.greeting = is_new_year_day(now, zone);
        if !self.greeting {
            self.fireworks = None;
        }
    }

    /// Whether the date check is due at `at`; records the check.
    pub fn check_due(&mut self, at: Instant) -> bool {
        let due = self
            .checked
            .is_none_or(|last| at.duration_since(last) >= RECHECK);
        if due {
            self.checked = Some(at);
        }
        due
    }

    /// Start the fireworks; only while the greeting shows.
    pub const fn start(&mut self) {
        if self.greeting {
            self.fireworks = Some(Fireworks::new());
        }
    }

    /// End the fireworks at once.
    pub const fn stop(&mut self) {
        self.fireworks = None;
    }

    /// Advance the animation one frame; it ends by itself.
    pub const fn tick(&mut self) {
        if let Some(fireworks) = &mut self.fireworks
            && fireworks.advance()
        {
            self.fireworks = None;
        }
    }
}

/// One burst: when its rocket launches, where it bursts (percent of the
/// area) and its colour.
struct Burst {
    launch: u16,
    x: u16,
    y: u16,
    color: Color,
}

const BURSTS: [Burst; 6] = [
    Burst {
        launch: 0,
        x: 30,
        y: 35,
        color: Color::Yellow,
    },
    Burst {
        launch: 8,
        x: 70,
        y: 30,
        color: Color::LightRed,
    },
    Burst {
        launch: 16,
        x: 50,
        y: 45,
        color: Color::LightCyan,
    },
    Burst {
        launch: 24,
        x: 20,
        y: 55,
        color: Color::LightMagenta,
    },
    Burst {
        launch: 30,
        x: 80,
        y: 50,
        color: Color::LightGreen,
    },
    Burst {
        launch: 36,
        x: 45,
        y: 25,
        color: Color::LightBlue,
    },
];

/// Frames a rocket rises before it bursts.
const RISE: u16 = 12;
/// Frames a burst's sparks live.
const BURST_LIFE: u16 = 24;
const SPARKS: u16 = 16;

/// Character fireworks, a function of the frame number.
#[derive(Debug)]
pub struct Fireworks {
    frame: u16,
}

impl Fireworks {
    /// Frames until the animation ends: about 3.5 s at the 50 ms tick.
    pub const FRAMES: u16 = 70;

    const fn new() -> Self {
        Self { frame: 0 }
    }

    /// Advance one frame; `true` once the animation is over.
    const fn advance(&mut self) -> bool {
        self.frame += 1;
        self.frame >= Self::FRAMES
    }

    /// Paint the current frame over `area`.
    pub fn paint(&self, buf: &mut Buffer, area: Rect) {
        if area.is_empty() {
            return;
        }
        for burst in &BURSTS {
            let Some(age) = self.frame.checked_sub(burst.launch) else {
                continue;
            };
            let cx = i32::from(area.x) + i32::from(area.width) * i32::from(burst.x) / 100;
            let top = i32::from(area.y) + i32::from(area.height) * i32::from(burst.y) / 100;
            if age < RISE {
                // The rocket climbs from the bottom row to the burst point.
                let bottom = i32::from(area.y) + i32::from(area.height) - 1;
                let y = bottom - (bottom - top) * i32::from(age) / i32::from(RISE);
                put(buf, area, cx, y, '|', burst.color);
            } else if age < RISE + BURST_LIFE {
                paint_sparks(buf, area, burst, age - RISE, cx, top);
            }
        }
    }
}

/// The sparks of a burst `age` frames after it went off.
fn paint_sparks(buf: &mut Buffer, area: Rect, burst: &Burst, age: u16, cx: i32, cy: i32) {
    let glyph = match age {
        0..=7 => '*',
        8..=15 => '+',
        _ => '.',
    };
    let t = f32::from(age);
    let radius = t.mul_add(0.55, 1.0);
    // Sparks sag under gravity as they age.
    let sag = t * t * 0.012;
    for spark in 0..SPARKS {
        let angle = std::f32::consts::TAU * f32::from(spark) / f32::from(SPARKS);
        let (sin, cos) = angle.sin_cos();
        // Cells are about twice as tall as wide.
        let dx = cos * radius * 2.0;
        let dy = sin.mul_add(radius, sag);
        put(
            buf,
            area,
            cx + round(dx),
            cy + round(dy),
            glyph,
            burst.color,
        );
    }
}

#[allow(clippy::cast_possible_truncation)]
const fn round(value: f32) -> i32 {
    value.round() as i32
}

/// Write `glyph` at `(x, y)` when the cell lies inside `area`.
fn put(buf: &mut Buffer, area: Rect, x: i32, y: i32, glyph: char, color: Color) {
    let (Ok(x), Ok(y)) = (u16::try_from(x), u16::try_from(y)) else {
        return;
    };
    if x < area.x || y < area.y || x >= area.x + area.width || y >= area.y + area.height {
        return;
    }
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_char(glyph).set_fg(color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(date: &str, zone: &str) -> (jiff::Timestamp, jiff::tz::TimeZone) {
        let zone = jiff::tz::TimeZone::get(zone).expect("zone");
        let ts = date
            .parse::<jiff::civil::DateTime>()
            .expect("datetime")
            .to_zoned(zone.clone())
            .expect("zoned")
            .timestamp();
        (ts, zone)
    }

    #[test]
    fn greeting_only_on_december_31_and_january_1_in_two_zones() {
        for zone in ["America/New_York", "Asia/Tokyo"] {
            for (date, shown) in [
                ("2026-12-30T12:00:00", false),
                ("2026-12-31T00:00:00", true),
                ("2026-12-31T23:59:59", true),
                ("2027-01-01T00:00:00", true),
                ("2027-01-01T23:59:59", true),
                ("2027-01-02T00:00:00", false),
            ] {
                let (now, zone_tz) = at(date, zone);
                assert_eq!(is_new_year_day(now, &zone_tz), shown, "{date} in {zone}");
            }
        }
    }

    #[test]
    fn the_boundary_follows_the_users_zone_not_utc() {
        // 2027-01-01T23:00 in New York is already January 2 in Tokyo.
        let (now, new_york) = at("2027-01-01T23:00:00", "America/New_York");
        let tokyo = jiff::tz::TimeZone::get("Asia/Tokyo").expect("zone");
        assert!(is_new_year_day(now, &new_york));
        assert!(!is_new_year_day(now, &tokyo));
    }

    #[test]
    fn fireworks_start_only_with_the_greeting_and_end_by_themselves() {
        let mut state = NewYearState::default();
        state.start();
        assert!(!state.animating());

        let (now, zone) = at("2026-12-31T12:00:00", "Asia/Tokyo");
        state.set_date(now, &zone);
        state.start();
        assert!(state.animating());
        for _ in 0..Fireworks::FRAMES {
            state.tick();
        }
        assert!(!state.animating());
        assert!(state.greeting());
    }

    #[test]
    fn the_greeting_ending_ends_the_fireworks() {
        let mut state = NewYearState::default();
        let (now, zone) = at("2027-01-01T12:00:00", "Asia/Tokyo");
        state.set_date(now, &zone);
        state.start();
        let (later, zone) = at("2027-01-02T12:00:00", "Asia/Tokyo");
        state.set_date(later, &zone);
        assert!(!state.greeting() && !state.animating());
    }

    #[test]
    fn painting_stays_inside_the_area() {
        let area = Rect::new(2, 1, 40, 12);
        let mut buf = Buffer::empty(Rect::new(0, 0, 50, 16));
        let blank = buf.clone();
        let mut fireworks = Fireworks::new();
        let mut drew = false;
        for _ in 0..Fireworks::FRAMES {
            fireworks.paint(&mut buf, area);
            drew |= buf != blank;
            fireworks.advance();
        }
        assert!(drew);
        for y in 0..16 {
            for x in 0..50 {
                let inside = (2..42).contains(&x) && (1..13).contains(&y);
                if !inside {
                    assert_eq!(buf[(x, y)], blank[(x, y)], "({x},{y}) outside the area");
                }
            }
        }
    }
}
