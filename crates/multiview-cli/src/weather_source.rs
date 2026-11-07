//! The BOM weather synthetic source: a seven-day outlook or today's detail
//! panel rendered in-process from the management-plane weather cache.
//!
//! * **Data**: the control plane's poller publishes normalized per-location
//!   snapshots into [`WeatherStore::global`]. The source's generator thread
//!   reads it with a non-blocking `try_get` and keys its cached frame on the
//!   location's revision, so it renders **only when the data (or template)
//!   changes** and otherwise republishes the same immutable `Arc` frame each
//!   tick at no cost. No fetch, parse, font or disk work ever runs on the
//!   output clock.
//! * **Look**: a dark gradient (or a transparent background whose translucent
//!   panels carry a real NV12 alpha plane, for layering over a looped media
//!   cell), rounded translucent panels, large numerals in the bundled
//!   overlay fonts, and vector icons built from overlay primitives.
//! * **Attribution**: every frame carries `Source: Bureau of Meteorology`.
//!   Values are drawn exactly as published; an absent value is omitted, never
//!   invented.

#[cfg(any(test, not(feature = "overlay")))]
use multiview_config::{WeatherBackground, WeatherTemplate};
use multiview_weather::{LocationWeather, WeatherStore};

/// The attribution BOM requires on every graphic.
pub const ATTRIBUTION: &str = "Source: Bureau of Meteorology";

/// Spawn the management-plane weather poller for `config` (on the current
/// tokio runtime) and return its handle. The poller publishes into the
/// process-global store the weather source workers read.
#[must_use]
pub fn spawn_weather_service(
    config: &multiview_config::MultiviewConfig,
) -> multiview_weather::service::WeatherService {
    let service = multiview_weather::service::WeatherService::new(std::sync::Arc::clone(
        WeatherStore::global(),
    ));
    service.set_locations(multiview_control::routes::weather::location_specs(
        &config.weather,
    ));
    // The task lives for the process (the control plane lives as long).
    let _task = service.spawn(
        multiview_weather::service::BomFtpFetcher::default(),
        std::future::pending(),
    );
    service
}

/// The cache key for a weather source: the location's published revision
/// (`-1` while it has no snapshot). `None` when the store is momentarily
/// locked by the publisher — the caller then keeps its cached frame.
#[must_use]
pub fn render_key(location: &str) -> Option<i64> {
    match WeatherStore::global().try_get(location) {
        Ok(Some(p)) => Some(i64::try_from(p.revision).unwrap_or(i64::MAX)),
        Ok(None) => Some(-1),
        Err(()) => None,
    }
}

/// The latest snapshot for `location` (non-blocking; `None` when absent or
/// momentarily locked).
#[must_use]
pub fn snapshot(location: &str) -> Option<std::sync::Arc<LocationWeather>> {
    WeatherStore::global()
        .try_get(location)
        .ok()
        .flatten()
        .map(|p| p.data)
}

#[cfg(feature = "overlay")]
pub use render::render_weather;

/// Without the `overlay` feature the weather graphic cannot be drawn.
///
/// # Errors
/// Always [`crate::synth::SynthError::OverlayRequired`].
#[cfg(not(feature = "overlay"))]
pub fn render_weather(
    _data: Option<&LocationWeather>,
    _template: WeatherTemplate,
    _background: WeatherBackground,
    _width: u32,
    _height: u32,
    _canvas: multiview_compositor::pipeline::CanvasColor,
) -> Result<multiview_compositor::pipeline::Nv12Image, crate::synth::SynthError> {
    Err(crate::synth::SynthError::OverlayRequired)
}

#[cfg(feature = "overlay")]
// reason: this module is 2-D drawing code — `x`/`y`/`w`/`h`/`s`(ize)/`c`(tx)/
// `t`(heme) are the conventional geometry names, and each template is one
// linear top-to-bottom layout that reads best unsplit.
#[allow(clippy::many_single_char_names, clippy::too_many_lines)]
mod render {
    use std::sync::Arc;

    use multiview_compositor::blend::PremulRgba;
    use multiview_compositor::overlay::subpass::{
        blend_overlays, LinearCanvasBuffer, OverlayColor, OverlayDrawList, OverlayPrimitive,
        OverlayRect,
    };
    use multiview_compositor::overlay::text::{FontFamily, TextEngine};
    use multiview_compositor::pipeline::{
        canvas_linear_to_output_yuv, AlphaPlanePool, CanvasColor, Nv12Image,
    };
    use multiview_compositor::transfer::srgb_eotf;
    use multiview_config::{WeatherBackground, WeatherTemplate};
    use multiview_weather::{icon_for_code, DayForecast, LocationWeather, WeatherIcon};

    use super::ATTRIBUTION;
    use crate::synth::{f32_of, push_run_offset, round_to_i32, SynthError};

    type R<T> = Result<T, SynthError>;

    #[allow(clippy::needless_pass_by_value)] // reason: used as `map_err(cc)`.
    fn cc(e: multiview_compositor::Error) -> SynthError {
        SynthError::Compositor(e.to_string())
    }

    /// An sRGB hex colour (`0xRRGGBB`) linearized, with straight alpha.
    fn rgb(hex: u32, a: f32) -> OverlayColor {
        let ch = |shift: u32| {
            srgb_eotf(f32::from(u8::try_from((hex >> shift) & 0xFF).unwrap_or(0)) / 255.0)
        };
        OverlayColor::new(ch(16), ch(8), ch(0), a)
    }

    fn px(v: f32) -> i32 {
        round_to_i32(v)
    }

