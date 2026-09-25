//! SVG arc-path generation for the volume ring.
//!
//! `.slint` has no expression-level number→string conversion, so a *smooth*
//! sweep arc cannot be built inside the theme itself. Instead the helper
//! generates the `d` attribute of an SVG arc and writes it into the theme's
//! `arc-path` string property; the theme binds it to a `Path` element:
//!
//! ```slint
//! Path {
//!     viewbox-width: 100; viewbox-height: 100;      // normalized space
//!     commands: root.arc-path;                       // from the helper
//!     stroke: accent; stroke-width: 5; stroke-line-cap: round;
//!     fill: transparent;
//! }
//! ```
//!
//! Geometry: a 100×100 viewbox, circle center (50, 50), radius 45, sweep
//! starting at 12 o'clock, running clockwise — exactly like the legacy v1
//! `drawArc(startAngle = -90°, sweepAngle = 360° * level)`.
//!
//! Themes that prefer to stay fully declarative can ignore `arc-path` and use
//! the numeric `input-level` / `smooth-level` properties instead (e.g. a
//! segmented tick ring).

use std::f64::consts::PI;

/// Viewbox edge length the arc paths are generated in (see module docs).
pub const VIEWBOX: f64 = 100.0;
pub const CENTER: f64 = VIEWBOX / 2.0;
pub const RADIUS: f64 = 45.0;

/// Build the SVG path data for a clockwise arc from 12 o'clock sweeping
/// `level * 360°`. Returns `""` for (near-)zero levels and a full circle
/// (two half-arcs) for (near-)one levels — a single 360° arc is degenerate
/// in SVG (start == end renders nothing).
pub fn arc_path(level: f64) -> String {
    let l = level.clamp(0.0, 1.0);
    if l <= 0.003 {
        return String::new();
    }
    if l >= 0.997 {
        // Two semicircles: top → bottom → top (clockwise, sweep-flag 1).
        return format!(
            "M {cx} {top} A {r} {r} 0 1 1 {cx} {bottom} A {r} {r} 0 1 1 {cx} {top}",
            cx = fmt(CENTER),
            top = fmt(CENTER - RADIUS),
            bottom = fmt(CENTER + RADIUS),
            r = fmt(RADIUS),
        );
    }
    let sweep = l * 2.0 * PI; // radians, clockwise from -Y axis
    let x = CENTER + RADIUS * sweep.sin();
    let y = CENTER - RADIUS * sweep.cos();
    let large_arc = if sweep > PI { 1 } else { 0 };
    format!(
        "M {cx} {top} A {r} {r} 0 {large_arc} 1 {x} {y}",
        cx = fmt(CENTER),
        top = fmt(CENTER - RADIUS),
        r = fmt(RADIUS),
        large_arc = large_arc,
        x = fmt(x),
        y = fmt(y),
    )
}

/// Compact fixed formatting (2 decimals, strip trailing zeros) — keeps the
/// property strings short since they are rebuilt ~30×/s.
fn fmt(v: f64) -> String {
    let s = format!("{:.2}", v);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewbox_geometry_constants() {
        assert_eq!(VIEWBOX, 100.0);
        assert_eq!(CENTER, 50.0);
    }

    #[test]
    fn empty_and_full() {
        assert_eq!(arc_path(0.0), "");
        assert_eq!(arc_path(0.001), "");
        assert_eq!(arc_path(-0.5), "");
        let full = arc_path(1.0);
        assert_eq!(full.matches(" A ").count(), 2, "full circle = 2 arcs: {full}");
        assert!(full.starts_with("M 50 5 "));
    }

    #[test]
    fn quarter_arc_geometry() {
        let p = arc_path(0.25);
        // sweep = 90° → endpoint (50+45, 50) = (95, 50), small-arc, clockwise
        assert!(p.starts_with("M 50 5 A 45 45 0 0 1 "), "{p}");
        assert!(p.ends_with("95 50"), "{p}");
    }

    #[test]
    fn three_quarter_uses_large_arc_flag() {
        let p = arc_path(0.75);
        assert!(p.contains(" 1 1 "), "{p}"); // large-arc-flag = 1
        // endpoint at 270°: (50-45, 50) = (5,50)
        assert!(p.ends_with("5 50"), "{p}");
    }

    #[test]
    fn half_arc() {
        let p = arc_path(0.5);
        assert!(p.ends_with("50 95"), "{p}"); // bottom of circle
    }

    #[test]
    fn bars_path_eight_segments_at_full_level() {
        let p = bars_path(1.0, 0.0);
        assert_eq!(p.matches("M ").count(), 8, "{p}");
        // first bar points up: (50,30) → (50,~18)
        assert!(p.starts_with("M 50 30 L 50 "), "{p}");
        assert_eq!(bars_path(0.0, 10.0), "");
    }

    #[test]
    fn clamps_out_of_range() {
        assert_eq!(arc_path(2.0), arc_path(1.0));
        assert_eq!(arc_path(-1.0), "");
    }

    #[test]
    fn monotonic_endpoint_moves_clockwise() {
        // x of the endpoint should increase through the first quadrant
        let mut last_x = 50.0;
        for i in 1..=10 {
            let l = i as f64 * 0.02; // 0.02..0.20 stays in quadrant I
            let p = arc_path(l);
            let tail = p.rsplit(' ').take(2).collect::<Vec<_>>();
            let x: f64 = tail[1].parse().unwrap();
            assert!(x >= last_x - 1e-9, "{l}: {x} < {last_x}");
            last_x = x;
        }
    }
}

/// Eight radial "wave bars" as one SVG path (100×100 viewbox), reproducing
/// the rotating tick decoration of the legacy v1 `FloatingAudioVisualizer`.
///
/// The software renderer of Slint 1.18 ignores element transforms
/// (`transform-rotation`/`transform-scale`), so rotated geometry must be
/// expressed as path commands instead of rotated rectangles. The phase
/// advances on the helper side (~30 Hz), which also keeps the animation
/// smooth independent of the theme.
pub fn bars_path(level: f64, phase_deg: f64) -> String {
    let l = level.clamp(0.0, 1.0);
    if l <= 0.02 {
        return String::new();
    }
    let r0 = 20.0; // inner radius (viewbox units)
    let mut out = String::new();
    for i in 0..8u32 {
        let angle = i as f64 * 45.0 + phase_deg;
        let dyn_level = l * (0.3 + 0.7 * ((angle * 0.1 + phase_deg * 0.02).to_radians()).sin());
        let len = 12.0 * dyn_level.clamp(0.0, 1.0);
        if len < 0.8 {
            continue;
        }
        let rad = angle.to_radians();
        let (sx, sy) = (rad.sin(), -rad.cos());
        let x0 = CENTER + r0 * sx;
        let y0 = CENTER + r0 * sy;
        let x1 = CENTER + (r0 + len) * sx;
        let y1 = CENTER + (r0 + len) * sy;
        out.push_str(&format!(
            "M {} {} L {} {} ",
            fmt(x0),
            fmt(y0),
            fmt(x1),
            fmt(y1)
        ));
    }
    out
}
