use crate::water::RingGroup;

const EPS: f64 = 1e-9;
/// Tolerance (degrees) for node coincidence and "on the clip border" tests.
const NODE_EPS: f64 = 1e-7;

fn close_pts(a: [f64; 2], b: [f64; 2]) -> bool {
  (a[0] - b[0]).abs() < NODE_EPS && (a[1] - b[1]).abs() < NODE_EPS
}

/// Chain coastline way polylines end-to-end WITHOUT flipping: unlike water
/// polygon segments, coastline direction is meaningful — OSM draws them with
/// land on the left and water on the right.
fn chain_coastlines(ways: &[Vec<[f64; 2]>]) -> Vec<Vec<[f64; 2]>> {
  let mut segs: Vec<Vec<[f64; 2]>> = ways
    .iter()
    .filter(|w| w.len() >= 2)
    .cloned()
    .collect();
  let mut chains = Vec::new();
  while let Some(mut chain) = segs.pop() {
    loop {
      if chain.len() >= 3 && close_pts(chain[0], *chain.last().unwrap()) {
        break; // closed ring
      }
      if let Some(j) = segs.iter().position(|s| close_pts(*chain.last().unwrap(), s[0])) {
        let seg = segs.remove(j);
        chain.extend_from_slice(&seg[1..]);
      } else if let Some(j) = segs.iter().position(|s| close_pts(*s.last().unwrap(), chain[0])) {
        let mut seg = segs.remove(j);
        seg.extend_from_slice(&chain[1..]);
        chain = seg;
      } else {
        break;
      }
    }
    chains.push(chain);
  }
  chains
}

/// Liang–Barsky: the parameter range of segment p→q inside the rect, if any.
fn clip_seg(p: [f64; 2], q: [f64; 2], s: f64, n: f64, w: f64, e: f64) -> Option<(f64, f64)> {
  let d = [q[0] - p[0], q[1] - p[1]];
  let checks = [
    (-d[0], p[0] - s),
    (d[0], n - p[0]),
    (-d[1], p[1] - w),
    (d[1], e - p[1]),
  ];
  let (mut t0, mut t1) = (0.0f64, 1.0f64);
  for (den, num) in checks {
    if den.abs() < EPS {
      if num < 0.0 {
        return None;
      }
    } else {
      let t = num / den;
      if den < 0.0 {
        t0 = t0.max(t);
      } else {
        t1 = t1.min(t);
      }
    }
  }
  if t0 > t1 + EPS {
    None
  } else {
    Some((t0, t1))
  }
}

/// Clip a polyline to the rect, splitting it into the sub-chains that lie
/// inside. Sub-chain endpoints created by clipping fall exactly on the rect
/// border; endpoints inherited from the input stay where they were.
fn clip_chain(chain: &[[f64; 2]], s: f64, n: f64, w: f64, e: f64) -> Vec<Vec<[f64; 2]>> {
  let mut subs: Vec<Vec<[f64; 2]>> = Vec::new();
  let mut cur: Vec<[f64; 2]> = Vec::new();
  for i in 0..chain.len().saturating_sub(1) {
    let (p, q) = (chain[i], chain[i + 1]);
    match clip_seg(p, q, s, n, w, e) {
      None => {
        if !cur.is_empty() {
          subs.push(std::mem::take(&mut cur));
        }
      }
      Some((t0, t1)) => {
        let at = |t: f64| [p[0] + t * (q[0] - p[0]), p[1] + t * (q[1] - p[1])];
        if t0 > EPS {
          if !cur.is_empty() {
            subs.push(std::mem::take(&mut cur));
          }
          cur.push(at(t0));
        } else if cur.is_empty() {
          cur.push(p);
        }
        cur.push(at(t1));
        if t1 < 1.0 - EPS {
          subs.push(std::mem::take(&mut cur));
        }
      }
    }
  }
  if !cur.is_empty() {
    subs.push(cur);
  }
  subs.retain(|sc| sc.len() >= 2);
  subs
}