    fn upx(v: f32) -> u32 {
        u32::try_from(round_to_i32(v).max(0)).unwrap_or(0)
    }

    /// Palette for one background mode.
    #[derive(Clone, Copy)]
    struct Theme {
        panel: OverlayColor,
        panel_strong: OverlayColor,
        text: OverlayColor,
        dim: OverlayColor,
        accent: OverlayColor,
        cool: OverlayColor,
        rain: OverlayColor,
    }

    fn theme(background: WeatherBackground) -> Theme {
        let transparent = matches!(background, WeatherBackground::Transparent);
        Theme {
            panel: if transparent {
                rgb(0x0B_1A2E, 0.62)
            } else {
                rgb(0x3A_6E9E, 0.28)
            },
            panel_strong: if transparent {
                rgb(0x0B_1A2E, 0.78)
            } else {
                rgb(0x4A_86BC, 0.36)
            },
            text: rgb(0xF5_F8FC, 1.0),
            dim: rgb(0xA9_BCD0, 1.0),
            accent: rgb(0xFF_C94A, 1.0),
            cool: rgb(0x8F_C7FF, 1.0),
            rain: rgb(0x5F_B0FF, 1.0),
        }
    }

    /// The drawing context: an engine + draw list + frame size.
    struct Ctx {
        engine: TextEngine,
        list: OverlayDrawList,
        w: f32,
        h: f32,
    }

    #[derive(Clone, Copy)]
    enum Align {
        Left,
        Centre,
        Right,
    }

    impl Ctx {
        fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, color: OverlayColor) {
            self.list.push(OverlayPrimitive::FilledRect {
                rect: OverlayRect::new(px(x), px(y), upx(w), upx(h)),
                corner_radius: upx(radius),
                color,
            });
        }

        fn disc(&mut self, cx: f32, cy: f32, r: f32, color: OverlayColor) {
            self.rect(cx - r, cy - r, r * 2.0, r * 2.0, r, color);
        }

        fn stroke(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, thick: f32, color: OverlayColor) {
            self.list.push(OverlayPrimitive::Stroke {
                x0,
                y0,
                x1,
                y1,
                half_thickness: thick / 2.0,
                color,
            });
        }

        /// Width (advance) of `text` at `size`.
        fn measure(&mut self, text: &str, family: FontFamily, size: f32) -> R<f32> {
            Ok(self
                .engine
                .rasterize_run(text, family, size, [1.0; 4])
                .map_err(cc)?
                .advance_width())
        }

        /// Draw `text` with its line box top at `y`, aligned about `x`.
        #[allow(clippy::too_many_arguments)]
        fn text(
            &mut self,
            text: &str,
            family: FontFamily,
            size: f32,
            color: OverlayColor,
            x: f32,
            y: f32,
            align: Align,
        ) -> R<f32> {
            if text.is_empty() {
                return Ok(0.0);
            }
            let run = self
                .engine
                .rasterize_run(text, family, size, [color.r, color.g, color.b, 1.0])
                .map_err(cc)?;
            let width = run.advance_width();
            let left = match align {
                Align::Left => x,
                Align::Centre => x - width / 2.0,
                Align::Right => x - width,
            };
            push_run_offset(&mut self.list, &run, px(left), px(y), color);
            Ok(width)
        }

