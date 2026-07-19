use crate::grid::rasterize_rings;

/// Water polygon rings belonging to one OSM element (way or multipolygon
/// relation). Each group must be filled with the even-odd rule on its own
/// and ORed with the rest — one even-odd pass over all elements would make
/// overlapping polygons cancel out.
pub(crate) type RingGroup = Vec<Vec<[f64; 2]>>;

/// Fetch water polygon rings from Overpass, grouped per element.
/// Retries across mirror endpoints; errors propagate so the caller can
/// surface them instead of silently rendering unmasked terrain.
pub(crate) async fn fetch_water_rings(
  south: f64,
  north: f64,
  west: f64,
  east: f64
) -> Result<Vec<RingGroup>, String> {
  const ENDPOINTS: [&str; 3] = [
    "https://overpass-api.de/api/interpreter",
    "https://maps.mail.ru/osm/tools/overpass/api/interpreter",
    "https://overpass.kumi.systems/api/interpreter",
  ];
  const ATTEMPTS: usize = 4;

  let mut last_err = String::new();
  for attempt in 0..ATTEMPTS {
    let endpoint = ENDPOINTS[attempt % ENDPOINTS.len()];
    match try_fetch_water_rings(endpoint, south, north, west, east).await {
      Ok(rings) => {
        return Ok(rings);
      }
      Err(e) => {
        last_err = e;
      }
    }
    if attempt + 1 < ATTEMPTS {
      tokio::time::sleep(std::time::Duration::from_secs(2 * ((attempt as u64) + 1))).await;
    }
  }
  Err(format!("Overpass water query failed after {ATTEMPTS} attempts: {last_err}"))
}

/// Stitch a collection of way segments (each a Vec of [lat,lon] points) into
/// one or more closed rings by chaining them end-to-end. Returns each closed
/// ring as a Vec<[f64;2]>. Segments are consumed in order and flipped if needed
/// to make endpoints match.
fn assemble_rings(mut segments: Vec<Vec<[f64; 2]>>) -> Vec<Vec<[f64; 2]>> {
  let close_enough = |a: [f64; 2], b: [f64; 2]| {
    (a[0] - b[0]).abs() < 1e-7 && (a[1] - b[1]).abs() < 1e-7
  };

  let mut rings: Vec<Vec<[f64; 2]>> = Vec::new();

  while !segments.is_empty() {
    let mut ring = segments.remove(0);
    let mut changed = true;
    while changed {
      changed = false;
      // If already closed, stop
      if ring.len() >= 3 && close_enough(*ring.first().unwrap(), *ring.last().unwrap()) {
        break;
      }
      let tail = *ring.last().unwrap();
      let mut found = None;
      for (i, seg) in segments.iter().enumerate() {
        if close_enough(tail, seg[0]) {
          found = Some((i, false));
          break;
        }
        if close_enough(tail, *seg.last().unwrap()) {
          found = Some((i, true));
          break;
        }
      }
      if let Some((i, flip)) = found {
        let mut seg = segments.remove(i);
        if flip {
          seg.reverse();
        }
        seg.remove(0); // drop duplicate junction point
        ring.extend(seg);
        changed = true;
      }
    }
    if ring.len() >= 3 {
      rings.push(ring);
    }
  }
  rings
}

fn member_segments(members: &[serde_json::Value], role: &str) -> Vec<Vec<[f64; 2]>> {
  members
    .iter()
    .filter(|m| m["role"].as_str() == Some(role))
    .filter_map(|m| {
      let geom = m["geometry"].as_array()?;
      let pts: Vec<[f64; 2]> = geom
        .iter()
        .filter_map(|g| Some([g["lat"].as_f64()?, g["lon"].as_f64()?]))
        .collect();
      if pts.len() >= 2 {
        Some(pts)
      } else {
        None
      }
    })
    .collect()
}

async fn try_fetch_water_rings(
  endpoint: &str,
  south: f64,
  north: f64,
  west: f64,
  east: f64
) -> Result<Vec<RingGroup>, String> {
  // `natural=coastline` is intentionally excluded: OSM coastlines are open ways
  // with special winding semantics, not closeable polygons.
  // `waterway=riverbank` is the legacy tag; `natural=water` covers modern usage.
  let query = format!(
    "[out:json][timeout:60];\
        (\
          way[\"natural\"=\"water\"]({south},{west},{north},{east});\
          way[\"waterway\"=\"riverbank\"]({south},{west},{north},{east});\
          relation[\"natural\"=\"water\"]({south},{west},{north},{east});\
          relation[\"waterway\"=\"riverbank\"]({south},{west},{north},{east});\
        );\
        out geom;"
  );

  let client = reqwest::Client
    ::builder()
    .timeout(std::time::Duration::from_secs(75))
    .user_agent(concat!("landgrep/", env!("CARGO_PKG_VERSION")))
    .build()
    .map_err(|e| e.to_string())?;

  let resp = client
    .get(endpoint)
    .query(&[("data", &query)])
    .header("Accept", "application/json")
    .send().await
    .map_err(|e| e.to_string())?;

  // Judge the response by its content, not its HTTP status: overpass-api.de
  // has been observed returning 406 while streaming a complete, valid payload.
  // Real failures (rate limit, gateway errors) don't parse as JSON.
  let status = resp.status();
  let text = resp.text().await.map_err(|e| e.to_string())?;
  let body: serde_json::Value = serde_json
    ::from_str(&text)
    .map_err(|_| {
      let snippet: String = text.chars().take(200).collect();
      format!("HTTP {status}, non-JSON response: {snippet}")
    })?;

  rings_from_overpass(&body)
}

