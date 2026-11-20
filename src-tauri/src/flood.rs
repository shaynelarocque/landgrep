//! Sea-level rise. Water comes in from the map edge (the sea beyond the bbox
//! is assumed to connect) and through every water body already on the map,
//! and fills the terrain up to the new level wherever it can reach: a
//! bathtub with plumbing, so a basin behind a ridge stays dry until the
//! water tops the ridge.

use std::cmp::{ Ordering, Reverse };
use std::collections::BinaryHeap;

use crate::CachedElevation;
use crate::landmass::label_landmasses;
use crate::water::{ RingGroup, flatten_water };

/// Sea-level state for one fetch. Nothing is copied until the sea first
/// moves.
#[derive(Default)]
pub(crate) struct Sea {
  /// Metres above today's sea level; None = the coastline as fetched.
  pub(crate) level: Option<f32>,
  /// Terrain and water as fetched. Every sea level is rebuilt from these,
  /// so going back down restores the fetch exactly.
  fetched: Option<(Vec<f32>, Option<Vec<bool>>)>,
  /// `flood_levels` of the fetched terrain, computed on first use.
  levels: Option<Vec<f32>>,
}

impl CachedElevation {
  /// Move the sea to `level` (None, or ≤ 0, = as fetched) and rebuild
  /// everything that hangs off the coastline: the water mask, the flattened
  /// terrain and the landmass labels. Selections drawn against the old
  /// coastline are dropped. Returns the level the water was flattened to.
  pub(crate) fn set_sea_level(&mut self, level: Option<f32>) -> Option<f32> {
    let (w, h) = (self.width, self.height);
    let level = level.filter(|l| *l > 0.0);
    self.sea.level = level;
    let (terrain, fetched_water) = &*self.sea.fetched.get_or_insert_with(|| (
      self.data.clone(),
      self.water_mask.clone(),
    ));
    let mask = match level {
      None => fetched_water.clone(),
      Some(level) => {
        let dry;
        let water = match fetched_water {
          Some(m) => m.as_slice(),
          None => {
            dry = vec![false; w * h];
            &dry
          }
        };
        let levels = self.sea.levels.get_or_insert_with(|| flood_levels(terrain, water, w, h));
        Some(flooded_mask(water, levels, level))
      }
    };

    let mut data = terrain.clone();
    let water_level = mask.as_ref().and_then(|m| flatten_water(&mut data, m, w));
    let (labels, n) = match &mask {
      Some(m) => {
        let (l, n) = label_landmasses(m, w, h);
        (Some(l), n)
      }
      None => (None, 0),
    };
    self.min_elev = data.iter().copied().fold(f32::INFINITY, f32::min);
    self.max_elev = data.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    self.data = data;
    self.water_mask = mask;
    self.labels = labels;
    self.n_landmasses = n;
    self.lasso = None;
    self.lasso_ring = None;
    water_level
  }

  /// The fetched OSM rings, while they still describe the water. A raised
  /// sea has coastlines OSM never drew.
  pub(crate) fn current_water_rings(&self) -> Option<&Vec<RingGroup>> {
    if self.sea.level.is_some() { None } else { self.water_rings.as_ref() }
  }

  /// Pixels the raised sea has drowned: water now, land as fetched. None at
  /// the fetched sea level. The map overlays paint these, since the map
  /// under them still shows streets.
  pub(crate) fn drowned(&self) -> Option<Vec<bool>> {
    self.sea.level?;
    let now = self.water_mask.as_ref()?;
    let (_, fetched) = self.sea.fetched.as_ref()?;
    Some(match fetched {
      Some(f) => now
        .iter()
        .zip(f)
        .map(|(&n, &f)| n && !f)
        .collect(),
      None => now.clone(),
    })
  }
}

/// f32 with a total order, for the heap.
#[derive(PartialEq)]
struct Level(f32);

impl Eq for Level {}