        /// Greedy word wrap into at most `max_lines` lines of `max_w`.
        fn wrap(
            &mut self,
            text: &str,
            family: FontFamily,
            size: f32,
            max_w: f32,
            max_lines: usize,
        ) -> R<Vec<String>> {
            let mut lines: Vec<String> = Vec::new();
            let mut current = String::new();
            for word in text.split_whitespace() {
                let candidate = if current.is_empty() {
                    word.to_owned()
                } else {
                    format!("{current} {word}")
                };
                if current.is_empty() || self.measure(&candidate, family, size)? <= max_w {
                    current = candidate;
                } else {
                    lines.push(std::mem::take(&mut current));
                    word.clone_into(&mut current);
                }
            }
            if !current.is_empty() {
                lines.push(current);
            }
            if lines.len() > max_lines {
                lines.truncate(max_lines);
                if let Some(last) = lines.last_mut() {
                    last.push('…');
                }
            }
            Ok(lines)
        }
    }

    // ---------------------------------------------------------------- icons

    fn sun(c: &mut Ctx, cx: f32, cy: f32, s: f32, t: &Theme) {
        let r = s * 0.2;
        for i in 0..8_u8 {
            let a = f32::from(i) * std::f32::consts::FRAC_PI_4;
            let (sn, cs) = a.sin_cos();
            c.stroke(
                cx + cs * r * 1.45,
                cy + sn * r * 1.45,
                cx + cs * r * 2.05,
                cy + sn * r * 2.05,
                (s * 0.045).max(1.5),
                t.accent,
            );
        }
        c.disc(cx, cy, r, t.accent);
    }

    fn cloud(c: &mut Ctx, cx: f32, cy: f32, s: f32, color: OverlayColor) {
        // Three overlapping opaque puffs on a rounded base.
        c.rect(cx - s * 0.36, cy, s * 0.72, s * 0.22, s * 0.11, color);
        c.disc(cx - s * 0.18, cy + s * 0.02, s * 0.15, color);
        c.disc(cx + s * 0.05, cy - s * 0.06, s * 0.2, color);
        c.disc(cx + s * 0.24, cy + s * 0.05, s * 0.12, color);
    }

    fn drops(c: &mut Ctx, cx: f32, top: f32, s: f32, n: u8, short: bool, t: &Theme) {
        let len = if short { s * 0.09 } else { s * 0.16 };
        for i in 0..n {
            let x = cx - s * 0.2 + f32::from(i) * s * (0.4 / f32::from(n.max(2) - 1));
            c.stroke(x, top, x - s * 0.05, top + len, (s * 0.05).max(1.5), t.rain);
            if short {
                c.stroke(
                    x - s * 0.07,
                    top + len * 1.8,
                    x - s * 0.1,
                    top + len * 2.5,
                    (s * 0.05).max(1.5),
                    t.rain,
                );
            }
        }
    }

    /// Draw the icon for `code` centred in a square of side `s` at `(cx, cy)`.
    fn icon(c: &mut Ctx, icon: WeatherIcon, cx: f32, cy: f32, s: f32, t: &Theme) {
        let light = rgb(0xE6_EDF5, 1.0);
        let grey = rgb(0xB8_C4D2, 1.0);
        let dark = rgb(0x7D_8A9C, 1.0);
        match icon {
            WeatherIcon::Sun => sun(c, cx, cy, s, t),
            WeatherIcon::Night => {
                c.disc(cx, cy, s * 0.24, rgb(0xF2_EBC9, 1.0));
                c.disc(cx - s * 0.08, cy - s * 0.06, s * 0.05, rgb(0xD9_D1AE, 1.0));
                c.disc(cx + s * 0.07, cy + s * 0.08, s * 0.035, rgb(0xD9_D1AE, 1.0));
                c.disc(cx + s * 0.33, cy - s * 0.3, s * 0.025, light);
                c.disc(cx - s * 0.34, cy + s * 0.28, s * 0.02, light);
            }
            WeatherIcon::PartCloud => {
                sun(c, cx + s * 0.12, cy - s * 0.12, s * 0.75, t);
                cloud(c, cx - s * 0.05, cy + s * 0.08, s * 0.85, light);
            }
            WeatherIcon::Cloud => {
                cloud(c, cx + s * 0.12, cy - s * 0.1, s * 0.6, grey);
                cloud(c, cx - s * 0.04, cy + s * 0.04, s * 0.95, light);
            }
            WeatherIcon::Rain => {
                cloud(c, cx, cy - s * 0.12, s * 0.95, grey);
                drops(c, cx, cy + s * 0.18, s, 4, false, t);
            }
            WeatherIcon::Showers => {
                sun(c, cx + s * 0.16, cy - s * 0.2, s * 0.6, t);
                cloud(c, cx - s * 0.03, cy - s * 0.06, s * 0.85, light);
                drops(c, cx - s * 0.03, cy + s * 0.2, s * 0.85, 3, true, t);
            }
            WeatherIcon::Storm => {
                cloud(c, cx, cy - s * 0.14, s * 0.95, dark);
                let th = (s * 0.06).max(2.0);
                c.stroke(
                    cx + s * 0.04,
                    cy + s * 0.06,
                    cx - s * 0.08,
                    cy + s * 0.24,
                    th,
                    t.accent,
                );
                c.stroke(
                    cx - s * 0.08,
                    cy + s * 0.24,
                    cx + s * 0.06,
                    cy + s * 0.24,
                    th,
                    t.accent,
                );
                c.stroke(
                    cx + s * 0.06,
                    cy + s * 0.24,
                    cx - s * 0.06,
                    cy + s * 0.42,
                    th,
                    t.accent,
                );
            }
            WeatherIcon::Fog => {
                let th = s * 0.075;
                for (i, (dx, wf)) in [(0.0, 0.7), (-0.06, 0.55), (0.05, 0.65), (-0.02, 0.45)]
                    .into_iter()
                    .enumerate()
                {
                    let y = cy - s * 0.24 + f32::from(u8::try_from(i).unwrap_or(0)) * s * 0.16;
                    c.rect(cx - s * wf / 2.0 + s * dx, y, s * wf, th, th / 2.0, grey);
                }
            }
            WeatherIcon::Wind => {
                let th = (s * 0.055).max(1.5);
                for (dy, len) in [(-0.18_f32, 0.62_f32), (0.0, 0.78), (0.18, 0.5)] {
                    let y = cy + s * dy;
                    let x0 = cx - s * 0.38;
                    let x1 = x0 + s * len;
                    c.stroke(x0, y, x1, y, th, light);
                    // A curled tip.
                    c.stroke(x1, y, x1 + s * 0.06, y - s * 0.07, th, light);
                    c.stroke(
                        x1 + s * 0.06,
                        y - s * 0.07,
                        x1 - s * 0.01,
                        y - s * 0.12,
                        th,
                        light,
                    );
                }
            }
        }
    }

    // ------------------------------------------------------------- helpers

    fn temp(v: Option<f64>) -> Option<String> {
        v.map(|t| {
            if (t - t.round()).abs() < 1e-6 {
                format!("{t:.0}°")
            } else {
                format!("{t:.1}°")
            }
        })
    }

    fn background(c: &mut Ctx, bg: WeatherBackground) {
        if matches!(bg, WeatherBackground::Transparent) {
            return;
        }
        // A vertical deep-navy → teal-blue gradient in 48 opaque bands.
        let top = (0x07_u32, 0x12_u32, 0x24_u32);
        let bottom = (0x0F_u32, 0x3A_u32, 0x5F_u32);
        let bands = 48_u16;
        let band_h = c.h / f32::from(bands);
        for i in 0..bands {
            let f = f32::from(i) / f32::from(bands - 1);
            let mix = |a: u32, b: u32| {
                let a = f32::from(u8::try_from(a).unwrap_or(0));
                let b = f32::from(u8::try_from(b).unwrap_or(0));
                u32::try_from(round_to_i32(a + (b - a) * f).clamp(0, 255)).unwrap_or(0)
            };
            let hex =
                (mix(top.0, bottom.0) << 16) | (mix(top.1, bottom.1) << 8) | mix(top.2, bottom.2);
            let y = f32::from(i) * band_h;
            c.rect(0.0, y, c.w, band_h + 1.0, 0.0, rgb(hex, 1.0));
        }
        // A soft highlight glow in the upper right: concentric low-alpha discs.
        for k in 0..24_u8 {
            let r = c.h * (0.64 - f32::from(k) * 0.022);
            c.disc(c.w * 0.88, c.h * 0.02, r, rgb(0x3B_7FB5, 0.016));
        }
    }

    fn header(c: &mut Ctx, t: &Theme, title: &str, subtitle: &str, issued: Option<&str>) -> R<()> {
        let m = c.h * 0.055;
        c.text(
            title,
            FontFamily::Sans,
            c.h * 0.078,
            t.text,
            m,
            m * 0.6,
            Align::Left,
        )?;
        c.text(
            subtitle,
            FontFamily::Sans,
            c.h * 0.034,
            t.dim,
            m,
            m * 0.6 + c.h * 0.095,
            Align::Left,
        )?;
        if let Some(issued) = issued {
            c.text(
                &format!("Issued {issued}"),
                FontFamily::Sans,
                c.h * 0.03,
                t.dim,
                c.w - m,
                m * 0.6 + c.h * 0.02,
                Align::Right,
            )?;
        }
        Ok(())
    }

    fn footer(c: &mut Ctx, t: &Theme, product: Option<&str>) -> R<()> {
        let m = c.h * 0.055;
        let size = c.h * 0.03;
        let y = c.h - m - size * 0.5;
        let width = c.measure(ATTRIBUTION, FontFamily::Sans, size)?;
        c.rect(
            m * 0.7,
            y - size * 0.35,
            width + m * 0.6,
            size * 1.7,
            size * 0.5,
            t.panel_strong,
        );
        c.text(
            ATTRIBUTION,
            FontFamily::Sans,
            size,
            t.text,
            m,
            y,
            Align::Left,
        )?;
        if let Some(product) = product {
            c.text(
                product,
                FontFamily::Mono,
                size * 0.8,
                t.dim,
                c.w - m,
                y + size * 0.1,
                Align::Right,
            )?;
        }
        Ok(())
    }

    fn unavailable(c: &mut Ctx, t: &Theme, data: Option<&LocationWeather>) -> R<()> {
        let title = data.map_or("Weather", |d| d.name.as_str());
        header(c, t, title, "Forecast unavailable", None)?;
        let (pw, ph) = (c.w * 0.6, c.h * 0.32);
        c.rect(
            (c.w - pw) / 2.0,
            (c.h - ph) / 2.0,
            pw,
            ph,
            c.h * 0.03,
            t.panel,
        );
        let reason = data
            .and_then(|d| d.forecast_freshness.error.clone())
            .unwrap_or_else(|| "Waiting for Bureau of Meteorology data".to_owned());
        c.text(
            "No forecast data",
            FontFamily::Sans,
            c.h * 0.05,
            t.text,
            c.w / 2.0,
            c.h * 0.42,
            Align::Centre,
        )?;
        let lines = c.wrap(&reason, FontFamily::Sans, c.h * 0.028, pw * 0.9, 2)?;
        for (i, line) in lines.iter().enumerate() {
            let y = c.h * 0.52 + f32::from(u8::try_from(i).unwrap_or(0)) * c.h * 0.04;
            c.text(
                line,
                FontFamily::Sans,
                c.h * 0.028,
                t.dim,
                c.w / 2.0,
                y,
                Align::Centre,
            )?;
        }
        footer(c, t, None)
    }

    // ----------------------------------------------------------- templates

    fn seven_day(c: &mut Ctx, t: &Theme, data: &LocationWeather) -> R<()> {
        let Some(f) = data.forecast.as_ref() else {
            return unavailable(c, t, Some(data));
        };
        header(
            c,
            t,
            &data.name,
            "7-day forecast",
            f.issued_local.as_deref(),
        )?;
        let m = c.h * 0.055;
        let top = c.h * 0.235;
        let bottom = c.h * 0.84;
        let cols = 7_u8;
        let gap = c.w * 0.012;
        let col_w = (c.w - 2.0 * m - gap * f32::from(cols - 1)) / f32::from(cols);
        let col_h = bottom - top;
        for (i, day) in f.days.iter().take(usize::from(cols)).enumerate() {
            let x = m + f32::from(u8::try_from(i).unwrap_or(0)) * (col_w + gap);
            let fill = if i == 0 { t.panel_strong } else { t.panel };
            c.rect(x, top, col_w, col_h, c.h * 0.025, fill);
            column(c, t, day, i == 0, x, top, col_w, col_h)?;
        }
        footer(c, t, Some(&f.product))
    }

    #[allow(clippy::too_many_arguments)]
    fn column(
        c: &mut Ctx,
        t: &Theme,
        day: &DayForecast,
        today: bool,
        x: f32,
        top: f32,
        w: f32,
        h: f32,
    ) -> R<()> {
        let cx = x + w / 2.0;
        let label = if today { "Today" } else { day.weekday.as_str() };
        c.text(
            label,
            FontFamily::Sans,
            c.h * 0.04,
            t.text,
            cx,
            top + h * 0.04,
            Align::Centre,
        )?;
        let s = (w * 0.62).min(h * 0.24);
        if let Some(i) = day.icon_code.and_then(icon_for_code) {
            icon(c, i, cx, top + h * 0.3, s, t);
        }
        let mut y = top + h * 0.47;
        if let Some(max) = temp(day.max_c) {
            c.text(
                &max,
                FontFamily::Sans,
                c.h * 0.07,
                t.text,
                cx,
                y,
                Align::Centre,
            )?;
        }
        y += c.h * 0.085;
        if let Some(min) = temp(day.min_c) {
            c.text(
                &min,
                FontFamily::Sans,
                c.h * 0.042,
                t.cool,
                cx,
                y,
                Align::Centre,
            )?;
        }
        y += c.h * 0.065;
        if let Some(precis) = day.precis.as_deref() {
            let size = c.h * 0.024;
            let lines = c.wrap(
                precis.trim_end_matches('.'),
                FontFamily::Sans,
                size,
                w * 0.88,
                2,
            )?;
            for line in &lines {
                c.text(line, FontFamily::Sans, size, t.dim, cx, y, Align::Centre)?;
                y += size * 1.25;
            }
        }
        if let Some(chance) = day.rain_chance.as_deref() {
            let size = c.h * 0.026;
            let ry = top + h - size * 1.9;
            let cw = c.measure(chance, FontFamily::Sans, size)?;
            c.disc(
                cx - cw / 2.0 - size * 0.55,
                ry + size * 0.62,
                size * 0.22,
                t.rain,
            );
            c.text(
                chance,
                FontFamily::Sans,
                size,
                t.rain,
                cx,
                ry,
                Align::Centre,
            )?;
        }
        Ok(())
    }

    fn today(c: &mut Ctx, t: &Theme, data: &LocationWeather) -> R<()> {
        let Some(f) = data.forecast.as_ref() else {
            return unavailable(c, t, Some(data));
        };
        let Some(day) = f.days.first() else {
            return unavailable(c, t, Some(data));
        };
        let subtitle = format!("Today · {}", day.label);
        header(c, t, &data.name, &subtitle, f.issued_local.as_deref())?;
        let m = c.h * 0.055;
        let top = c.h * 0.235;
        let bottom = c.h * 0.84;
        let gap = c.w * 0.02;
        let left_w = (c.w - 2.0 * m - gap) * 0.55;
        let right_w = c.w - 2.0 * m - gap - left_w;
        let ph = bottom - top;

        // Forecast panel.
        c.rect(m, top, left_w, ph, c.h * 0.03, t.panel_strong);
        let s = ph * 0.42;
        if let Some(i) = day.icon_code.and_then(icon_for_code) {
            icon(c, i, m + left_w * 0.22, top + ph * 0.3, s, t);
        }
        let tx = m + left_w * 0.45;
        if let Some(max) = temp(day.max_c) {
            c.text(
                &max,
                FontFamily::Sans,
                c.h * 0.16,
                t.text,
                tx,
                top + ph * 0.06,
                Align::Left,
            )?;
            c.text(
                "max",
                FontFamily::Sans,
                c.h * 0.03,
                t.dim,
                tx,
                top + ph * 0.06 + c.h * 0.17,
                Align::Left,
            )?;
        }
        if let Some(min) = temp(day.min_c) {
            let mx = tx + left_w * 0.3;
            c.text(
                &min,
                FontFamily::Sans,
                c.h * 0.08,
                t.cool,
                mx,
                top + ph * 0.12,
                Align::Left,
            )?;
            c.text(
                "min",
                FontFamily::Sans,
                c.h * 0.03,
                t.dim,
                mx,
                top + ph * 0.12 + c.h * 0.095,
                Align::Left,
            )?;
        }
        let mut y = top + ph * 0.58;
        if let Some(precis) = day.precis.as_deref() {
            let size = c.h * 0.045;
            for line in c.wrap(precis, FontFamily::Sans, size, left_w * 0.9, 2)? {
                c.text(
                    &line,
                    FontFamily::Sans,
                    size,
                    t.text,
                    m + left_w * 0.05,
                    y,
                    Align::Left,
                )?;
                y += size * 1.25;
            }
        }
        let mut rain = Vec::new();
        if let Some(chance) = day.rain_chance.as_deref() {
            rain.push(format!("Chance of rain {chance}"));
        }
        if let Some(range) = day.rain_range.as_deref() {
            rain.push(range.to_owned());
        }
        if !rain.is_empty() {
            let size = c.h * 0.034;
            c.text(
                &rain.join("  ·  "),
                FontFamily::Sans,
                size,
                t.rain,
                m + left_w * 0.05,
                top + ph - size * 1.9,
                Align::Left,
            )?;
        }

        // Observation panel.
        let ox = m + left_w + gap;
        c.rect(ox, top, right_w, ph, c.h * 0.03, t.panel);
        let pad = right_w * 0.07;
        match data.observation.as_ref() {
            None => {
                let msg = if data.observation_freshness.is_some() {
                    "Latest observation unavailable"
                } else {
                    "No observation station configured"
                };
                c.text(
                    msg,
                    FontFamily::Sans,
                    c.h * 0.03,
                    t.dim,
                    ox + pad,
                    top + pad,
                    Align::Left,
                )?;
            }
            Some(o) => {
                c.text(
                    &format!("Observed {}", o.time_display),
                    FontFamily::Sans,
                    c.h * 0.034,
                    t.text,
                    ox + pad,
                    top + pad * 0.7,
                    Align::Left,
                )?;
                c.text(
                    &o.station_name,
                    FontFamily::Sans,
                    c.h * 0.024,
                    t.dim,
                    ox + pad,
                    top + pad * 0.7 + c.h * 0.05,
                    Align::Left,
                )?;
                let mut rows: Vec<(&str, String)> = Vec::new();
                if let Some(v) = temp(o.air_temp_c) {
                    rows.push(("Temperature", v));
                }
                if let Some(v) = temp(o.apparent_temp_c) {
                    rows.push(("Feels like", v));
                }
                if let Some(v) = o.rel_humidity_pct {
                    rows.push(("Humidity", format!("{v:.0}%")));
                }
                match (o.wind_dir.as_deref(), o.wind_kmh) {
                    (Some(d), Some(s)) => rows.push(("Wind", format!("{d} {s:.0} km/h"))),
                    (Some(d), None) => rows.push(("Wind", d.to_owned())),
                    (None, Some(s)) => rows.push(("Wind", format!("{s:.0} km/h"))),
                    (None, None) => {}
                }
                if let Some(v) = o.pressure_hpa {
                    rows.push(("Pressure", format!("{v:.1} hPa")));
                }
                if let Some(v) = o.rain_since_9am_mm {
                    rows.push(("Rain since 9am", format!("{v:.1} mm")));
                }
                let size = c.h * 0.036;
                let mut y = top + ph * 0.27;
                let step = (ph * 0.68 / 6.0).max(size * 1.3);
                for (label, value) in rows {
                    c.text(
                        label,
                        FontFamily::Sans,
                        size * 0.8,
                        t.dim,
                        ox + pad,
                        y + size * 0.15,
                        Align::Left,
                    )?;
                    c.text(
                        &value,
                        FontFamily::Sans,
                        size,
                        t.text,
                        ox + right_w - pad,
                        y,
                        Align::Right,
                    )?;
                    c.rect(
                        ox + pad,
                        y + step * 0.86,
                        right_w - 2.0 * pad,
                        1.0,
                        0.0,
                        rgb(0xFF_FFFF, 0.08),
                    );
                    y += step;
                }
            }
        }
        footer(c, t, Some(&f.product))
    }

    // ---------------------------------------------------------------- bake

    /// Render one weather frame. Opaque gradient frames carry no alpha plane;
    /// a transparent background yields an NV12 image with a straight alpha
    /// plane (the compositor's NV12+alpha path).
    ///
    /// # Errors
    /// [`SynthError::Compositor`] on a geometry / colour / font failure.
    pub fn render_weather(
        data: Option<&LocationWeather>,
        template: WeatherTemplate,
        bg: WeatherBackground,
        width: u32,
        height: u32,
        canvas: CanvasColor,
    ) -> R<Nv12Image> {
        let mut c = Ctx {
            engine: TextEngine::new().map_err(cc)?,
            list: OverlayDrawList::new(),
            w: f32_of(width),
            h: f32_of(height),
        };
        let t = theme(bg);
        background(&mut c, bg);
        match (data, template) {
            (Some(d), WeatherTemplate::Today) => today(&mut c, &t, d)?,
            (Some(d), _) => seven_day(&mut c, &t, d)?,
            (None, _) => unavailable(&mut c, &t, None)?,
        }
        let mut buffer = LinearCanvasBuffer::transparent(width, height);
        blend_overlays(&mut buffer, &c.list);
        bake(
            &buffer,
            width,
            height,
            canvas,
            matches!(bg, WeatherBackground::Transparent),
        )
    }

    fn bake(
        buffer: &LinearCanvasBuffer,
        width: u32,
        height: u32,
        canvas: CanvasColor,
        with_alpha: bool,
    ) -> R<Nv12Image> {
        let w = usize::try_from(width).unwrap_or(0);
        let h = usize::try_from(height).unwrap_or(0);
        let mut y_plane = vec![0_u8; w.saturating_mul(h)];
        let mut uv_plane = vec![0_u8; w.saturating_mul(h) / 2];
        let mut alpha = if with_alpha {
            vec![0_u8; w.saturating_mul(h)]
        } else {
            Vec::new()
        };
        let pixel = |x: u32, y: u32| buffer.pixel(x, y).unwrap_or(PremulRgba::TRANSPARENT);
        let mut idx = 0_usize;
        for y in 0..height {
            for x in 0..width {
                let p = pixel(x, y);
                let s = p.unpremultiplied();
                let yuv = canvas_linear_to_output_yuv([s.r, s.g, s.b], canvas).map_err(cc)?;
                if let Some(slot) = y_plane.get_mut(idx) {
                    *slot = yuv[0];
                }
                if with_alpha {
                    if let Some(slot) = alpha.get_mut(idx) {
                        *slot =
                            u8::try_from(round_to_i32(p.a.clamp(0.0, 1.0) * 255.0).clamp(0, 255))
                                .unwrap_or(0);
                    }
                }
                idx += 1;
            }
        }
        // Chroma: average the 2×2 block in premultiplied linear light.
        for cy in 0..height / 2 {
            for cxi in 0..width / 2 {
                let mut acc = [0.0_f32; 4];
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let p = pixel(cxi * 2 + dx, cy * 2 + dy);
                    acc[0] += p.r;
                    acc[1] += p.g;
                    acc[2] += p.b;
                    acc[3] += p.a;
                }
                let avg = PremulRgba {
                    r: acc[0] / 4.0,
                    g: acc[1] / 4.0,
                    b: acc[2] / 4.0,
                    a: acc[3] / 4.0,
                };
                let s = avg.unpremultiplied();
                let yuv = canvas_linear_to_output_yuv([s.r, s.g, s.b], canvas).map_err(cc)?;
                let i =
                    usize::try_from(cy).unwrap_or(0) * w + usize::try_from(cxi).unwrap_or(0) * 2;
                if let Some(slot) = uv_plane.get_mut(i) {
                    *slot = yuv[1];
                }
                if let Some(slot) = uv_plane.get_mut(i + 1) {
                    *slot = yuv[2];
                }
            }
        }
        let image =
            Nv12Image::new(width, height, y_plane, uv_plane, canvas.output_tag()).map_err(cc)?;
        if with_alpha {
            Arc::new(AlphaPlanePool::default())
                .attach(image, &alpha)
                .map_err(cc)
        } else {
            Ok(image)
        }
    }

    #[cfg(test)]
    pub(super) fn attribution_box(width: u32, height: u32) -> (u32, u32, u32, u32) {
        // Mirrors `footer`: the attribution sits in the bottom-left band.
        let h = f32_of(height);
        let m = h * 0.055;
        let size = h * 0.03;
        let y = h - m - size * 0.5;
        (
            upx(m * 0.7),
            upx(y - size * 0.35),
            upx(f32_of(width) * 0.45),
            upx(size * 1.7),
        )
    }
}