/// Extract per-element ring groups from an Overpass response. Keeping the
/// element grouping (rather than one flat ring list) preserves the fill
/// semantics: even-odd within an element, union across elements.
pub(crate) fn rings_from_overpass(body: &serde_json::Value) -> Result<Vec<RingGroup>, String> {
  // Overpass reports rate limiting / mid-stream timeouts as a `remark` on an
  // otherwise valid (but truncated) response. Truncation silently drops the
  // large relations — exactly the big rivers and lakes — so treat it as failure.
  if let Some(remark) = body["remark"].as_str() {
    return Err(format!("Overpass remark: {remark}"));
  }

  let mut groups: Vec<RingGroup> = Vec::new();

  let elements = body["elements"].as_array().ok_or("no elements")?;
  for el in elements {
    match el["type"].as_str() {
      Some("way") => {
        if let Some(geom) = el["geometry"].as_array() {
          let ring: Vec<[f64; 2]> = geom
            .iter()
            .filter_map(|g| Some([g["lat"].as_f64()?, g["lon"].as_f64()?]))
            .collect();
          if ring.len() >= 3 {
            groups.push(vec![ring]);
          }
        }
      }
      Some("relation") => {
        // Assemble outer segments into closed rings before rasterizing —
        // rasterizing each open segment independently produces wrong fills.
        // Inner rings participate in the same even-odd pass so islands
        // stay land instead of being flooded.
        if let Some(members) = el["members"].as_array() {
          let mut rings = assemble_rings(member_segments(members, "outer"));
          rings.extend(assemble_rings(member_segments(members, "inner")));
          if !rings.is_empty() {
            groups.push(rings);
          }
        }
      }
      _ => {}
    }
  }

  Ok(groups)
}

/// Rasterize ring groups onto a grid: even-odd fill per group, union across
/// groups. Resolution-agnostic — the same rings serve the DEM-res water mask
/// and the supersampled export footprint.
pub(crate) fn rasterize_ring_groups(
  groups: &[RingGroup],
  south: f64,
  north: f64,
  west: f64,
  east: f64,
  width: usize,
  height: usize
) -> Vec<bool> {
  let mut mask = vec![false; width * height];
  for g in groups {
    rasterize_rings(g, south, north, west, east, width, height, &mut mask);
  }
  mask
}

/// Rings + rasterization in one step. Production code keeps the two stages
/// separate (rings are cached for export-resolution re-rasterization);
/// fixture tests pin this composition's output.
#[cfg(test)]
pub(crate) fn mask_from_overpass(
  body: &serde_json::Value,
  south: f64,
  north: f64,
  west: f64,
  east: f64,
  width: usize,
  height: usize
) -> Result<Vec<bool>, String> {
  Ok(rasterize_ring_groups(&rings_from_overpass(body)?, south, north, west, east, width, height))
}

