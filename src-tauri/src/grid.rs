pub(crate) fn downsample<T: Copy>(
  src: &[T],
  sw: usize,
  sh: usize,
  max_dim: usize
) -> (Vec<T>, usize, usize) {
  if sw <= max_dim && sh <= max_dim {
    return (src.to_vec(), sw, sh);
  }
  let scale = (sw.max(sh) + max_dim - 1) / max_dim;
  let dw = (sw + scale - 1) / scale;
  let dh = (sh + scale - 1) / scale;
  let mut out = Vec::with_capacity(dw * dh);
  for row in 0..dh {
    for col in 0..dw {
      let sr = (row * scale).min(sh - 1);
      let sc = (col * scale).min(sw - 1);
      out.push(src[sr * sw + sc]);
    }
  }
  (out, dw, dh)
}

/// Bilinearly upsample a vertex-centered grid by an integer factor. Output is
/// (w-1)*factor+1 wide so source vertices land exactly on output vertices —
/// the geographic bbox is unchanged.
pub(crate) fn upsample_bilinear(
  src: &[f32],
  w: usize,
  h: usize,
  factor: usize
) -> (Vec<f32>, usize, usize) {
  let (dw, dh) = ((w - 1) * factor + 1, (h - 1) * factor + 1);
  let mut out = Vec::with_capacity(dw * dh);
  for r in 0..dh {
    let y = (r as f32) / (factor as f32);
    let r0 = (y as usize).min(h - 1);
    let r1 = (r0 + 1).min(h - 1);
    let fy = y - (r0 as f32);
    for c in 0..dw {
      let x = (c as f32) / (factor as f32);
      let c0 = (x as usize).min(w - 1);
      let c1 = (c0 + 1).min(w - 1);
      let fx = x - (c0 as f32);
      let top = src[r0 * w + c0] * (1.0 - fx) + src[r0 * w + c1] * fx;
      let bot = src[r1 * w + c0] * (1.0 - fx) + src[r1 * w + c1] * fx;
      out.push(top * (1.0 - fy) + bot * fy);
    }
  }
  (out, dw, dh)
}

/// Separable box blur with clamped, renormalized windows at the borders, so
/// a constant field stays exactly constant (no edge erosion).
pub(crate) fn box_blur(data: &mut [f32], w: usize, h: usize, radius: usize) {
  if radius == 0 {
    return;
  }
  let mut tmp = vec![0.0f32; w * h];
  for r in 0..h {
    for c in 0..w {
      let c0 = c.saturating_sub(radius);
      let c1 = (c + radius).min(w - 1);
      let sum: f32 = data[r * w + c0..=r * w + c1].iter().sum();
      tmp[r * w + c] = sum / ((c1 - c0 + 1) as f32);
    }
  }
  for c in 0..w {
    for r in 0..h {
      let r0 = r.saturating_sub(radius);
      let r1 = (r + radius).min(h - 1);
      let mut sum = 0.0f32;
      for rr in r0..=r1 {
        sum += tmp[rr * w + c];
      }
      data[r * w + c] = sum / ((r1 - r0 + 1) as f32);
    }
  }
}

/// Edge-smoothing supersample for exports: bilinear elevation upsample plus a
/// smoothed footprint upsample (0/1 indicator → bilinear → box blur → 0.5
/// threshold). Staircase footprint borders come out chamfered and rounded
/// instead of tracing pixel edges. Synthesizes no new terrain detail — it
/// only interpolates — and multiplies mesh dimensions by `factor`.
pub(crate) fn smooth_upsample(
  data: &[f32],
  footprint: &[bool],
  w: usize,
  h: usize,
  factor: usize
) -> Result<(Vec<f32>, Vec<bool>, usize, usize), String> {
  let (up_data, dw, dh) = upsample_bilinear(data, w, h, factor);
  let fp_field: Vec<f32> = footprint
    .iter()
    .map(|&b| if b { 1.0 } else { 0.0 })
    .collect();
  let (mut fp_up, _, _) = upsample_bilinear(&fp_field, w, h, factor);
  box_blur(&mut fp_up, dw, dh, factor / 2);
  let fp: Vec<bool> = fp_up
    .iter()
    .map(|&v| v >= 0.5)
    .collect();
  if !fp.iter().any(|&b| b) {
    return Err("Selection too small to smooth — reduce edge smoothing".into());
  }
  Ok((up_data, fp, dw, dh))
}