#[cfg(all(test, feature = "overlay"))]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::similar_names
)]
mod tests {
    use super::*;
    use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
    use multiview_weather::{DayForecast, ForecastSummary, Freshness, Observation};

    fn canvas() -> CanvasColor {
        CanvasColor::default()
    }

    fn fixture() -> LocationWeather {
        let t = |s: &str| {
            multiview_weather::chrono::DateTime::parse_from_rfc3339(s)
                .expect("time")
                .with_timezone(&multiview_weather::chrono::Utc)
        };
        let codes = [1_u8, 16, 11, 3, 1, 3, 11];
        let days = (0..7_u8)
            .map(|i| DayForecast {
                start_local: format!("2026-11-{:02}T00:00:00+11:00", 6 + i),
                date: format!("2026-11-{:02}", 6 + i),
                weekday: ["Fri", "Sat", "Sun", "Mon", "Tue", "Wed", "Thu"][usize::from(i)]
                    .to_owned(),
                label: format!(
                    "{} {} Nov",
                    ["Fri", "Sat", "Sun", "Mon", "Tue", "Wed", "Thu"][usize::from(i)],
                    6 + i
                ),
                icon_code: Some(codes[usize::from(i)]),
                max_c: Some(20.0 + f64::from(i)),
                min_c: (i > 0).then(|| 10.0 + f64::from(i)),
                precis: Some("Shower or two.".to_owned()),
                rain_chance: Some("50%".to_owned()),
                rain_range: (i == 1).then(|| "0 to 1 mm".to_owned()),
            })
            .collect();
        LocationWeather {
            location_id: "sydney".into(),
            name: "Sydney".into(),
            forecast: Some(ForecastSummary {
                product: "IDN11060".into(),
                aac: "NSW_PT131".into(),
                area_name: "Sydney".into(),
                issued_utc: t("2026-11-05T22:45:31Z"),
                issued_local: Some("9:45 am Fri 6 Nov".into()),
                expires_utc: None,
                days,
            }),
            forecast_freshness: Freshness::default(),
            observation: Some(Observation {
                product: "IDN60920".into(),
                wmo_id: "94768".into(),
                station_name: "SYDNEY (OBSERVATORY HILL)".into(),
                time_utc: t("2026-11-05T23:50:00Z"),
                time_local: "2026-11-06T10:50:00+11:00".into(),
                time_display: "10:50 am".into(),
                air_temp_c: Some(22.6),
                apparent_temp_c: Some(21.2),
                rel_humidity_pct: Some(56.0),
                wind_dir: Some("ENE".into()),
                wind_kmh: Some(13.0),
                gust_kmh: None,
                pressure_hpa: Some(1018.0),
                rain_since_9am_mm: Some(0.0),
            }),
            observation_freshness: Some(Freshness::default()),
        }
    }