/// Close coastline chains along a slightly inflated bbox border into land
/// rings (following the land-on-left convention: border arcs are walked
/// counterclockwise so land stays on the left), and return ONE ring group
/// whose even-odd fill is the OCEAN: an oversized outer ring plus the land
/// rings — inside the outer ring only = sea, inside outer + land = land.
///
/// The clip rect is inflated so the border arcs land outside the raster
/// (rasterization clamps them away); the outer ring is inflated further so
/// it strictly contains the arcs.
///
/// Chains legitimately end just outside the queried bbox: Overpass returns
/// ways with a node inside the bbox, so the junction with the first
/// unreturned way always lies beyond the border — possibly within the
/// inflated margin band. Such endpoints are snapped out to the clip border
/// (routing inside the band can't affect any raster pixel). Only endpoints
/// strictly inside the true bbox are treated as incomplete data.
///
/// Returns Ok(None) when no coastline is present (inland bbox — or open
/// ocean with no data, indistinguishable here). Errors when a chain endpoint
/// lies inside the true bbox (incomplete coastline data); callers should
/// fall back to a raster heuristic like `flood_ocean_from_border`.
pub(crate) fn ocean_ring_group(
  coastlines: &[Vec<[f64; 2]>],
  south: f64,
  north: f64,
  west: f64,
  east: f64
) -> Result<Option<RingGroup>, String> {
  if coastlines.is_empty() {
    return Ok(None);
  }
  let delta = 0.02 * (north - south).max(east - west);
  let (s, n, w, e) = (south - delta, north + delta, west - delta, east + delta);
  let (rw, rh) = (e - w, n - s);
  let perim = 2.0 * (rw + rh);

  let inside = |p: [f64; 2]|
    p[0] >= s - EPS && p[0] <= n + EPS && p[1] >= w - EPS && p[1] <= e + EPS;
  let inside_true_bbox = |p: [f64; 2]| {
    p[0] > south && p[0] < north && p[1] > west && p[1] < east
  };

  let mut land_rings: Vec<Vec<[f64; 2]>> = Vec::new();
  let mut subs: Vec<Vec<[f64; 2]>> = Vec::new();

  for mut chain in chain_coastlines(coastlines) {
    let mut closed = chain.len() >= 4 && close_pts(chain[0], *chain.last().unwrap());
    if !closed {
      let (a, b) = (chain[0], *chain.last().unwrap());
      if inside_true_bbox(a) || inside_true_bbox(b) {
        return Err("Coastline endpoint inside the region — incomplete coastline data".into());
      }
      // Both dangling ends outside the bbox on a common side: an island or
      // coast fragment whose continuation never enters the bbox (Overpass
      // only returns ways with a node inside). Close it directly — the
      // synthetic edge stays outside the bbox, so no raster pixel can see
      // it. Snapping such fragments to the border and walking instead
      // fabricates crossings with arbitrary orientation and can enclose
      // the whole bbox rather than the fragment.
      let common_side =
        (a[0] <= south && b[0] <= south) ||
        (a[0] >= north && b[0] >= north) ||
        (a[1] <= west && b[1] <= west) ||
        (a[1] >= east && b[1] >= east);
      if common_side && chain.len() >= 3 {
        chain.push(chain[0]);
        closed = true;
      }
    }
    if closed && chain.iter().all(|&p| inside(p)) {
      land_rings.push(chain); // island (or sea hole) fully inside
      continue;
    }
    let open: Vec<[f64; 2]> = if closed {
      // A crossing ring: rotate it to start outside the rect so the seam
      // can't fall inside and masquerade as a data gap.
      let k = chain
        .iter()
        .position(|&p| !inside(p))
        .expect("closed && !all-inside implies an outside point");
      let mut rot: Vec<[f64; 2]> = chain[k..chain.len() - 1].to_vec();
      rot.extend_from_slice(&chain[..=k]);
      rot
    } else {
      chain
    };
    subs.extend(clip_chain(&open, s, n, w, e));
  }

  let on_border = |p: [f64; 2]| {
    (p[0] - s).abs() < NODE_EPS ||
      (n - p[0]).abs() < NODE_EPS ||
      (p[1] - w).abs() < NODE_EPS ||
      (e - p[1]).abs() < NODE_EPS
  };
  // Perimeter coordinate, counterclockwise from the SW corner (so walking in
  // +t keeps the interior — the land side of a border arc — on the left).
  let t_of = |p: [f64; 2]| -> f64 {
    if (p[0] - s).abs() < NODE_EPS {
      p[1] - w // south edge, eastbound
    } else if (p[1] - e).abs() < NODE_EPS {
      rw + (p[0] - s) // east edge, northbound
    } else if (p[0] - n).abs() < NODE_EPS {
      rw + rh + (e - p[1]) // north edge, westbound
    } else {
      2.0 * rw + rh + (n - p[0]) // west edge, southbound
    }
  };

  // Push a transit chain's dangling endpoint from the margin band out to
  // the clip border, on the side(s) of the true bbox it already exceeds.
  // Only different-side dangles reach here (common-side fragments were
  // closed directly above), so the exceed direction reflects where the
  // chain was genuinely heading and the fabricated crossing keeps the real
  // orientation.
  let snap = |p: [f64; 2]| -> [f64; 2] {
    let mut q = p;
    if p[0] <= south {
      q[0] = s;
    } else if p[0] >= north {
      q[0] = n;
    }
    if p[1] <= west {
      q[1] = w;
    } else if p[1] >= east {
      q[1] = e;
    }
    q
  };

  let mut t_in = Vec::with_capacity(subs.len());
  let mut t_out = Vec::with_capacity(subs.len());
  for sc in subs.iter_mut() {
    if !on_border(sc[0]) {
      let p = sc[0];
      if inside_true_bbox(p) {
        return Err("Coastline endpoint inside the region — incomplete coastline data".into());
      }
      sc.insert(0, snap(p));
    }
    if !on_border(*sc.last().unwrap()) {
      let p = *sc.last().unwrap();
      if inside_true_bbox(p) {
        return Err("Coastline endpoint inside the region — incomplete coastline data".into());
      }
      sc.push(snap(p));
    }
    t_in.push(t_of(sc[0]));
    t_out.push(t_of(*sc.last().unwrap()));
  }

  let cyc = |from: f64, to: f64| {
    let d = (to - from).rem_euclid(perim);
    if d < EPS { perim } else { d }
  };
  let corner_t = [rw, rw + rh, 2.0 * rw + rh, perim];
  let corner_p = [
    [s, e],
    [n, e],
    [n, w],
    [s, w],
  ];

  let mut used = vec![false; subs.len()];
  for i in 0..subs.len() {
    if used[i] {
      continue;
    }
    used[i] = true;
    let mut ring = subs[i].clone();
    let start_t = t_in[i];
    let mut t = t_out[i];
    loop {
      // nearest entry point walking counterclockwise from t; the starting
      // chain's own entry closes the ring
      let mut next: (usize, f64) = (usize::MAX, cyc(t, start_t));
      for j in 0..subs.len() {
        if !used[j] {
          let d = cyc(t, t_in[j]);
          if d < next.1 {
            next = (j, d);
          }
        }
      }
      let mut passed: Vec<(f64, [f64; 2])> = corner_t
        .iter()
        .zip(corner_p)
        .map(|(&ct, cp)| (cyc(t, ct), cp))
        .filter(|&(d, _)| d < next.1)
        .collect();
      passed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
      for (_, cp) in passed {
        ring.push(cp);
      }
      if next.0 == usize::MAX {
        break;
      }
      used[next.0] = true;
      ring.extend_from_slice(&subs[next.0]);
      t = t_out[next.0];
    }
    land_rings.push(ring);
  }

  let big = 2.0 * delta;
  let outer = vec![
    [south - big, west - big],
    [south - big, east + big],
    [north + big, east + big],
    [north + big, west - big],
  ];
  let mut group: RingGroup = vec![outer];
  group.extend(land_rings);
  Ok(Some(group))
}

