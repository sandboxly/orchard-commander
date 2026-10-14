//! The info window's donut chart as runs of pixels: drawing stays a pure
//! function of the window's state (no image to keep in step with the color
//! scheme or the window size), and the renderer gets one rectangle per run.
//! Pixels on the ring's edges and on slice borders are 4×4 supersampled, so
//! the chart is smooth at any scale.

use oc_grid::Rgba;

/// A horizontal run of pixels of one color: x, y (from the chart's top
/// left), length.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Run {
    pub x: u32,
    pub y: u32,
    pub len: u32,
    pub color: Rgba,
}

const SUBSAMPLES: u32 = 4;

/// A donut of `diameter` pixels whose hole is `hole` (0..1) of it.
/// `slices` are (share, color), clockwise from 12 o'clock; shares are
/// relative to their sum. Pixels outside the ring are left out, so `bg`
/// (what lies behind) only colors the edges' blend.
pub fn donut(diameter: u32, hole: f32, slices: &[(f64, Rgba)], bg: Rgba) -> Vec<Run> {
    let total: f64 = slices.iter().map(|s| s.0.max(0.0)).sum();
    if diameter == 0 || total <= 0.0 {
        return Vec::new();
    }
    // Where each slice ends, as a fraction of the turn.
    let mut ends = Vec::with_capacity(slices.len());
    let mut acc = 0.0;
    for (share, _) in slices {
        acc += share.max(0.0) / total;
        ends.push(acc);
    }
    let r_out = diameter as f64 / 2.0;
    let r_in = r_out * f64::from(hole.clamp(0.0, 0.95));
    let center = r_out;
    let tau = std::f64::consts::TAU;
    let slice_at = |x: f64, y: f64| -> Option<Rgba> {
        let (dx, dy) = (x - center, y - center);
        let r = dx.hypot(dy);
        if r > r_out || r < r_in {
            return None;
        }
        let turn = dx.atan2(-dy).rem_euclid(tau) / tau;
        let i = ends.partition_point(|&e| e <= turn).min(slices.len() - 1);
        Some(slices[i].1)
    };
    // Slice borders as angles, for the supersampling test (a single slice has none).
    let borders: Vec<f64> =
        if slices.iter().filter(|s| s.0 > 0.0).count() > 1 { ends.iter().map(|e| e * tau).collect() } else { Vec::new() };
    let near_edge = |x: f64, y: f64| -> bool {
        let (dx, dy) = (x - center, y - center);
        let r = dx.hypot(dy);
        if (r - r_out).abs() < 1.0 || (r - r_in).abs() < 1.0 {
            return true;
        }
        if r > r_out || r < r_in {
            return false;
        }
        let a = dx.atan2(-dy).rem_euclid(tau);
        borders.iter().any(|&b| {
            let d = (a - b).abs();
            d.min(tau - d) * r < 1.0
        })
    };
    let mut runs: Vec<Run> = Vec::new();
    for py in 0..diameter {
        let mut open: Option<Run> = None;
        for px in 0..diameter {
            let (cx, cy) = (px as f64 + 0.5, py as f64 + 0.5);
            let color = if near_edge(cx, cy) {
                blend_samples(px, py, bg, &slice_at)
            } else {
                slice_at(cx, cy)
            };
            match (color, open.as_mut()) {
                (Some(c), Some(run)) if run.color == c => run.len += 1,
                (c, _) => {
                    if let Some(run) = open.take() {
                        runs.push(run);
                    }
                    open = c.map(|color| Run { x: px, y: py, len: 1, color });
                }
            }
        }
        runs.extend(open);
    }
    runs
}

/// The average color of a pixel's subsamples, `bg` where they miss the ring;
/// None if all of them do.
fn blend_samples(px: u32, py: u32, bg: Rgba, slice_at: &impl Fn(f64, f64) -> Option<Rgba>) -> Option<Rgba> {
    let (mut r, mut g, mut b, mut hits) = (0u32, 0u32, 0u32, 0u32);
    let n = SUBSAMPLES;
    for sy in 0..n {
        for sx in 0..n {
            let x = px as f64 + (sx as f64 + 0.5) / n as f64;
            let y = py as f64 + (sy as f64 + 0.5) / n as f64;
            let c = match slice_at(x, y) {
                Some(c) => {
                    hits += 1;
                    c
                }
                None => bg,
            };
            r += u32::from(c.r);
            g += u32::from(c.g);
            b += u32::from(c.b);
        }
    }
    let count = n * n;
    (hits > 0).then(|| Rgba::rgb((r / count) as u8, (g / count) as u8, (b / count) as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: Rgba = Rgba::rgb(255, 0, 0);
    const BLUE: Rgba = Rgba::rgb(0, 0, 255);
    const BG: Rgba = Rgba::rgb(0, 0, 0);

    fn pixel(runs: &[Run], x: u32, y: u32) -> Option<Rgba> {
        runs.iter().find(|r| r.y == y && (r.x..r.x + r.len).contains(&x)).map(|r| r.color)
    }

    #[test]
    fn halves_split_at_twelve_and_six() {
        let runs = donut(100, 0.5, &[(1.0, RED), (1.0, BLUE)], BG);
        assert_eq!(pixel(&runs, 90, 50), Some(RED), "right half: the first slice");
        assert_eq!(pixel(&runs, 10, 50), Some(BLUE));
        assert_eq!(pixel(&runs, 50, 50), None, "the hole");
        assert_eq!(pixel(&runs, 1, 1), None, "outside the ring");
        // The edge blends with the background.
        let blended = runs.iter().filter(|r| r.color != RED && r.color != BLUE).count();
        assert!(blended > 100, "{blended} blended pixels");
        assert!(runs.iter().all(|r| r.color != BG));
        // Runs never overlap and stay inside the box.
        for r in &runs {
            assert!(r.x + r.len <= 100 && r.y < 100);
        }
    }

    #[test]
    fn shares_follow_their_sizes() {
        let runs = donut(200, 0.0, &[(3.0, RED), (1.0, BLUE)], BG);
        let count = |c: Rgba| runs.iter().filter(|r| r.color == c).map(|r| u64::from(r.len)).sum::<u64>() as f64;
        let ratio = count(RED) / count(BLUE);
        assert!((ratio - 3.0).abs() < 0.1, "{ratio}");
        // Upper left quarter: the last slice (clockwise from 12, 75 % is red).
        assert_eq!(pixel(&runs, 60, 60), Some(BLUE));
        assert_eq!(pixel(&runs, 60, 140), Some(RED));
    }

    #[test]
    fn nothing_to_draw() {
        assert!(donut(0, 0.5, &[(1.0, RED)], BG).is_empty());
        assert!(donut(50, 0.5, &[], BG).is_empty());
        assert!(donut(50, 0.5, &[(0.0, RED)], BG).is_empty());
        // One slice: a full ring, with no border to smooth.
        let runs = donut(50, 0.5, &[(5.0, RED), (0.0, BLUE)], BG);
        assert!(runs.iter().all(|r| r.color != BLUE));
    }
}