impl PartialOrd for Level {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

impl Ord for Level {
  fn cmp(&self, other: &Self) -> Ordering {
    self.0.total_cmp(&other.0)
  }
}

/// Priority-Flood (Barnes, Lehman & Mulla 2014): for every pixel, the lowest
/// sea level that reaches it, i.e. the minimax elevation over all paths in
/// from the map edge or from existing water. Existing water reports −∞
/// (always wet). Computed once; every sea level after that is a threshold,
/// see `flooded_mask`. 4-connected, like `label_landmasses`.
pub(crate) fn flood_levels(elev: &[f32], water: &[bool], width: usize, height: usize) -> Vec<f32> {
  let n = width * height;
  let mut level = vec![f32::INFINITY; n];
  let mut heap: BinaryHeap<Reverse<(Level, usize)>> = BinaryHeap::new();

  let neighbours = |i: usize| {
    let (row, col) = (i / width, i % width);
    [
      (col > 0).then(|| i - 1),
      (col + 1 < width).then(|| i + 1),
      (row > 0).then(|| i - width),
      (row + 1 < height).then(|| i + width),
    ]
      .into_iter()
      .flatten()
  };

  // Existing water is wet at any level. Only its shore pixels can spread
  // anywhere, so only those need to go on the heap.
  for i in 0..n {
    if water[i] {
      level[i] = f32::NEG_INFINITY;
      if neighbours(i).any(|j| !water[j]) {
        heap.push(Reverse((Level(f32::NEG_INFINITY), i)));
      }
    }
  }
  // The edge of the map is open sea: a border pixel floods as soon as the
  // water is above it.
  for i in 0..n {
    let (row, col) = (i / width, i % width);
    let border = row == 0 || col == 0 || row + 1 == height || col + 1 == width;
    if border && !water[i] {
      level[i] = elev[i];
      heap.push(Reverse((Level(elev[i]), i)));
    }
  }

  while let Some(Reverse((Level(l), i))) = heap.pop() {
    if l > level[i] {
      continue; // stale: already reached lower
    }
    for j in neighbours(i) {
      let lj = l.max(elev[j]);
      if lj < level[j] {
        level[j] = lj;
        heap.push(Reverse((Level(lj), j)));
      }
    }
  }
  level
}

/// The water mask at sea level `sea_level`: what was already water, plus
/// everything the rise reaches.
pub(crate) fn flooded_mask(water: &[bool], levels: &[f32], sea_level: f32) -> Vec<bool> {
  water
    .iter()
    .zip(levels)
    .map(|(&w, &l)| w || l <= sea_level)
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  /// West to east, three rows alike: sea (OSM water) at 0 m, a 5 m beach, a
  /// 40 m hill, a 10 m valley, a 60 m hill, a 30 m plateau. Built the way
  /// fetch_elevation builds it, water flattened and land labelled.
  fn two_hills(osm_water: bool) -> CachedElevation {
    let cols = [0.0f32, 5.0, 40.0, 40.0, 10.0, 60.0, 60.0, 30.0, 30.0];
    let (w, h) = (cols.len(), 3usize);
    let mut data: Vec<f32> = (0..w * h).map(|i| cols[i % w]).collect();
    let mask: Option<Vec<bool>> = osm_water.then(|| (0..w * h).map(|i| i % w == 0).collect());
    if let Some(m) = &mask {
      flatten_water(&mut data, m, w);
    }
    let (labels, n) = match &mask {
      Some(m) => {
        let (l, n) = label_landmasses(m, w, h);
        (Some(l), n)
      }
      None => (None, 0),
    };
    CachedElevation {
      data,
      width: w,
      height: h,
      min_elev: 0.0,
      max_elev: 60.0,
      south: 45.0,
      north: 45.01,
      west: -73.0,
      east: -72.97,
      water_mask: mask,
      water_rings: Some(vec![]),
      labels,
      n_landmasses: n,
      lasso: None,
      lasso_ring: None,
      sea: Default::default(),
    }
  }

  #[test]
  fn a_rising_sea_splits_the_land_and_going_back_restores_it() {
    let mut c = two_hills(true);
    let fetched = c.data.clone();
    assert_eq!(c.n_landmasses, 1);
    c.lasso = Some(vec![true; c.width * c.height]);

    c.set_sea_level(Some(20.0));
    assert_eq!(c.n_landmasses, 2, "the valley floods: two islands");
    assert!(c.lasso.is_none(), "a selection doesn't survive a new coastline");
    assert!(c.current_water_rings().is_none(), "OSM never drew this coastline");

    c.set_sea_level(Some(50.0));
    assert_eq!(c.n_landmasses, 1, "only the 60 m hill is left");

    c.set_sea_level(Some(61.0));
    assert_eq!(c.n_landmasses, 0, "nothing is left");

    c.set_sea_level(None);
    assert_eq!(c.data, fetched, "back to the fetch, exactly");
    assert_eq!(c.n_landmasses, 1);
    assert!(c.current_water_rings().is_some());
  }

  #[test]
  fn montreal_after_a_30_m_rise_still_exports_watertight_islands() {
    // The real Montreal water (fixture) over a west-to-east ramp from 10 m
    // to 48 m: at +30 m the western half drowns and the Island of Montreal
    // loses its west end. What's left has to select and mesh like any other
    // landmass.
    let (w, h) = (760usize, 500usize);
    let geo = crate::water::geometry_from_overpass(&crate::test_support::montreal_fixture()).unwrap();
    let ramp: Vec<f32> = (0..w * h).map(|i| 10.0 + ((i % w) as f32) * 0.05).collect();
    let bbox = (45.3, 45.75, -74.25, -73.3);
    let (mut c, _, _) = crate::commands::build_terrain(ramp, w, h, bbox, Some(Ok(geo)));
    let at = |c: &CachedElevation, lat: f64, lon: f64| {
      let col = (((lon - bbox.2) / (bbox.3 - bbox.2)) * ((w - 1) as f64)) as usize;
      let row = (((bbox.1 - lat) / (bbox.1 - bbox.0)) * ((h - 1) as f64)) as usize;
      c.labels.as_ref().unwrap()[row * w + col]
    };
    assert_ne!(at(&c, 45.47, -73.85), 0, "the West Island, as fetched");

    c.set_sea_level(Some(30.0));
    assert_eq!(at(&c, 45.47, -73.85), 0, "the West Island is under");
    let downtown = at(&c, 45.5, -73.57);
    assert_ne!(downtown, 0, "downtown, at 37 m, isn't");
    assert_eq!(at(&c, 45.5, -73.62), downtown, "and it's still one island");
    let labels = c.labels.clone().unwrap();
    let (crop, fp, cw, ch, s, n, wst, e) = crate::selection::crop_to_landmasses(&c, &labels, &[downtown]).unwrap();
    let stl = crate::mesh::build_stl(&crop, &fp, cw, ch, 1.5, 2.0, s, n, wst, e).unwrap();
    crate::test_support::assert_watertight(&stl);
  }

  #[test]
  fn without_osm_water_the_sea_comes_in_from_the_edges() {
    // Water mask off (or Overpass down): a raised sea still makes
    // coastlines, and landmasses to click.
    let mut c = two_hills(false);
    assert!(c.labels.is_none());
    c.set_sea_level(Some(20.0));
    assert_eq!(c.n_landmasses, 2);
    c.set_sea_level(Some(0.0));
    assert!(c.labels.is_none() && c.water_mask.is_none(), "sea level 0 is the fetch");
  }

  /// Wet set at `sea_level` the slow way: flood-fill from every seed
  /// through pixels at or below the level.
  fn brute_force(elev: &[f32], water: &[bool], w: usize, h: usize, sea_level: f32) -> Vec<bool> {
    let mut wet = water.to_vec();
    let mut stack: Vec<usize> = (0..w * h)
      .filter(|&i| {
        let (r, c) = (i / w, i % w);
        water[i] || ((r == 0 || c == 0 || r + 1 == h || c + 1 == w) && elev[i] <= sea_level)
      })
      .collect();
    for &i in &stack {
      wet[i] = true;
    }
    while let Some(i) = stack.pop() {
      let (r, c) = (i / w, i % w);
      let mut visit = |j: usize| {
        if !wet[j] && elev[j] <= sea_level {
          wet[j] = true;
          stack.push(j);
        }
      };
      if c > 0 {
        visit(i - 1);
      }
      if c + 1 < w {
        visit(i + 1);
      }
      if r > 0 {
        visit(i - w);
      }
      if r + 1 < h {
        visit(i + w);
      }
    }
    wet
  }

  #[test]
  fn basin_behind_a_ridge_stays_dry_until_the_water_tops_it() {
    // West to east: coast at 0 m, a 50 m ridge, a basin at 10 m, high
    // ground at 80 m. The basin is lower than the ridge but can only be
    // reached over it.
    let row = [0.0f32, 50.0, 10.0, 10.0, 80.0];
    let (w, h) = (row.len(), 3usize);
    let elev: Vec<f32> = (0..w * h).map(|i| if i / w == 1 { row[i % w] } else { 100.0 }).collect();
    let water = vec![false; w * h];
    let levels = flood_levels(&elev, &water, w, h);
    let mid = |c: usize| levels[w + c];
    assert_eq!(mid(0), 0.0, "the coast floods first");
    assert_eq!(mid(2), 50.0, "the basin floods when the ridge does");
    assert_eq!(mid(3), 50.0);
    assert!(!flooded_mask(&water, &levels, 30.0)[w + 2], "dry at +30 m");
    assert!(flooded_mask(&water, &levels, 50.0)[w + 2], "wet at +50 m");
  }

  #[test]
  fn rivers_carry_the_flood_inland() {
    // 100 m country with a river channel cut in from the west edge, and
    // 20 m banks along it: the banks flood at 20 m via the river, long
    // before the sea could come overland.
    let (w, h) = (7usize, 5usize);
    let mut elev = vec![100.0f32; w * h];
    let mut water = vec![false; w * h];
    for c in 0..5 {
      water[2 * w + c] = true;
      elev[w + c] = 20.0;
      elev[3 * w + c] = 20.0;
    }
    let levels = flood_levels(&elev, &water, w, h);
    assert_eq!(levels[2 * w + 3], f32::NEG_INFINITY, "the river is always wet");
    assert_eq!(levels[w + 4], 20.0);
    assert_eq!(levels[3 * w + 4], 20.0);
    assert_eq!(levels[w + 6], 100.0, "the far bank needs the whole country to drown");
  }

  #[test]
  fn flood_levels_match_brute_force_flood_fills() {
    // Random terrain with a few lakes: thresholding the levels must agree
    // with a direct flood-fill at every elevation in the map.
    let mut seed = 0x9e37_79b9u32;
    let mut rand = || {
      seed ^= seed << 13;
      seed ^= seed >> 17;
      seed ^= seed << 5;
      seed
    };
    for _ in 0..20 {
      let (w, h) = (9usize, 7usize);
      let elev: Vec<f32> = (0..w * h).map(|_| (rand() % 40) as f32).collect();
      let water: Vec<bool> = (0..w * h).map(|_| rand() % 9 == 0).collect();
      let levels = flood_levels(&elev, &water, w, h);
      let mut thresholds = elev.clone();
      thresholds.push(-1.0);
      for &t in &thresholds {
        assert_eq!(flooded_mask(&water, &levels, t), brute_force(&elev, &water, w, h, t), "sea level {t}");
      }
    }
  }
}
