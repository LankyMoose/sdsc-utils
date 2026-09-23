//! Battery-driven lightbar colors.

use serde::de::{Deserializer, Error as DeError, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    #[allow(dead_code)] // used by unit tests
    pub const WHITE: Self = Self::new(255, 255, 255);
    pub const ORANGE: Self = Self::new(255, 100, 0);

    pub fn lerp(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        Self {
            r: lerp_u8(self.r, other.r, t),
            g: lerp_u8(self.g, other.g, t),
            b: lerp_u8(self.b, other.b, t),
        }
    }

    pub fn to_hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }

    pub fn to_hsv(self) -> (f32, f32, f32) {
        let r = self.r as f32 / 255.0;
        let g = self.g as f32 / 255.0;
        let b = self.b as f32 / 255.0;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let delta = max - min;

        let h = if delta < f32::EPSILON {
            0.0
        } else if max == r {
            60.0 * (((g - b) / delta) % 6.0)
        } else if max == g {
            60.0 * (((b - r) / delta) + 2.0)
        } else {
            60.0 * (((r - g) / delta) + 4.0)
        };
        let h = if h < 0.0 { h + 360.0 } else { h };
        let s = if max < f32::EPSILON { 0.0 } else { delta / max };
        (h, s, max)
    }
}

/// A battery-percent color stop on the lightbar spectrum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GradientStop {
    pub percent: u8,
    pub color: Rgb,
}

impl GradientStop {
    pub const fn new(percent: u8, color: Rgb) -> Self {
        Self { percent, color }
    }
}

/// Variable-stop spectrum used to map battery percent to lightbar RGB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatterySpectrum {
    pub stops: Vec<GradientStop>,
}

impl Default for BatterySpectrum {
    fn default() -> Self {
        Self::default_spectrum()
    }
}

impl BatterySpectrum {
    pub const MIN_STOPS: usize = 2;
    pub const MAX_STOPS: usize = 5;

    /// Default blue → purple → red stops.
    pub fn default_spectrum() -> Self {
        Self {
            stops: vec![
                GradientStop::new(100, Rgb::new(0x25, 0x25, 0xEB)),
                GradientStop::new(50, Rgb::new(0x89, 0x10, 0xE1)),
                GradientStop::new(0, Rgb::new(0xFF, 0x01, 0x01)),
            ],
        }
    }

    pub fn from_stops(mut stops: Vec<GradientStop>) -> Result<Self, String> {
        normalize_stops(&mut stops)?;
        Ok(Self { stops })
    }

    pub fn color_at_percent(&self, percent: u8) -> Rgb {
        let percent = percent.min(100);
        let stops = &self.stops;
        if stops.is_empty() {
            return BatterySpectrum::default_spectrum().color_at_percent(percent);
        }
        if stops.len() == 1 {
            return stops[0].color;
        }

        if percent >= stops[0].percent {
            return stops[0].color;
        }
        if percent <= stops[stops.len() - 1].percent {
            return stops[stops.len() - 1].color;
        }

        for window in stops.windows(2) {
            let high = window[0];
            let low = window[1];
            if percent <= high.percent && percent >= low.percent {
                let span = (high.percent - low.percent).max(1) as f32;
                let t = (high.percent - percent) as f32 / span;
                return high.color.lerp(low.color, t);
            }
        }

        stops[0].color
    }

    pub fn set_stop_color(&mut self, index: usize, color: Rgb) -> Result<(), String> {
        let stop = self
            .stops
            .get_mut(index)
            .ok_or_else(|| format!("stop index {index} out of range"))?;
        stop.color = color;
        Ok(())
    }

    pub fn set_stop_percent(&mut self, index: usize, percent: u8) -> Result<usize, String> {
        if index >= self.stops.len() {
            return Err(format!("stop index {index} out of range"));
        }
        let color = self.stops[index].color;
        let occupied: Vec<u8> = self
            .stops
            .iter()
            .enumerate()
            .filter_map(|(i, stop)| (i != index).then_some(stop.percent))
            .collect();
        let percent = unique_percent(&occupied, percent.min(100))
            .ok_or_else(|| "unable to place stop at a unique percent".to_string())?;
        self.stops[index].percent = percent;
        normalize_stops(&mut self.stops)?;
        self.stops
            .iter()
            .position(|stop| stop.percent == percent && stop.color == color)
            .ok_or_else(|| "failed to relocate stop after reorder".to_string())
    }

    pub fn add_stop_at(&mut self, percent: u8) -> Result<usize, String> {
        if self.stops.len() >= Self::MAX_STOPS {
            return Err(format!("at most {} stops are allowed", Self::MAX_STOPS));
        }
        let percent = percent.min(100);
        if self.stops.iter().any(|stop| stop.percent == percent) {
            return Err(format!("a stop already exists at {percent}%"));
        }
        let color = self.color_at_percent(percent);
        self.stops.push(GradientStop::new(percent, color));
        normalize_stops(&mut self.stops)?;
        self.stops
            .iter()
            .position(|stop| stop.percent == percent)
            .ok_or_else(|| "failed to locate newly added stop".to_string())
    }