/// Flatten masked pixels to just below the lowest LAND elevation, not to 0 —
/// zeroing gouges a crater wherever terrain sits well above sea level
/// (e.g. a 230 m reservoir) and makes coastal noise indistinguishable.
/// Returns the level used, or None when the mask is empty or all-water.
pub(crate) fn flatten_water(data: &mut [f32], mask: &[bool]) -> Option<f32> {
  let water_px = mask
    .iter()
    .filter(|&&w| w)
    .count();
  if water_px == 0 || water_px == mask.len() {
    return None;
  }

  let mut land_min = f32::INFINITY;
  let mut land_max = f32::NEG_INFINITY;
  for (i, &is_water) in mask.iter().enumerate() {
    if !is_water {
      land_min = land_min.min(data[i]);
      land_max = land_max.max(data[i]);
    }
  }
  let gap = ((land_max - land_min) * 0.02).max(1.0);
  let level = land_min - gap;
  for (i, &is_water) in mask.iter().enumerate() {
    if is_water {
      data[i] = level;
    }
  }
  Some(level)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::test_support::montreal_fixture;

  #[test]
  fn mask_matches_fixture() {
    let body = montreal_fixture();
    let mask = mask_from_overpass(&body, 45.30, 45.75, -74.25, -73.3, 760, 500).unwrap();
    let water = mask
      .iter()
      .filter(|&&w| w)
      .count();
    // Reference count from an independent Python implementation of the same
    // even-odd fill over the identical fixture.
    assert_eq!(water, 54984);

    // The Island of Montreal must be isolated: mid-island is land while the
    // St. Lawrence (S/E), Rivière des Prairies (N) and Lac Saint-Louis (SW)
    // are all water.
    let px = |lat: f64, lon: f64| {
      let col = ((lon - -74.25) / (-73.3 - -74.25)) * 759.0;
      let row = ((45.75 - lat) / (45.75 - 45.30)) * 499.0;
      mask[(row as usize) * 760 + (col as usize)]
    };
    assert!(!px(45.50, -73.62), "mid-island should be land");
    assert!(px(45.47, -73.52), "St. Lawrence should be water");
    assert!(px(45.55, -73.68), "Rivière des Prairies should be water");
    assert!(px(45.40, -73.80), "Lac Saint-Louis should be water");
  }

  #[test]
  fn truncated_response_is_rejected() {
    let body =
      serde_json::json!({
      "remark": "runtime error: Query timed out in \"query\" at line 1",
      "elements": []
    });
    assert!(mask_from_overpass(&body, 0.0, 1.0, 0.0, 1.0, 10, 10).is_err());
  }

  #[test]
  fn assemble_rings_chains_and_flips_segments() {
    // A square split into three segments, middle one reversed, given out of
    // chaining order.
    let segments = vec![
      vec![[0.0, 0.0], [0.0, 1.0]],
      vec![[1.0, 1.0], [1.0, 0.0], [0.0, 0.0]], // closing segment ahead of its turn
      vec![[1.0, 1.0], [0.0, 1.0]] // needs flipping to chain onto the tail
    ];
    let rings = assemble_rings(segments);
    assert_eq!(rings.len(), 1);
    let ring = &rings[0];
    assert_eq!(ring.first(), ring.last(), "ring should close");
    assert_eq!(ring.len(), 5);
  }

  #[test]
  fn assemble_rings_separates_disjoint_rings() {
    let segments = vec![
      vec![[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [0.0, 0.0]],
      vec![[5.0, 5.0], [5.0, 6.0], [6.0, 6.0], [5.0, 5.0]]
    ];
    assert_eq!(assemble_rings(segments).len(), 2);
  }

  #[test]
  fn flatten_water_sits_below_land_minimum() {
    // Reservoir scenario: land 197–289 m, water surface ~232 m. Flattening
    // must land just below 197, never at 0.
    let mut data = vec![197.0, 289.0, 232.0, 231.5];
    let mask = vec![false, false, true, true];
    let level = flatten_water(&mut data, &mask).unwrap();
    let expected_gap = ((289.0f32 - 197.0) * 0.02).max(1.0);
    assert_eq!(level, 197.0 - expected_gap);
    assert_eq!(data, vec![197.0, 289.0, level, level]);
  }

  #[test]
  fn flatten_water_degenerate_masks_are_noops() {
    let mut data = vec![5.0, 10.0];
    assert_eq!(flatten_water(&mut data, &[false, false]), None);
    assert_eq!(flatten_water(&mut data, &[true, true]), None);
    assert_eq!(data, vec![5.0, 10.0]);
  }

  /// Live network check: set LANDGREP_LIVE_OVERPASS=1 to exercise the real
  /// endpoint/retry path against a small bbox over Lac Saint-Louis.
  #[test]
  fn live_overpass_fetch() {
    if std::env::var("LANDGREP_LIVE_OVERPASS").is_err() {
      return;
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let rings = rt.block_on(fetch_water_rings(45.38, 45.42, -73.85, -73.8)).unwrap();
    let mask = rasterize_ring_groups(&rings, 45.38, 45.42, -73.85, -73.8, 100, 100);
    let water = mask
      .iter()
      .filter(|&&w| w)
      .count();
    assert!(water > 1000, "expected mostly water over Lac Saint-Louis, got {water} px");
  }

  #[test]
  fn overlapping_elements_fill_union_not_xor() {
    // Two overlapping lakes as separate ways: per-element grouping must OR
    // them; a single even-odd pass would carve a hole out of the overlap.
    let body =
      serde_json::json!({
      "elements": [
        {"type": "way", "geometry": [
          {"lat": 0.1, "lon": 0.1}, {"lat": 0.1, "lon": 0.6},
          {"lat": 0.6, "lon": 0.6}, {"lat": 0.6, "lon": 0.1}
        ]},
        {"type": "way", "geometry": [
          {"lat": 0.4, "lon": 0.4}, {"lat": 0.4, "lon": 0.9},
          {"lat": 0.9, "lon": 0.9}, {"lat": 0.9, "lon": 0.4}
        ]}
      ]
    });
    let mask = mask_from_overpass(&body, 0.0, 1.0, 0.0, 1.0, 100, 100).unwrap();
    let at = |lat: f64, lon: f64| mask[(((1.0 - lat) * 99.0) as usize) * 100 + ((lon * 99.0) as usize)];
    assert!(at(0.5, 0.5), "overlap must stay water");
    assert!(at(0.2, 0.2), "first lake");
    assert!(at(0.8, 0.8), "second lake");
    assert!(!at(0.05, 0.9), "outside both");
  }
}