/// Scanline-fill the interior of a polygon into `mask` using the even-odd rule
/// across ALL rings at once, so inner rings (islands) become holes instead of
/// being flooded.
pub(crate) fn rasterize_rings(
  rings: &[Vec<[f64; 2]>],
  south: f64,
  north: f64,
  west: f64,
  east: f64,
  width: usize,
  height: usize,
  mask: &mut Vec<bool>
) {
  // lat/lon → (col, row) in pixel space, row 0 = north
  let to_px = |lat: f64, lon: f64| -> (f64, f64) {
    let col = ((lon - west) / (east - west)) * ((width as f64) - 1.0);
    let row = ((north - lat) / (north - south)) * ((height as f64) - 1.0);
    (col, row)
  };

  let px_rings: Vec<Vec<(f64, f64)>> = rings
    .iter()
    .filter(|r| r.len() >= 3)
    .map(|r|
      r
        .iter()
        .map(|&[lat, lon]| to_px(lat, lon))
        .collect()
    )
    .collect();
  if px_rings.is_empty() {
    return;
  }

  let min_row = px_rings
    .iter()
    .flatten()
    .map(|p| p.1)
    .fold(f64::INFINITY, f64::min)
    .max(0.0) as usize;
  let max_row = px_rings
    .iter()
    .flatten()
    .map(|p| p.1)
    .fold(f64::NEG_INFINITY, f64::max)
    .min((height as f64) - 1.0) as usize;

  for row in min_row..=max_row {
    let y = (row as f64) + 0.5;
    let mut xs: Vec<f64> = Vec::new();
    for px in &px_rings {
      let n = px.len();
      for i in 0..n {
        let (x0, y0) = px[i];
        let (x1, y1) = px[(i + 1) % n];
        if (y0 <= y && y < y1) || (y1 <= y && y < y0) {
          xs.push(x0 + ((y - y0) * (x1 - x0)) / (y1 - y0));
        }
      }
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut i = 0;
    while i + 1 < xs.len() {
      let (x0, x1) = (xs[i], xs[i + 1]);
      i += 2;
      // span entirely outside the raster
      if x1 < 0.0 || x0 > (width as f64) - 1.0 {
        continue;
      }
      let c0 = x0.max(0.0) as usize;
      let c1 = x1.min((width as f64) - 1.0) as usize;
      for col in c0..=c1 {
        mask[row * width + col] = true;
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::mesh::build_stl;
  use crate::test_support::assert_watertight;

  #[test]
  fn upsample_bilinear_preserves_vertices_and_interpolates() {
    let src = vec![0.0, 10.0, 20.0, 30.0]; // 2×2
    let (out, w, h) = upsample_bilinear(&src, 2, 2, 2);
    assert_eq!((w, h), (3, 3));
    // source vertices land exactly on even output coordinates
    assert_eq!(out[0], 0.0);
    assert_eq!(out[2], 10.0);
    assert_eq!(out[6], 20.0);
    assert_eq!(out[8], 30.0);
    // midpoints are averages
    assert_eq!(out[1], 5.0);
    assert_eq!(out[4], 15.0); // centre = mean of all four
  }

  #[test]
  fn box_blur_keeps_constant_fields_constant() {
    let mut data = vec![7.5f32; 5 * 4];
    box_blur(&mut data, 5, 4, 2);
    assert!(data.iter().all(|&v| (v - 7.5).abs() < 1e-6));
  }

  #[test]
  fn smooth_upsample_full_footprint_stays_full() {
    let data = vec![1.0f32; 4 * 4];
    let fp = vec![true; 4 * 4];
    let (_, fp_up, w, h) = smooth_upsample(&data, &fp, 4, 4, 3).unwrap();
    assert_eq!((w, h), (10, 10));
    assert!(fp_up.iter().all(|&b| b));
  }

  #[test]
  fn smooth_upsample_chamfers_staircase_and_meshes_watertight() {
    // diagonal staircase footprint on a 6×6 grid: true below the diagonal
    let (w, h) = (6usize, 6usize);
    let fp: Vec<bool> = (0..w * h).map(|i| i / w >= i % w).collect();
    let data: Vec<f32> = (0..w * h).map(|i| 100.0 + ((i / w) as f32)).collect();
    let factor = 2;
    let (up_data, up_fp, uw, uh) = smooth_upsample(&data, &fp, w, h, factor).unwrap();

    // smoothing must actually change the shape vs. nearest-neighbour upsampling
    let nearest: Vec<bool> = (0..uw * uh)
      .map(|i| {
        let (r, c) = ((i / uw / factor).min(h - 1), (i % uw / factor).min(w - 1));
        fp[r * w + c]
      })
      .collect();
    assert_ne!(up_fp, nearest);

    let stl = build_stl(&up_data, &up_fp, uw, uh, 1.0, 1.0, 45.0, 45.1, -73.1, -73.0).unwrap();
    assert_watertight(&stl);
  }

  #[test]
  fn smooth_upsample_of_vanishing_selection_is_an_error() {
    // a single isolated pixel dissolves under blur+threshold
    let (w, h) = (7usize, 7usize);
    let mut fp = vec![false; w * h];
    fp[3 * w + 3] = true;
    let data = vec![1.0f32; w * h];
    assert!(smooth_upsample(&data, &fp, w, h, 4).is_err());
  }

  #[test]
  fn downsample_caps_dimensions_and_preserves_values() {
    let src: Vec<f32> = (0..100 * 60).map(|i| i as f32).collect();
    let (out, w, h) = downsample(&src, 100, 60, 32);
    assert!(w <= 32 && h <= 32);
    assert_eq!(out.len(), w * h);
    // Nearest sampling: every output value must exist in the source.
    assert!(out.iter().all(|v| src.contains(v)));
    // No-op below the cap
    let (same, sw, sh) = downsample(&src, 100, 60, 128);
    assert_eq!((sw, sh), (100, 60));
    assert_eq!(same, src);
  }

  #[test]
  fn inner_rings_become_holes() {
    let outer = vec![[0.2, 0.2], [0.2, 0.8], [0.8, 0.8], [0.8, 0.2], [0.2, 0.2]];
    let inner = vec![[0.4, 0.4], [0.4, 0.6], [0.6, 0.6], [0.6, 0.4], [0.4, 0.4]];
    let mut mask = vec![false; 100 * 100];
    rasterize_rings(&[outer, inner], 0.0, 1.0, 0.0, 1.0, 100, 100, &mut mask);
    // centre of the hole is land, ring between outer and inner is water
    assert!(!mask[50 * 100 + 50]);
    assert!(mask[30 * 100 + 30]);
    assert!(!mask[10 * 100 + 10]);
  }
}