    pub fn remove_stop(&mut self, index: usize) -> Result<usize, String> {
        if self.stops.len() <= Self::MIN_STOPS {
            return Err(format!("at least {} stops are required", Self::MIN_STOPS));
        }
        if index >= self.stops.len() {
            return Err(format!("stop index {index} out of range"));
        }
        self.stops.remove(index);
        Ok(index.min(self.stops.len().saturating_sub(1)))
    }
}

fn unique_percent(occupied: &[u8], percent: u8) -> Option<u8> {
    if !occupied.contains(&percent) {
        return Some(percent);
    }
    for delta in 1..=100u8 {
        let up = percent.saturating_add(delta);
        if up <= 100 && !occupied.contains(&up) {
            return Some(up);
        }
        if percent >= delta {
            let down = percent - delta;
            if !occupied.contains(&down) {
                return Some(down);
            }
        }
    }
    None
}

fn normalize_stops(stops: &mut [GradientStop]) -> Result<(), String> {
    if !(BatterySpectrum::MIN_STOPS..=BatterySpectrum::MAX_STOPS).contains(&stops.len()) {
        return Err(format!(
            "spectrum requires {}-{} stops, got {}",
            BatterySpectrum::MIN_STOPS,
            BatterySpectrum::MAX_STOPS,
            stops.len()
        ));
    }
    for stop in stops.iter_mut() {
        stop.percent = stop.percent.min(100);
    }
    stops.sort_by(|a, b| {
        b.percent
            .cmp(&a.percent)
            .then_with(|| a.color.to_hex().cmp(&b.color.to_hex()))
    });
    for window in stops.windows(2) {
        if window[0].percent == window[1].percent {
            return Err(format!("duplicate stop percent {}%", window[0].percent));
        }
    }
    Ok(())
}

impl<'de> Deserialize<'de> for BatterySpectrum {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(BatterySpectrumVisitor)
    }
}

struct BatterySpectrumVisitor;

impl<'de> Visitor<'de> for BatterySpectrumVisitor {
    type Value = BatterySpectrum;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a battery spectrum with stops or legacy full/mid/empty fields")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut stops = Vec::new();
        while let Some(stop) = seq.next_element::<GradientStop>()? {
            stops.push(stop);
        }
        BatterySpectrum::from_stops(stops).map_err(DeError::custom)
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut stops: Option<Vec<GradientStop>> = None;
        let mut full: Option<Rgb> = None;
        let mut mid: Option<Rgb> = None;
        let mut empty: Option<Rgb> = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "stops" => {
                    if stops.is_some() {
                        return Err(DeError::duplicate_field("stops"));
                    }
                    stops = Some(map.next_value()?);
                }
                "full" => {
                    if full.is_some() {
                        return Err(DeError::duplicate_field("full"));
                    }
                    full = Some(map.next_value()?);
                }
                "mid" => {
                    if mid.is_some() {
                        return Err(DeError::duplicate_field("mid"));
                    }
                    mid = Some(map.next_value()?);
                }
                "empty" => {
                    if empty.is_some() {
                        return Err(DeError::duplicate_field("empty"));
                    }
                    empty = Some(map.next_value()?);
                }
                _ => {
                    let _ = map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
        }

        if let Some(stops) = stops {
            return BatterySpectrum::from_stops(stops).map_err(DeError::custom);
        }

        match (full, mid, empty) {
            (Some(full), Some(mid), Some(empty)) => BatterySpectrum::from_stops(vec![
                GradientStop::new(100, full),
                GradientStop::new(50, mid),
                GradientStop::new(0, empty),
            ])
            .map_err(DeError::custom),
            _ => Err(DeError::custom(
                "spectrum requires either stops or legacy full/mid/empty colors",
            )),
        }
    }
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t)
        .round()
        .clamp(0.0, 255.0) as u8
}

fn active_spectrum() -> &'static Mutex<BatterySpectrum> {
    static SPECTRUM: OnceLock<Mutex<BatterySpectrum>> = OnceLock::new();
    SPECTRUM.get_or_init(|| Mutex::new(BatterySpectrum::default_spectrum()))
}

/// Install the spectrum used by [`color_for_battery_percent`].
pub fn set_active_spectrum(spectrum: BatterySpectrum) {
    if let Ok(mut guard) = active_spectrum().lock() {
        *guard = spectrum;
    }
}

/// Map battery percent through the active RGB spectrum.
pub fn color_for_battery_percent(percent: u8) -> Rgb {
    let spectrum = active_spectrum()
        .lock()
        .map(|g| g.clone())
        .unwrap_or_else(|_| BatterySpectrum::default_spectrum());
    spectrum.color_at_percent(percent)
}

pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> Rgb {
    let s = s.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    let c = v * s;
    let h_prime = (h.rem_euclid(360.0)) / 60.0;
    let x = c * (1.0 - (h_prime % 2.0 - 1.0).abs());
    let m = v - c;

    let (r1, g1, b1) = match h_prime as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };

    Rgb {
        r: ((r1 + m) * 255.0).round() as u8,
        g: ((g1 + m) * 255.0).round() as u8,
        b: ((b1 + m) * 255.0).round() as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_stops_match_expected_hex() {
        let s = BatterySpectrum::default_spectrum();
        assert_eq!(s.color_at_percent(100), Rgb::new(0x25, 0x25, 0xEB));
        assert_eq!(s.color_at_percent(50), Rgb::new(0x89, 0x10, 0xE1));
        assert_eq!(s.color_at_percent(0), Rgb::new(0xFF, 0x01, 0x01));
    }

    #[test]
    fn full_battery_is_blueish() {
        let c = BatterySpectrum::default_spectrum().color_at_percent(100);
        assert!(c.b > c.r && c.b > c.g, "expected blue-dominant, got {c:?}");
    }

    #[test]
    fn mid_battery_is_purpleish() {
        let c = BatterySpectrum::default_spectrum().color_at_percent(50);
        assert!(c.r > 0 && c.b > 0, "expected purple-ish, got {c:?}");
        assert!(c.g < c.r && c.g < c.b, "expected low green, got {c:?}");
    }

    #[test]
    fn empty_battery_is_redish() {
        for percent in [0u8, 5] {
            let c = BatterySpectrum::default_spectrum().color_at_percent(percent);
            assert!(
                c.r > c.g && c.r >= c.b,
                "expected red-dominant at {percent}%, got {c:?}"
            );
        }
    }

    #[test]
    fn lerp_midpoint_between_full_and_mid() {
        let s = BatterySpectrum::default_spectrum();
        let c = s.color_at_percent(75);
        // Halfway from #2525EB to #8910E1
        assert_eq!(c, Rgb::new(87, 27, 230));
    }

    #[test]
    fn active_spectrum_is_used() {
        set_active_spectrum(
            BatterySpectrum::from_stops(vec![
                GradientStop::new(100, Rgb::new(0, 255, 0)),
                GradientStop::new(50, Rgb::new(255, 255, 0)),
                GradientStop::new(0, Rgb::new(255, 0, 0)),
            ])
            .unwrap(),
        );
        assert_eq!(color_for_battery_percent(100), Rgb::new(0, 255, 0));
        set_active_spectrum(BatterySpectrum::default_spectrum());
    }

    #[test]
    fn legacy_prefs_migrate_to_stops() {
        let spectrum: BatterySpectrum = serde_json::from_str(
            r#"{"full":{"r":1,"g":2,"b":3},"mid":{"r":4,"g":5,"b":6},"empty":{"r":7,"g":8,"b":9}}"#,
        )
        .unwrap();
        assert_eq!(
            spectrum.stops,
            vec![
                GradientStop::new(100, Rgb::new(1, 2, 3)),
                GradientStop::new(50, Rgb::new(4, 5, 6)),
                GradientStop::new(0, Rgb::new(7, 8, 9)),
            ]
        );
    }

    #[test]
    fn serializes_stops_not_legacy_fields() {
        let json = serde_json::to_value(BatterySpectrum::default_spectrum()).unwrap();
        assert!(json.get("stops").is_some());
        assert!(json.get("full").is_none());
    }

    #[test]
    fn enforces_stop_limits_and_unique_percents() {
        assert!(BatterySpectrum::from_stops(vec![GradientStop::new(100, Rgb::WHITE)]).is_err());
        let mut spectrum = BatterySpectrum::default_spectrum();
        assert!(spectrum.add_stop_at(75).is_ok());
        assert!(spectrum.add_stop_at(25).is_ok());
        assert!(spectrum.add_stop_at(10).is_err());
        assert!(spectrum.remove_stop(1).is_ok());
        while spectrum.stops.len() > BatterySpectrum::MIN_STOPS {
            spectrum.remove_stop(0).unwrap();
        }
        assert!(spectrum.remove_stop(0).is_err());
    }

    #[test]
    fn adding_stop_preserves_existing_gradient_colors() {
        let mut spectrum = BatterySpectrum::default_spectrum();
        let before = (0..=100)
            .map(|percent| spectrum.color_at_percent(percent))
            .collect::<Vec<_>>();
        let index = spectrum.add_stop_at(75).unwrap();
        assert_eq!(spectrum.stops[index].percent, 75);
        for percent in 0..=100u8 {
            let after = spectrum.color_at_percent(percent);
            let expected = before[percent as usize];
            assert!(
                after.r.abs_diff(expected.r) <= 1
                    && after.g.abs_diff(expected.g) <= 1
                    && after.b.abs_diff(expected.b) <= 1,
                "percent {percent}: got {after:?}, expected near {expected:?}"
            );
        }
    }

    #[test]
    fn reordering_stop_updates_index() {
        let mut spectrum = BatterySpectrum::default_spectrum();
        let mid_color = spectrum.stops[1].color;
        let new_index = spectrum.set_stop_percent(1, 10).unwrap();
        assert_eq!(spectrum.stops[new_index].percent, 10);
        assert_eq!(spectrum.stops[new_index].color, mid_color);
        assert!(
            spectrum
                .stops
                .windows(2)
                .all(|pair| pair[0].percent > pair[1].percent)
        );
    }
}