    fn hash(image: &Nv12Image) -> u64 {
        // FNV-1a over both planes (+ alpha) — stable across runs.
        let mut h = 0xcbf2_9ce4_8422_2325_u64;
        for b in image
            .y_plane()
            .iter()
            .chain(image.uv_plane())
            .chain(image.alpha_plane().unwrap_or_default())
        {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h
    }

    #[test]
    fn render_is_deterministic_for_fixed_data() {
        let data = fixture();
        for template in [WeatherTemplate::SevenDay, WeatherTemplate::Today] {
            let a = render_weather(
                Some(&data),
                template,
                WeatherBackground::Gradient,
                640,
                360,
                canvas(),
            )
            .expect("render");
            let b = render_weather(
                Some(&data),
                template,
                WeatherBackground::Gradient,
                640,
                360,
                canvas(),
            )
            .expect("render");
            assert_eq!(hash(&a), hash(&b));
            assert!(a.alpha_plane().is_none(), "gradient is opaque");
        }
        let seven = render_weather(
            Some(&data),
            WeatherTemplate::SevenDay,
            WeatherBackground::Gradient,
            640,
            360,
            canvas(),
        )
        .expect("r");
        let today = render_weather(
            Some(&data),
            WeatherTemplate::Today,
            WeatherBackground::Gradient,
            640,
            360,
            canvas(),
        )
        .expect("r");
        assert_ne!(hash(&seven), hash(&today), "templates differ");
        let mut changed = data.clone();
        if let Some(f) = changed.forecast.as_mut() {
            f.days[3].max_c = Some(31.0);
        }
        let other = render_weather(
            Some(&changed),
            WeatherTemplate::SevenDay,
            WeatherBackground::Gradient,
            640,
            360,
            canvas(),
        )
        .expect("r");
        assert_ne!(hash(&seven), hash(&other), "data change re-renders");
    }

    #[test]
    fn attribution_region_is_inked() {
        let data = fixture();
        let (w, h) = (640_u32, 360_u32);
        let with = render_weather(
            Some(&data),
            WeatherTemplate::SevenDay,
            WeatherBackground::Gradient,
            w,
            h,
            canvas(),
        )
        .expect("r");
        let (x0, y0, bw, bh) = render::attribution_box(w, h);
        let ys = with.y_plane();
        let width = usize::try_from(w).expect("w");
        let mut lo = u8::MAX;
        let mut hi = 0_u8;
        for y in y0..y0 + bh {
            for x in x0..x0 + bw {
                let v = ys[usize::try_from(y).expect("y") * width + usize::try_from(x).expect("x")];
                lo = lo.min(v);
                hi = hi.max(v);
            }
        }
        assert!(
            hi.saturating_sub(lo) > 80,
            "attribution text inks the footer ({lo}..{hi})"
        );
        // Also present on the unavailable card and the transparent variant.
        let none = render_weather(
            None,
            WeatherTemplate::Today,
            WeatherBackground::Transparent,
            w,
            h,
            canvas(),
        )
        .expect("r");
        let alpha = none.alpha_plane().expect("transparent carries alpha");
        let mut inked = 0_usize;
        for y in y0..y0 + bh {
            for x in x0..x0 + bw {
                if alpha[usize::try_from(y).expect("y") * width + usize::try_from(x).expect("x")]
                    > 200
                {
                    inked += 1;
                }
            }
        }
        assert!(
            inked > 100,
            "attribution panel is opaque-ish on transparent bg"
        );
        assert_eq!(alpha[0], 0, "background corner stays transparent");
    }

    /// Manual check aid: with `WEATHER_PREVIEW_DIR` set and optional
    /// `WEATHER_FORECAST_XML` / `WEATHER_OBS_XML` (real BOM samples kept
    /// outside git), write raw NV12 (+alpha) frames for inspection. A no-op
    /// otherwise.
    #[test]
    fn manual_preview_dump() {
        let Ok(dir) = std::env::var("WEATHER_PREVIEW_DIR") else {
            return;
        };
        let mut data = fixture();
        if let Ok(path) = std::env::var("WEATHER_FORECAST_XML") {
            let f = multiview_weather::parse_forecast_with_limits(
                &std::fs::read(path).expect("read"),
                multiview_weather::Limits {
                    max_bytes: 8 << 20,
                    max_nodes: 400_000,
                    ..Default::default()
                },
            )
            .expect("parse");
            data.forecast = multiview_weather::normalize_forecast(&f, "NSW_PT131");
        }
        if let Ok(path) = std::env::var("WEATHER_OBS_XML") {
            let o = multiview_weather::parse_observations_with_limits(
                &std::fs::read(path).expect("read"),
                multiview_weather::Limits {
                    max_bytes: 8 << 20,
                    max_nodes: 400_000,
                    ..Default::default()
                },
            )
            .expect("parse");
            data.observation = multiview_weather::normalize_observation(&o, "94768");
        }
        for (name, template, bg) in [
            (
                "seven",
                WeatherTemplate::SevenDay,
                WeatherBackground::Gradient,
            ),
            ("today", WeatherTemplate::Today, WeatherBackground::Gradient),
            (
                "seven_alpha",
                WeatherTemplate::SevenDay,
                WeatherBackground::Transparent,
            ),
        ] {
            let img =
                render_weather(Some(&data), template, bg, 1280, 720, canvas()).expect("render");
            let mut out = img.y_plane().to_vec();
            out.extend_from_slice(img.uv_plane());
            std::fs::write(format!("{dir}/{name}.nv12"), out).expect("write");
            if let Some(a) = img.alpha_plane() {
                std::fs::write(format!("{dir}/{name}.alpha"), a).expect("write");
            }
        }
    }

    #[test]
    fn render_key_changes_only_with_the_picture() {
        let id = "render-key-test-loc";
        assert_eq!(render_key(id), Some(-1), "unknown location");
        let store = WeatherStore::global();
        let mut data = fixture();
        data.location_id = id.to_owned();
        let others: Vec<_> = store.all().into_iter().map(|p| (*p.data).clone()).collect();
        let mut all = others.clone();
        all.push(data.clone());
        store.publish_all(all.clone());
        let first = render_key(id).expect("key");
        // A refetch of identical data (only freshness differs) keeps the key.
        if let Some(last) = all.last_mut() {
            last.forecast_freshness.fetched_at = Some(multiview_weather::chrono::Utc::now());
        }
        store.publish_all(all.clone());
        assert_eq!(render_key(id), Some(first));
        // A data change bumps it.
        if let Some(f) = all.last_mut().and_then(|d| d.forecast.as_mut()) {
            f.days[0].max_c = Some(40.0);
        }
        store.publish_all(all);
        assert_ne!(render_key(id), Some(first));
        store.publish_all(others);
    }
}