/// Ocean fallback when coastline geometry is absent or fails to close:
/// flood-fill from the raster border across pixels at or below sea level.
/// Below-sea-level inland basins survive unless they touch the border
/// (which also means below-sea-level coastal land like polders can be
/// misclassified — this is the heuristic's known trade-off).
pub(crate) fn flood_ocean_from_border(data: &[f32], width: usize, height: usize) -> Vec<bool> {
  let mut ocean = vec![false; width * height];
  let mut stack: Vec<usize> = Vec::new();
  let seed = |i: usize, ocean: &mut Vec<bool>, stack: &mut Vec<usize>| {
    if !ocean[i] && data[i] <= 0.0 {
      ocean[i] = true;
      stack.push(i);
    }
  };
  for col in 0..width {
    seed(col, &mut ocean, &mut stack);
    seed((height - 1) * width + col, &mut ocean, &mut stack);
  }
  for row in 0..height {
    seed(row * width, &mut ocean, &mut stack);
    seed(row * width + width - 1, &mut ocean, &mut stack);
  }
  while let Some(i) = stack.pop() {
    let (row, col) = (i / width, i % width);
    if col > 0 {
      seed(i - 1, &mut ocean, &mut stack);
    }
    if col + 1 < width {
      seed(i + 1, &mut ocean, &mut stack);
    }
    if row > 0 {
      seed(i - width, &mut ocean, &mut stack);
    }
    if row + 1 < height {
      seed(i + width, &mut ocean, &mut stack);
    }
  }
  ocean
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::water::rasterize_ring_groups;

  /// Rasterize an ocean group over the unit bbox at 50×50 and return a
  /// lat/lon point probe. (Rasters must be fine enough that the inflated
  /// border arcs land beyond the outermost scanline — >25px across here.)
  fn probe(group: RingGroup) -> impl Fn(f64, f64) -> bool {
    let mask = rasterize_ring_groups(&[group], 0.0, 1.0, 0.0, 1.0, 50, 50);
    move |lat: f64, lon: f64| mask[(((1.0 - lat) * 49.0) as usize) * 50 + ((lon * 49.0) as usize)]
  }

  #[test]
  fn straight_coastline_puts_water_on_the_right() {
    // Southbound coastline at lon 0.5 → land on the left = east half.
    let ways = vec![vec![[1.5, 0.5], [-0.5, 0.5]]];
    let group = ocean_ring_group(&ways, 0.0, 1.0, 0.0, 1.0).unwrap().unwrap();
    let water = probe(group);
    assert!(water(0.5, 0.2), "west of the coastline is sea");
    assert!(water(0.05, 0.05), "sea reaches the SW corner");
    assert!(water(0.95, 0.2), "sea reaches the NW side");
    assert!(!water(0.5, 0.8), "east of the coastline is land");
    assert!(!water(0.05, 0.95), "land reaches the SE corner");
  }

  #[test]
  fn u_shaped_coastline_makes_a_peninsula() {
    // Enters from the north at lon 0.3, U-turns at lat 0.5, exits north at
    // lon 0.7: land-on-left encloses the peninsula between the two legs.
    let ways = vec![vec![[1.5, 0.3], [0.5, 0.3], [0.5, 0.7], [1.5, 0.7]]];
    let group = ocean_ring_group(&ways, 0.0, 1.0, 0.0, 1.0).unwrap().unwrap();
    let water = probe(group);
    assert!(!water(0.8, 0.5), "inside the U is land");
    assert!(water(0.8, 0.1), "west of the peninsula is sea");
    assert!(water(0.8, 0.9), "east of the peninsula is sea");
    assert!(water(0.2, 0.5), "south of the U-turn is sea");
  }

  #[test]
  fn closed_ring_is_an_island() {
    let ring = vec![[0.4, 0.4], [0.4, 0.6], [0.6, 0.6], [0.6, 0.4], [0.4, 0.4]];
    let group = ocean_ring_group(&[ring], 0.0, 1.0, 0.0, 1.0).unwrap().unwrap();
    let water = probe(group);
    assert!(!water(0.5, 0.5), "island interior is land");
    assert!(water(0.2, 0.2), "around the island is sea");
    assert!(water(0.5, 0.9), "sea east of the island");
  }

  #[test]
  fn way_segments_chain_without_flipping() {
    // The straight coastline split into two ways, given out of order —
    // chaining must join them tail-to-head (no flip) and close identically.
    let ways = vec![vec![[0.4, 0.5], [-0.5, 0.5]], vec![[1.5, 0.5], [0.4, 0.5]]];
    let group = ocean_ring_group(&ways, 0.0, 1.0, 0.0, 1.0).unwrap().unwrap();
    let water = probe(group);
    assert!(water(0.5, 0.2));
    assert!(!water(0.5, 0.8));
  }

  #[test]
  fn dangling_endpoint_is_an_error() {
    // Coastline that just stops mid-bbox: incomplete data, must not guess.
    let ways = vec![vec![[1.5, 0.5], [0.7, 0.5]]];
    assert!(ocean_ring_group(&ways, 0.0, 1.0, 0.0, 1.0).is_err());
  }

  #[test]
  fn island_fragment_with_gap_outside_closes_as_island() {
    // An island straddling the west border: Overpass only returns the ways
    // with a node inside, so the chain dangles on both ends just outside
    // the bbox (the Marettimo case from the live Sicily bbox). It must
    // close as the island itself — NOT via border arcs, which would
    // enclose the whole bbox and invert the mask.
    // CCW ring fragment (land on the left), west edge missing:
    let frag = vec![vec![[0.4, -0.01], [0.4, 0.15], [0.6, 0.15], [0.6, -0.01]]];
    let group = ocean_ring_group(&frag, 0.0, 1.0, 0.0, 1.0).unwrap().unwrap();
    let water = probe(group);
    assert!(!water(0.5, 0.05), "island sliver is land");
    assert!(water(0.5, 0.5), "mid-bbox is open sea");
    assert!(water(0.2, 0.1), "sea south of the island");
    assert!(water(0.8, 0.1), "sea north of the island");
  }

  #[test]
  fn island_fragment_beyond_clip_rect_still_closes_as_island() {
    // Same case but the dangles reach beyond the inflated clip rect, so the
    // direct-joined ring goes through rotate-and-clip and the border walk.
    let frag = vec![vec![[0.4, -0.05], [0.4, 0.15], [0.6, 0.15], [0.6, -0.05]]];
    let group = ocean_ring_group(&frag, 0.0, 1.0, 0.0, 1.0).unwrap().unwrap();
    let water = probe(group);
    assert!(!water(0.5, 0.05), "island sliver is land");
    assert!(water(0.5, 0.5), "mid-bbox is open sea");
    assert!(water(0.2, 0.1), "sea south of the island");
  }

  #[test]
  fn endpoint_in_margin_band_snaps_to_border() {
    // Overpass returns ways with a node inside the bbox, so chains end at
    // junctions just OUTSIDE it — here at lat -0.01, within the inflated
    // margin. That must close cleanly, not read as a data gap.
    let ways = vec![vec![[1.5, 0.5], [-0.01, 0.5]]];
    let group = ocean_ring_group(&ways, 0.0, 1.0, 0.0, 1.0).unwrap().unwrap();
    let water = probe(group);
    assert!(water(0.5, 0.2), "sea west of the coastline");
    assert!(!water(0.5, 0.8), "land east of the coastline");
  }

  #[test]
  fn no_coastlines_is_none() {
    assert!(ocean_ring_group(&[], 0.0, 1.0, 0.0, 1.0).unwrap().is_none());
  }

  /// Live network check against the bbox that surfaced the missing-ocean
  /// bug (western Sicily): set LANDGREP_LIVE_OVERPASS=1 to run.
  #[test]
  fn live_sicily_ocean_closes() {
    if std::env::var("LANDGREP_LIVE_OVERPASS").is_err() {
      return;
    }
    let (s, n, w, e) = (37.0333, 38.346, 12.0355, 13.5626);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let geo = rt.block_on(crate::water::fetch_water_geometry(s, n, w, e)).unwrap();
    assert!(!geo.coastlines.is_empty(), "Sicily bbox must return coastline ways");
    let group = ocean_ring_group(&geo.coastlines, s, n, w, e)
      .expect("coastline closing failed")
      .unwrap();
    let mut rings = geo.water_groups;
    rings.push(group);
    let (mw, mh) = (400usize, 340usize);
    let mask = rasterize_ring_groups(&rings, s, n, w, e, mw, mh);
    let at = |lat: f64, lon: f64| {
      let col = (((lon - w) / (e - w)) * ((mw as f64) - 1.0)) as usize;
      let row = (((n - lat) / (n - s)) * ((mh as f64) - 1.0)) as usize;
      mask[row * mw + col]
    };
    assert!(at(38.2, 12.5), "Tyrrhenian Sea northwest of Trapani should be water");
    assert!(at(37.15, 12.8), "Mediterranean south of Sicily should be water");
    assert!(!at(37.8, 13.3), "inland Sicily should be land");
    assert!(!at(38.0, 12.7), "the Trapani hinterland should be land");
  }

  #[test]
  fn sicily_fixture_ocean_is_masked() {
    let body = crate::test_support::sicily_fixture();
    let geo = crate::water::geometry_from_overpass(&body).unwrap();
    assert_eq!(geo.coastlines.len(), 212);
    let (s, n, w, e) = (37.0333, 38.346, 12.0355, 13.5626);
    let group = ocean_ring_group(&geo.coastlines, s, n, w, e).unwrap().unwrap();
    let (mw, mh) = (400usize, 340usize);
    let mask = rasterize_ring_groups(&[group], s, n, w, e, mw, mh);
    let at = |lat: f64, lon: f64| {
      let col = (((lon - w) / (e - w)) * ((mw as f64) - 1.0)) as usize;
      let row = (((n - lat) / (n - s)) * ((mh as f64) - 1.0)) as usize;
      mask[row * mw + col]
    };
    assert!(at(38.2, 12.5), "Tyrrhenian Sea northwest of Trapani should be water");
    assert!(at(37.15, 12.8), "Mediterranean south of Sicily should be water");
    assert!(!at(37.8, 13.3), "inland Sicily should be land");
    assert!(!at(38.0, 12.7), "the Trapani hinterland should be land");
    assert!(!at(37.985, 12.06), "the Marettimo sliver should be land, not invert the mask");
    let frac = (mask
      .iter()
      .filter(|&&m| m)
      .count() as f64) / (mask.len() as f64);
    assert!((0.55..0.8).contains(&frac), "sea should be roughly 2/3 of this bbox, got {frac:.2}");
  }

  #[test]
  fn flood_fill_respects_border_connectivity() {
    let (w, h) = (10usize, 10usize);
    let mut data = vec![100.0f32; w * h];
    // sea strip touching the west border
    for r in 0..h {
      data[r * w] = 0.0;
      data[r * w + 1] = 0.0;
    }
    // inland depression below sea level, not border-connected
    data[5 * w + 5] = -3.0;
    let ocean = flood_ocean_from_border(&data, w, h);
    assert!(ocean[4 * w], "border sea flooded");
    assert!(ocean[4 * w + 1], "sea strip flooded inward");
    assert!(!ocean[5 * w + 5], "inland depression stays land");
    assert!(!ocean[4 * w + 5], "high ground stays land");
  }
}
