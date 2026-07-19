use serde::{ Deserialize, Serialize };
use std::io::Cursor;
use std::sync::Mutex;
use tiff::decoder::{ Decoder, DecodingResult };

struct CachedElevation {
  data: Vec<f32>,
  width: usize,
  height: usize,
  #[allow(unused)]
  min_elev: f32,
  #[allow(unused)]
  max_elev: f32,
  south: f64,
  north: f64,
  west: f64,
  east: f64,
  water_mask: Option<Vec<bool>>,
  /// Connected-component id per pixel: 0 = water, 1..=n = landmass.
  labels: Option<Vec<u32>>,
  n_landmasses: u32,
  /// Freehand lasso selection (drawn polygon ∩ land) at full resolution.
  lasso: Option<Vec<bool>>,
}

pub struct ElevationState(Mutex<Option<CachedElevation>>);

#[derive(Debug, Serialize, Deserialize)]
pub struct Heightmap {
  pub data: Vec<f32>,
  pub width: usize,
  pub height: usize,
  #[serde(rename = "minElev")]
  pub min_elev: f32,
  #[serde(rename = "maxElev")]
  pub max_elev: f32,
  /// Elevation all water pixels were flattened to; None when masking was off,
  /// failed, or found no water in the bbox.
  #[serde(rename = "waterLevel")]
  pub water_level: Option<f32>,
  #[serde(rename = "waterMaskError")]
  pub water_mask_error: Option<String>,
  /// Number of connected land components; None when no mask was built.
  #[serde(rename = "landmassCount")]
  pub landmass_count: Option<u32>,
}

fn downsample<T: Copy>(src: &[T], sw: usize, sh: usize, max_dim: usize) -> (Vec<T>, usize, usize) {
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
fn upsample_bilinear(src: &[f32], w: usize, h: usize, factor: usize) -> (Vec<f32>, usize, usize) {
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
fn box_blur(data: &mut [f32], w: usize, h: usize, radius: usize) {
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
fn smooth_upsample(
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

fn parse_geotiff(bytes: Vec<u8>) -> Result<(Vec<f32>, usize, usize), String> {
  let cursor = Cursor::new(bytes);
  let mut dec = Decoder::new(cursor)
    .map_err(|e| e.to_string())?
    .with_limits(tiff::decoder::Limits::unlimited());

  let (w, h) = dec.dimensions().map_err(|e| e.to_string())?;
  let image = dec.read_image().map_err(|e| e.to_string())?;

  let mut data: Vec<f32> = match image {
    DecodingResult::F32(v) => v,
    DecodingResult::F64(v) =>
      v
        .into_iter()
        .map(|x| x as f32)
        .collect(),
    DecodingResult::I16(v) =>
      v
        .into_iter()
        .map(|x| if x == -32768 { 0.0 } else { x as f32 })
        .collect(),
    DecodingResult::U16(v) =>
      v
        .into_iter()
        .map(|x| x as f32)
        .collect(),
    DecodingResult::I32(v) =>
      v
        .into_iter()
        .map(|x| x as f32)
        .collect(),
    DecodingResult::U32(v) =>
      v
        .into_iter()
        .map(|x| x as f32)
        .collect(),
    DecodingResult::U8(v) =>
      v
        .into_iter()
        .map(|x| x as f32)
        .collect(),
    _ => {
      return Err("Unsupported TIFF sample format".into());
    }
  };

  for v in data.iter_mut() {
    if *v < -1000.0 {
      *v = 0.0;
    }
  }

  Ok((data, w as usize, h as usize))
}

/// Scanline-fill the interior of a polygon into `mask` using the even-odd rule
/// across ALL rings at once, so inner rings (islands) become holes instead of
/// being flooded.
fn rasterize_rings(
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

/// Fetch water polygons from Overpass and return a pixel mask.
/// Retries across mirror endpoints; errors propagate so the caller can
/// surface them instead of silently rendering unmasked terrain.
async fn build_water_mask(
  south: f64,
  north: f64,
  west: f64,
  east: f64,
  width: usize,
  height: usize
) -> Result<Vec<bool>, String> {
  const ENDPOINTS: [&str; 3] = [
    "https://overpass-api.de/api/interpreter",
    "https://maps.mail.ru/osm/tools/overpass/api/interpreter",
    "https://overpass.kumi.systems/api/interpreter",
  ];
  const ATTEMPTS: usize = 4;

  let mut last_err = String::new();
  for attempt in 0..ATTEMPTS {
    let endpoint = ENDPOINTS[attempt % ENDPOINTS.len()];
    match try_build_water_mask(endpoint, south, north, west, east, width, height).await {
      Ok(mask) => {
        return Ok(mask);
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

async fn try_build_water_mask(
  endpoint: &str,
  south: f64,
  north: f64,
  west: f64,
  east: f64,
  width: usize,
  height: usize
) -> Result<Vec<bool>, String> {
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

  mask_from_overpass(&body, south, north, west, east, width, height)
}

fn mask_from_overpass(
  body: &serde_json::Value,
  south: f64,
  north: f64,
  west: f64,
  east: f64,
  width: usize,
  height: usize
) -> Result<Vec<bool>, String> {
  // Overpass reports rate limiting / mid-stream timeouts as a `remark` on an
  // otherwise valid (but truncated) response. Truncation silently drops the
  // large relations — exactly the big rivers and lakes — so treat it as failure.
  if let Some(remark) = body["remark"].as_str() {
    return Err(format!("Overpass remark: {remark}"));
  }

  let mut mask = vec![false; width * height];

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
            rasterize_rings(&[ring], south, north, west, east, width, height, &mut mask);
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
          rasterize_rings(&rings, south, north, west, east, width, height, &mut mask);
        }
      }
      _ => {}
    }
  }

  Ok(mask)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Real Overpass response for the Montreal region (bbox 45.30,-74.25 →
  /// 45.75,-73.30, fetched 2026-07-13): 1697 ways + 107 relations including
  /// multipolygons with inner rings (islands).
  fn montreal_fixture() -> serde_json::Value {
    use std::io::Read;
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/overpass_montreal.json.gz");
    let file = std::fs::File::open(path).unwrap();
    let mut json = String::new();
    flate2::read::GzDecoder::new(file).read_to_string(&mut json).unwrap();
    serde_json::from_str(&json).unwrap()
  }

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

  #[test]
  fn montreal_island_is_its_own_landmass() {
    let body = montreal_fixture();
    let mask = mask_from_overpass(&body, 45.30, 45.75, -74.25, -73.3, 760, 500).unwrap();
    let (labels, n) = label_landmasses(&mask, 760, 500);
    assert!(n >= 3, "expected at least island/Laval/shores, got {n}");
    let at = |lat: f64, lon: f64| {
      let col = (((lon - -74.25) / 0.95) * 759.0) as usize;
      let row = (((45.75 - lat) / 0.45) * 499.0) as usize;
      labels[row * 760 + col]
    };
    let island = at(45.5, -73.62); // Island of Montreal
    let laval = at(45.58, -73.74); // Île Jésus, across Rivière des Prairies
    let south_shore = at(45.53, -73.42); // Longueuil, across the St. Lawrence
    assert!(island != 0 && laval != 0 && south_shore != 0, "all three should be land");
    assert_ne!(island, laval);
    assert_ne!(island, south_shore);
  }

  /// 2-manifold check: every directed edge appears exactly once with its
  /// reverse present (no holes, no duplicate faces), all floats finite, and
  /// enclosed volume positive (outward normals).
  fn assert_watertight(stl: &[u8]) {
    use std::collections::HashMap;
    let n = u32::from_le_bytes(stl[80..84].try_into().unwrap()) as usize;
    assert_eq!(stl.len(), 84 + n * 50);
    let vert = |t: usize, k: usize| -> [f32; 3] {
      let off = 84 + t * 50 + 12 + k * 12;
      let f = |o: usize| f32::from_le_bytes(stl[o..o + 4].try_into().unwrap());
      [f(off), f(off + 4), f(off + 8)]
    };
    let key = |v: [f32; 3]| (v[0].to_bits(), v[1].to_bits(), v[2].to_bits());

    let mut edges: HashMap<_, i32> = HashMap::new();
    let mut vol6 = 0f64;
    for t in 0..n {
      let vs = [vert(t, 0), vert(t, 1), vert(t, 2)];
      for v in vs.iter().flatten() {
        assert!(v.is_finite(), "non-finite coordinate in triangle {t}");
      }
      for e in 0..3 {
        *edges.entry((key(vs[e]), key(vs[(e + 1) % 3]))).or_insert(0) += 1;
      }
      let [a, b, c] = vs.map(|v| v.map(|x| x as f64));
      vol6 +=
        a[0] * (b[1] * c[2] - b[2] * c[1]) -
        a[1] * (b[0] * c[2] - b[2] * c[0]) +
        a[2] * (b[0] * c[1] - b[1] * c[0]);
    }
    for (&(a, b), &cnt) in &edges {
      assert_eq!(cnt, 1, "directed edge repeated — overlapping faces");
      assert_eq!(edges.get(&(b, a)), Some(&1), "unmatched edge — hole in mesh");
    }
    assert!(vol6 > 0.0, "non-positive volume — normals inverted");
  }

  #[test]
  fn full_rect_stl_is_watertight() {
    let data = vec![10.0, 12.0, 11.0, 10.5, 13.0, 12.5, 10.0, 11.0, 14.0];
    let (w, h) = (3usize, 3usize);
    let footprint = vec![true; w * h];
    let stl = build_stl(&data, &footprint, w, h, 1.5, 2.0, 45.0, 45.1, -73.1, -73.0).unwrap();
    let n_tris = 4 * (w - 1) * (h - 1) + 4 * (w - 1) + 4 * (h - 1);
    assert_eq!(u32::from_le_bytes(stl[80..84].try_into().unwrap()) as usize, n_tris);
    assert_watertight(&stl);
  }

  #[test]
  fn footprint_stl_is_clipped_and_watertight() {
    // plus-shaped footprint on a 7×7 grid
    let (w, h) = (7usize, 7usize);
    let mut footprint = vec![false; w * h];
    for r in 0..h {
      for c in 0..w {
        if (2..=4).contains(&r) || (2..=4).contains(&c) {
          footprint[r * w + c] = true;
        }
      }
    }
    let data: Vec<f32> = (0..w * h).map(|i| 100.0 + ((i % 7) as f32)).collect();
    let stl = build_stl(&data, &footprint, w, h, 1.0, 1.0, 45.0, 45.1, -73.1, -73.0).unwrap();
    assert_watertight(&stl);
    // clipped: fewer top cells than the full 6×6 grid would produce
    let n = u32::from_le_bytes(stl[80..84].try_into().unwrap()) as usize;
    assert!(n < 4 * (w - 1) * (h - 1) + 4 * (w - 1) + 4 * (h - 1));
  }

  #[test]
  fn empty_footprint_is_an_error() {
    let data = vec![1.0f32; 9];
    assert!(build_stl(&data, &[false; 9], 3, 3, 1.0, 1.0, 45.0, 45.1, -73.1, -73.0).is_err());
  }

  fn two_block_cache() -> (CachedElevation, Vec<u32>) {
    // 8×8: land block A rows/cols 1–2, land block B rows/cols 5–6, water else.
    let (w, h) = (8usize, 8usize);
    let mut water = vec![true; w * h];
    let mut data = vec![0.0f32; w * h];
    for r in 1..=2 {
      for c in 1..=2 {
        water[r * w + c] = false;
        data[r * w + c] = 100.0;
      }
    }
    for r in 5..=6 {
      for c in 5..=6 {
        water[r * w + c] = false;
        data[r * w + c] = 200.0;
      }
    }
    let (labels, n) = label_landmasses(&water, w, h);
    assert_eq!(n, 2);
    let cache = CachedElevation {
      data,
      width: w,
      height: h,
      min_elev: 0.0,
      max_elev: 200.0,
      south: 45.0,
      north: 46.0,
      west: -74.0,
      east: -73.0,
      water_mask: Some(water),
      labels: Some(labels.clone()),
      n_landmasses: n,
      lasso: None,
    };
    (cache, labels)
  }

  #[test]
  fn crop_single_landmass_footprint() {
    let (cache, labels) = two_block_cache();
    let id_a = labels[1 * 8 + 1];
    let (crop, footprint, cw, ch, s, n_, wst, e) = crop_to_landmasses(
      &cache,
      &labels,
      &[id_a]
    ).unwrap();
    // bbox rows/cols 1–2 with margin 1 → 0..=3 → 4×4
    assert_eq!((cw, ch), (4, 4));
    assert_eq!(
      footprint
        .iter()
        .filter(|&&f| f)
        .count(),
      4
    );
    // footprint pixels carry landmass A elevations, untouched
    for i in 0..crop.len() {
      if footprint[i] {
        assert_eq!(crop[i], 100.0);
      }
    }
    assert!(s < n_ && wst < e && s >= 45.0 && n_ <= 46.0 && wst >= -74.0 && e <= -73.0);
    // and the footprint meshes into a watertight solid
    let stl = build_stl(&crop, &footprint, cw, ch, 1.0, 1.0, s, n_, wst, e).unwrap();
    assert_watertight(&stl);
  }

  #[test]
  fn crop_multiple_landmasses_meshes_both() {
    let (cache, labels) = two_block_cache();
    let (id_a, id_b) = (labels[1 * 8 + 1], labels[5 * 8 + 5]);
    let (crop, footprint, cw, ch, s, n_, wst, e) = crop_to_landmasses(
      &cache,
      &labels,
      &[id_a, id_b]
    ).unwrap();
    assert_eq!((cw, ch), (8, 8)); // union bbox spans both blocks
    assert_eq!(
      footprint
        .iter()
        .filter(|&&f| f)
        .count(),
      8
    );
    let stl = build_stl(&crop, &footprint, cw, ch, 1.0, 1.0, s, n_, wst, e).unwrap();
    // two disjoint solids, each closed
    assert_watertight(&stl);
  }

  #[test]
  fn lasso_clips_to_land_and_meshes() {
    let (cache, _) = two_block_cache();
    // Top-left quadrant of the 45–46 / -74–-73 bbox: covers land block A
    // (rows/cols 1–2) plus surrounding water, which must be dropped.
    let ring = vec![[46.0, -74.0], [46.0, -73.5], [45.5, -73.5], [45.5, -74.0]];
    let keep = lasso_footprint(&cache, ring).unwrap();
    assert_eq!(
      keep
        .iter()
        .filter(|&&k| k)
        .count(),
      4
    );
    let (crop, footprint, cw, ch, s, n_, wst, e) = crop_to_footprint(&cache, &keep).unwrap();
    let stl = build_stl(&crop, &footprint, cw, ch, 1.0, 1.0, s, n_, wst, e).unwrap();
    assert_watertight(&stl);
  }

  #[test]
  fn lasso_splits_a_single_landmass() {
    // One connected 2×6 land strip; the lasso takes only its left half —
    // the manual-split case connected-component labeling can't express.
    let (w, h) = (8usize, 8usize);
    let mut water = vec![true; w * h];
    let mut data = vec![0.0f32; w * h];
    for r in 1..=2 {
      for c in 1..=6 {
        water[r * w + c] = false;
        data[r * w + c] = 100.0;
      }
    }
    let (labels, n) = label_landmasses(&water, w, h);
    assert_eq!(n, 1);
    let cache = CachedElevation {
      data,
      width: w,
      height: h,
      min_elev: 0.0,
      max_elev: 100.0,
      south: 45.0,
      north: 46.0,
      west: -74.0,
      east: -73.0,
      water_mask: Some(water),
      labels: Some(labels),
      n_landmasses: n,
      lasso: None,
    };
    // rows 0–2, cols 0–3 of the raster → strip pixels rows 1–2 × cols 1–3
    let ring = vec![[46.0, -74.0], [46.0, -73.5], [45.6, -73.5], [45.6, -74.0]];
    let keep = lasso_footprint(&cache, ring).unwrap();
    assert_eq!(
      keep
        .iter()
        .filter(|&&k| k)
        .count(),
      6
    );
    for (i, &k) in keep.iter().enumerate() {
      if k {
        let (row, col) = (i / w, i % w);
        assert!((1..=2).contains(&row) && (1..=3).contains(&col));
      }
    }
  }

  #[test]
  fn lasso_without_water_mask_keeps_whole_polygon() {
    let (w, h) = (8usize, 8usize);
    let cache = CachedElevation {
      data: vec![50.0; w * h],
      width: w,
      height: h,
      min_elev: 50.0,
      max_elev: 50.0,
      south: 45.0,
      north: 46.0,
      west: -74.0,
      east: -73.0,
      water_mask: None,
      labels: None,
      n_landmasses: 0,
      lasso: None,
    };
    let ring = vec![[46.0, -74.0], [46.0, -73.5], [45.6, -73.5], [45.6, -74.0]];
    let keep = lasso_footprint(&cache, ring).unwrap();
    // rows 0–2 × cols 0–3, nothing clipped away
    assert_eq!(
      keep
        .iter()
        .filter(|&&k| k)
        .count(),
      12
    );
  }

  #[test]
  fn degenerate_lassos_are_errors() {
    let (cache, _) = two_block_cache();
    assert!(lasso_footprint(&cache, vec![[45.5, -73.5], [45.6, -73.6]]).is_err());
    // water-only region
    let ring = vec![[46.0, -74.0], [46.0, -73.9], [45.95, -73.9], [45.95, -74.0]];
    assert!(lasso_footprint(&cache, ring).is_err());
  }

  #[test]
  fn montreal_island_stl_is_watertight() {
    let body = montreal_fixture();
    let mask = mask_from_overpass(&body, 45.30, 45.75, -74.25, -73.3, 760, 500).unwrap();
    let (labels, _) = label_landmasses(&mask, 760, 500);
    let island = labels[((0.25 / 0.45) * 499.0) as usize * 760 + ((0.63 / 0.95) * 759.0) as usize];
    assert_ne!(island, 0);
    let footprint: Vec<bool> = labels
      .iter()
      .map(|&l| l == island)
      .collect();
    let data: Vec<f32> = (0..760 * 500).map(|i| 10.0 + (((i % 760) as f32) * 0.01)).collect();
    let stl = build_stl(&data, &footprint, 760, 500, 1.0, 2.0, 45.3, 45.75, -74.25, -73.3).unwrap();
    assert_watertight(&stl);
  }

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

  /// Live network check: set LANDGREP_LIVE_OVERPASS=1 to exercise the real
  /// endpoint/retry path against a small bbox over Lac Saint-Louis.
  #[test]
  fn live_overpass_fetch() {
    if std::env::var("LANDGREP_LIVE_OVERPASS").is_err() {
      return;
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let mask = rt
      .block_on(build_water_mask(45.38, 45.42, -73.85, -73.8, 100, 100))
      .unwrap();
    let water = mask
      .iter()
      .filter(|&&w| w)
      .count();
    assert!(water > 1000, "expected mostly water over Lac Saint-Louis, got {water} px");
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

/// Flatten masked pixels to just below the lowest LAND elevation, not to 0 —
/// zeroing gouges a crater wherever terrain sits well above sea level
/// (e.g. a 230 m reservoir) and makes coastal noise indistinguishable.
/// Returns the level used, or None when the mask is empty or all-water.
fn flatten_water(data: &mut [f32], mask: &[bool]) -> Option<f32> {
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

/// Label 4-connected components of land (`!water`) pixels.
/// Returns (labels, count) where labels[i] = 0 for water, 1..=count for land.
fn label_landmasses(water: &[bool], width: usize, height: usize) -> (Vec<u32>, u32) {
  let mut labels = vec![0u32; width * height];
  let mut next = 0u32;
  let mut stack: Vec<usize> = Vec::new();

  for start in 0..water.len() {
    if water[start] || labels[start] != 0 {
      continue;
    }
    next += 1;
    labels[start] = next;
    stack.push(start);
    while let Some(i) = stack.pop() {
      let (row, col) = (i / width, i % width);
      let mut visit = |j: usize| {
        if !water[j] && labels[j] == 0 {
          labels[j] = next;
          stack.push(j);
        }
      };
      if col > 0 {
        visit(i - 1);
      }
      if col + 1 < width {
        visit(i + 1);
      }
      if row > 0 {
        visit(i - width);
      }
      if row + 1 < height {
        visit(i + width);
      }
    }
  }
  (labels, next)
}

/// Tint palette for landmasses. Deliberately excludes blues and cyans: over
/// the map's pale-blue water, a blue-tinted landmass is indistinguishable
/// from unmasked water.
const LANDMASS_PALETTE: [[u8; 3]; 8] = [
  [229, 132, 57], // orange
  [124, 179, 66], // green
  [149, 117, 205], // purple
  [224, 92, 92], // crimson
  [212, 177, 63], // gold
  [201, 107, 181], // magenta
  [163, 164, 65], // olive
  [161, 116, 91], // brown
];

/// Palette slot per landmass id (1-based), assigned by area rank so the
/// largest — typically adjacent — masses get distinct colors and modulo
/// collisions only hit small islets.
fn landmass_palette_slots(labels: &[u32], n: u32) -> Vec<usize> {
  let mut counts = vec![0usize; (n as usize) + 1];
  for &l in labels {
    counts[l as usize] += 1;
  }
  let mut ids: Vec<u32> = (1..=n).collect();
  ids.sort_by_key(|&id| std::cmp::Reverse(counts[id as usize]));
  let mut slots = vec![0usize; (n as usize) + 1];
  for (rank, &id) in ids.iter().enumerate() {
    slots[id as usize] = rank % LANDMASS_PALETTE.len();
  }
  slots
}

fn encode_png_rgba(rgba: &[u8], width: usize, height: usize) -> Result<Vec<u8>, String> {
  let mut out = Vec::new();
  {
    let mut enc = png::Encoder::new(&mut out, width as u32, height as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())?;
  }
  Ok(out)
}

// ── Commands ──────────────────────────────────────────────────────────────────

#[tauri::command]
async fn fetch_elevation(
  state: tauri::State<'_, ElevationState>,
  south: f64,
  north: f64,
  west: f64,
  east: f64,
  api_key: String,
  dem_type: String,
  preview_res: usize,
  water_mask: bool
) -> Result<Heightmap, String> {
  let url = format!(
    "https://portal.opentopography.org/API/globaldem\
         ?demtype={dem_type}\
         &south={south}&north={north}&west={west}&east={east}\
         &outputFormat=GTiff\
         &API_Key={api_key}"
  );

  let bytes = reqwest
    ::get(&url).await
    .map_err(|e| e.to_string())?
    .error_for_status()
    .map_err(|e| format!("API error: {e}"))?
    .bytes().await
    .map_err(|e| e.to_string())?;

  let (mut full_data, width, height) = parse_geotiff(bytes.to_vec())?;

  let mut water_level: Option<f32> = None;
  let mut water_mask_error: Option<String> = None;
  let mut cached_mask: Option<Vec<bool>> = None;
  let mut labels: Option<Vec<u32>> = None;
  let mut n_landmasses = 0u32;
  if water_mask {
    match build_water_mask(south, north, west, east, width, height).await {
      Ok(mask) => {
        water_level = flatten_water(&mut full_data, &mask);
        let (l, n) = label_landmasses(&mask, width, height);
        labels = Some(l);
        n_landmasses = n;
        cached_mask = Some(mask);
      }
      Err(e) => {
        water_mask_error = Some(e);
      }
    }
  }

  let min_elev = full_data.iter().cloned().fold(f32::INFINITY, f32::min);
  let max_elev = full_data.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
  let landmass_count = labels.as_ref().map(|_| n_landmasses);

  *state.0.lock().unwrap() = Some(CachedElevation {
    data: full_data.clone(),
    width,
    height,
    min_elev,
    max_elev,
    south,
    north,
    west,
    east,
    water_mask: cached_mask,
    labels,
    n_landmasses,
    lasso: None,
  });

  let cap = if preview_res == 0 { 256 } else { preview_res };
  let (preview_data, pw, ph) = downsample(&full_data, width, height, cap);

  Ok(Heightmap {
    data: preview_data,
    width: pw,
    height: ph,
    min_elev,
    max_elev,
    water_level,
    water_mask_error,
    landmass_count,
  })
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
  [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
  [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
  let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
  if len > 1e-10 {
    [v[0] / len, v[1] / len, v[2] / len]
  } else {
    [0.0, 0.0, 1.0]
  }
}

fn write_tri(buf: &mut Vec<u8>, v0: [f32; 3], v1: [f32; 3], v2: [f32; 3]) {
  let n = normalize(cross(sub(v1, v0), sub(v2, v0)));
  for f in n.iter().chain(v0.iter()).chain(v1.iter()).chain(v2.iter()) {
    buf.extend_from_slice(&f.to_le_bytes());
  }
  buf.extend_from_slice(&(0u16).to_le_bytes());
}

/// Build a watertight solid clipped to a per-vertex footprint: heightfield
/// top over footprint cells, vertical walls along the footprint boundary,
/// and a mirrored flat bottom at z=0. A grid cell is meshed when all four of
/// its corner vertices are inside the footprint, so a landmass export
/// contains that shape alone — not its bounding rectangle. Relief is
/// measured above the footprint's minimum elevation and sits on a
/// `base_mm`-thick slab.
fn build_stl(
  data: &[f32],
  footprint: &[bool],
  width: usize,
  height: usize,
  vertical_scale: f32,
  base_mm: f32,
  south: f64,
  north: f64,
  west: f64,
  east: f64
) -> Result<Vec<u8>, String> {
  let lat_center = (south + north) / 2.0;
  let lat_m = (north - south) * 110_540.0;
  let lon_m = (east - west) * 111_320.0 * ((lat_center * std::f64::consts::PI) / 180.0).cos();
  let max_m = lat_m.max(lon_m);
  let mm_per_m = 200.0 / max_m;

  let cell_x = ((lon_m * mm_per_m) / ((width - 1) as f64)) as f32;
  let cell_y = ((lat_m * mm_per_m) / ((height - 1) as f64)) as f32;

  let mut min_elev = f32::INFINITY;
  let mut max_elev = f32::NEG_INFINITY;
  for (i, &inside) in footprint.iter().enumerate() {
    if inside {
      min_elev = min_elev.min(data[i]);
      max_elev = max_elev.max(data[i]);
    }
  }
  if !min_elev.is_finite() {
    return Err("Selection contains no terrain".into());
  }
  let elev_range = max_elev - min_elev;
  let elev_scale = if elev_range > 0.0 {
    (((200.0 * mm_per_m) / (elev_range as f64)) as f32) * vertical_scale
  } else {
    1.0
  };
  let base = base_mm.max(0.4); // degenerate walls below ~one layer height

  let v = |col: usize, row: usize| -> [f32; 3] {
    [
      (col as f32) * cell_x,
      (row as f32) * cell_y,
      (data[row * width + col] - min_elev).max(0.0) * elev_scale + base,
    ]
  };
  let vb = |col: usize, row: usize| -> [f32; 3] {
    [(col as f32) * cell_x, (row as f32) * cell_y, 0.0]
  };
  // cell (col,row) spans vertices (col..col+1, row..row+1)
  let (cw, ch) = (width - 1, height - 1);
  let mut cells = vec![false; cw * ch];
  for r in 0..ch {
    for c in 0..cw {
      cells[r * cw + c] =
        footprint[r * width + c] &&
        footprint[r * width + c + 1] &&
        footprint[(r + 1) * width + c] &&
        footprint[(r + 1) * width + c + 1];
    }
  }
  // Cells that touch only at a corner would make four wall quads share one
  // vertical edge — non-manifold. Fill one of the two empty diagonal
  // neighbours until no corner-only contacts remain (additions can create
  // new contacts, hence the fixed-point loop).
  loop {
    let mut changed = false;
    for r in 0..ch - 1 {
      for c in 0..cw - 1 {
        let (a, b) = (cells[r * cw + c], cells[r * cw + c + 1]);
        let (d, e) = (cells[(r + 1) * cw + c], cells[(r + 1) * cw + c + 1]);
        if a && e && !b && !d {
          cells[r * cw + c + 1] = true;
          changed = true;
        } else if b && d && !a && !e {
          cells[r * cw + c] = true;
          changed = true;
        }
      }
    }
    if !changed {
      break;
    }
  }
  let cell = |col: isize, row: isize| -> bool {
    if col < 0 || row < 0 || (col as usize) >= cw || (row as usize) >= ch {
      return false;
    }
    cells[(row as usize) * cw + (col as usize)]
  };

  let mut buf = vec![0u8; 84]; // header + count placeholder, patched below

  for row in 0..height - 1 {
    for col in 0..width - 1 {
      if !cell(col as isize, row as isize) {
        continue;
      }
      let (bl, br) = (v(col, row), v(col + 1, row));
      let (tl, tr) = (v(col, row + 1), v(col + 1, row + 1));
      let (bbl, bbr) = (vb(col, row), vb(col + 1, row));
      let (btl, btr) = (vb(col, row + 1), vb(col + 1, row + 1));

      // top (+z) and mirrored bottom (-z)
      write_tri(&mut buf, bl, br, tl);
      write_tri(&mut buf, br, tr, tl);
      write_tri(&mut buf, bbl, btl, bbr);
      write_tri(&mut buf, bbr, btl, btr);

      // walls wherever the neighbouring cell is outside the footprint
      let (ci, ri) = (col as isize, row as isize);
      if !cell(ci, ri - 1) {
        // -y side
        write_tri(&mut buf, bbl, bbr, bl);
        write_tri(&mut buf, bbr, br, bl);
      }
      if !cell(ci, ri + 1) {
        // +y side
        write_tri(&mut buf, btr, btl, tr);
        write_tri(&mut buf, btl, tl, tr);
      }
      if !cell(ci - 1, ri) {
        // -x side
        write_tri(&mut buf, bbl, bl, btl);
        write_tri(&mut buf, btl, bl, tl);
      }
      if !cell(ci + 1, ri) {
        // +x side
        write_tri(&mut buf, bbr, btr, br);
        write_tri(&mut buf, btr, tr, br);
      }
    }
  }

  let n_tris = (buf.len() - 84) / 50;
  if n_tris == 0 {
    return Err("Selection too small to mesh at this resolution".into());
  }
  buf[80..84].copy_from_slice(&(n_tris as u32).to_le_bytes());
  Ok(buf)
}

#[tauri::command]
async fn export_stl(
  app: tauri::AppHandle,
  state: tauri::State<'_, ElevationState>,
  vertical_scale: f32,
  export_res: usize,
  base_mm: f32,
  landmasses: Option<Vec<u32>>,
  lasso: Option<bool>,
  smooth_factor: Option<usize>
) -> Result<String, String> {
  let ids = landmasses.unwrap_or_default();
  let (data, footprint, width, height, south, north, west, east) = {
    let guard = state.0.lock().unwrap();
    let c = guard.as_ref().ok_or("No terrain loaded — fetch first")?;
    if lasso.unwrap_or(false) {
      let keep = c.lasso.as_ref().ok_or("No lasso selection — draw one first")?;
      crop_to_footprint(c, keep)?
    } else if ids.is_empty() {
      let n = c.width * c.height;
      (c.data.clone(), vec![true; n], c.width, c.height, c.south, c.north, c.west, c.east)
    } else {
      let labels = c.labels
        .as_ref()
        .ok_or("No landmass data — fetch with the water mask enabled")?;
      crop_to_landmasses(c, labels, &ids)?
    }
  };

  let (mesh_data, mesh_footprint, mesh_w, mesh_h) = if export_res > 0 {
    let (d, mw, mh) = downsample(&data, width, height, export_res);
    let (f, _, _) = downsample(&footprint, width, height, export_res);
    (d, f, mw, mh)
  } else {
    (data, footprint, width, height)
  };

  // Edge smoothing: supersample the (possibly downsampled) mesh grid so
  // footprint borders stop tracing pixel edges.
  let smooth = smooth_factor.unwrap_or(1).clamp(1, 4);
  let (mesh_data, mesh_footprint, mesh_w, mesh_h) = if smooth > 1 {
    smooth_upsample(&mesh_data, &mesh_footprint, mesh_w, mesh_h, smooth)?
  } else {
    (mesh_data, mesh_footprint, mesh_w, mesh_h)
  };

  use tauri_plugin_dialog::DialogExt;
  let path = app
    .dialog()
    .file()
    .set_title("Export terrain as STL")
    .add_filter("STL file", &["stl"])
    .blocking_save_file();

  let path = match path {
    Some(p) => p.into_path().map_err(|e| e.to_string())?,
    None => {
      return Err("Cancelled".into());
    }
  };

  let stl = build_stl(
    &mesh_data,
    &mesh_footprint,
    mesh_w,
    mesh_h,
    vertical_scale,
    base_mm,
    south,
    north,
    west,
    east
  )?;
  std::fs::write(&path, stl).map_err(|e| e.to_string())?;

  Ok(path.to_string_lossy().into_owned())
}

/// Extract one or more landmasses from the cache: build a footprint of the
/// selected components and crop to it, for footprint-clipped STL meshing.
#[allow(clippy::type_complexity)]
fn crop_to_landmasses(
  c: &CachedElevation,
  labels: &[u32],
  ids: &[u32]
) -> Result<(Vec<f32>, Vec<bool>, usize, usize, f64, f64, f64, f64), String> {
  let keep: Vec<bool> = labels
    .iter()
    .map(|l| ids.contains(l))
    .collect();
  crop_to_footprint(c, &keep)
}

/// Crop the cache to the pixel bounding box of `keep` (plus margin) and
/// return the elevation crop plus the per-vertex footprint restricted to the
/// crop window.
#[allow(clippy::type_complexity)]
fn crop_to_footprint(
  c: &CachedElevation,
  keep: &[bool]
) -> Result<(Vec<f32>, Vec<bool>, usize, usize, f64, f64, f64, f64), String> {
  let (mut r0, mut r1, mut c0, mut c1) = (usize::MAX, 0usize, usize::MAX, 0usize);
  for row in 0..c.height {
    for col in 0..c.width {
      if keep[row * c.width + col] {
        r0 = r0.min(row);
        r1 = r1.max(row);
        c0 = c0.min(col);
        c1 = c1.max(col);
      }
    }
  }
  if r0 == usize::MAX {
    return Err("Selection is empty".into());
  }
  const MARGIN: usize = 1;
  r0 = r0.saturating_sub(MARGIN);
  c0 = c0.saturating_sub(MARGIN);
  r1 = (r1 + MARGIN).min(c.height - 1);
  c1 = (c1 + MARGIN).min(c.width - 1);

  let (w, h) = (c1 - c0 + 1, r1 - r0 + 1);
  if w < 2 || h < 2 {
    return Err("Selection too small to export".into());
  }

  let mut data = Vec::with_capacity(w * h);
  let mut footprint = Vec::with_capacity(w * h);
  for row in r0..=r1 {
    for col in c0..=c1 {
      let i = row * c.width + col;
      data.push(c.data[i]);
      footprint.push(keep[i]);
    }
  }

  // pixel bbox → geographic bbox (row 0 = north)
  let lat_at = |row: usize| c.north - ((row as f64) / ((c.height as f64) - 1.0)) * (c.north - c.south);
  let lon_at = |col: usize| c.west + ((col as f64) / ((c.width as f64) - 1.0)) * (c.east - c.west);
  Ok((data, footprint, w, h, lat_at(r1), lat_at(r0), lon_at(c0), lon_at(c1)))
}

#[derive(Debug, Serialize)]
pub struct LandmassInfo {
  pub id: u32,
  #[serde(rename = "areaKm2")]
  pub area_km2: f64,
}

/// Identify the landmass under a map click. Returns None over water or when
/// the point falls outside the fetched bounds.
#[tauri::command]
async fn landmass_at(
  state: tauri::State<'_, ElevationState>,
  lat: f64,
  lng: f64
) -> Result<Option<LandmassInfo>, String> {
  let guard = state.0.lock().unwrap();
  let c = guard.as_ref().ok_or("No terrain loaded — fetch first")?;
  let labels = c.labels.as_ref().ok_or("No landmass data — fetch with the water mask enabled")?;

  if lat < c.south || lat > c.north || lng < c.west || lng > c.east {
    return Ok(None);
  }
  let col = (((lng - c.west) / (c.east - c.west)) * ((c.width as f64) - 1.0)).round() as usize;
  let row = (((c.north - lat) / (c.north - c.south)) * ((c.height as f64) - 1.0)).round() as usize;
  let id = labels[row.min(c.height - 1) * c.width + col.min(c.width - 1)];
  if id == 0 {
    return Ok(None);
  }

  let px_count = labels
    .iter()
    .filter(|&&l| l == id)
    .count();
  Ok(Some(LandmassInfo { id, area_km2: ((px_count as f64) * cell_area_m2(c)) / 1.0e6 }))
}

/// Ground area of one raster cell in m².
fn cell_area_m2(c: &CachedElevation) -> f64 {
  let lat_center = (c.south + c.north) / 2.0;
  (((c.north - c.south) * 110_540.0) / (c.height as f64)) *
    (((c.east - c.west) * 111_320.0 * ((lat_center * std::f64::consts::PI) / 180.0).cos()) /
      (c.width as f64))
}

/// Rasterize a drawn lasso ring onto the cached grid and clip it to land
/// when a water mask exists. The outline only needs to be rough: water
/// inside it is dropped, so a coastline never has to be traced precisely —
/// this is also the manual escape hatch for landmasses that label as one
/// component through causeways or bridges.
fn lasso_footprint(c: &CachedElevation, points: Vec<[f64; 2]>) -> Result<Vec<bool>, String> {
  if points.len() < 3 {
    return Err("Lasso needs at least 3 points".into());
  }
  let mut mask = vec![false; c.width * c.height];
  rasterize_rings(&[points], c.south, c.north, c.west, c.east, c.width, c.height, &mut mask);
  if let Some(water) = &c.water_mask {
    for (m, &is_water) in mask.iter_mut().zip(water) {
      *m = *m && !is_water;
    }
  }
  if !mask.iter().any(|&m| m) {
    return Err("Lasso contains no land".into());
  }
  Ok(mask)
}

#[derive(Debug, Serialize)]
pub struct LassoInfo {
  #[serde(rename = "areaKm2")]
  pub area_km2: f64,
}

/// Store a freehand lasso selection (points are [lat, lng] pairs) and return
/// its land area. The mask lives in the cache until the next fetch; export
/// with `export_stl { lasso: true }`.
#[tauri::command]
async fn lasso_select(
  state: tauri::State<'_, ElevationState>,
  points: Vec<[f64; 2]>
) -> Result<LassoInfo, String> {
  let mut guard = state.0.lock().unwrap();
  let c = guard.as_mut().ok_or("No terrain loaded — fetch first")?;
  let mask = lasso_footprint(c, points)?;
  let px = mask
    .iter()
    .filter(|&&m| m)
    .count();
  let area_km2 = ((px as f64) * cell_area_m2(c)) / 1.0e6;
  c.lasso = Some(mask);
  Ok(LassoInfo { area_km2 })
}

/// Render the lasso selection as a translucent RGBA PNG (data URL): selected
/// pixels highlighted, remaining land faded back, water transparent — same
/// styling as landmass_overlay with a selection.
#[tauri::command]
async fn lasso_overlay(state: tauri::State<'_, ElevationState>) -> Result<String, String> {
  const MAX_DIM: usize = 1024;
  let (lasso, land, width, height) = {
    let guard = state.0.lock().unwrap();
    let c = guard.as_ref().ok_or("No terrain loaded — fetch first")?;
    let lasso = c.lasso.clone().ok_or("No lasso selection — draw one first")?;
    let land: Option<Vec<bool>> = c.water_mask
      .as_ref()
      .map(|w|
        w
          .iter()
          .map(|&is_water| !is_water)
          .collect()
      );
    (lasso, land, c.width, c.height)
  };

  let (lasso, w, h) = downsample(&lasso, width, height, MAX_DIM);
  let land = land.map(|l| downsample(&l, width, height, MAX_DIM).0);

  let mut rgba = vec![0u8; w * h * 4];
  for i in 0..w * h {
    let (rgb, alpha): ([u8; 3], u8) = if lasso[i] {
      ([255, 140, 0], 165)
    } else if land.as_ref().is_some_and(|l| l[i]) {
      ([130, 130, 130], 40)
    } else {
      continue;
    };
    rgba[i * 4..i * 4 + 3].copy_from_slice(&rgb);
    rgba[i * 4 + 3] = alpha;
  }

  let png = encode_png_rgba(&rgba, w, h)?;
  use base64::Engine;
  Ok(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png)))
}

/// Render the landmass segmentation as a translucent RGBA PNG (data URL) for
/// use as a georeferenced Leaflet image overlay. Water is transparent. With
/// `selected`, that landmass is highlighted and the rest fade back.
#[tauri::command]
async fn landmass_overlay(
  state: tauri::State<'_, ElevationState>,
  selected: Option<Vec<u32>>
) -> Result<String, String> {
  let selected = selected.unwrap_or_default();
  const MAX_DIM: usize = 1024;
  let (labels, width, height, n_landmasses) = {
    let guard = state.0.lock().unwrap();
    let c = guard.as_ref().ok_or("No terrain loaded — fetch first")?;
    let labels = c.labels
      .clone()
      .ok_or("No landmass data — fetch with the water mask enabled")?;
    (labels, c.width, c.height, c.n_landmasses)
  };
  let slots = landmass_palette_slots(&labels, n_landmasses);

  // nearest-sample downsample, same scheme as elevation preview
  let (labels, w, h) = if width <= MAX_DIM && height <= MAX_DIM {
    (labels, width, height)
  } else {
    let scale = (width.max(height) + MAX_DIM - 1) / MAX_DIM;
    let dw = (width + scale - 1) / scale;
    let dh = (height + scale - 1) / scale;
    let mut out = vec![0u32; dw * dh];
    for row in 0..dh {
      for col in 0..dw {
        out[row * dw + col] = labels[(row * scale).min(height - 1) * width + (col * scale).min(width - 1)];
      }
    }
    (out, dw, dh)
  };

  let mut rgba = vec![0u8; w * h * 4];
  for (i, &id) in labels.iter().enumerate() {
    if id == 0 {
      continue; // water stays transparent
    }
    let (rgb, alpha) = if selected.contains(&id) {
      ([255u8, 140, 0], 165u8)
    } else if !selected.is_empty() {
      ([130u8, 130, 130], 40u8)
    } else {
      (LANDMASS_PALETTE[slots[id as usize]], 95u8)
    };
    rgba[i * 4..i * 4 + 3].copy_from_slice(&rgb);
    rgba[i * 4 + 3] = alpha;
  }

  let png = encode_png_rgba(&rgba, w, h)?;
  use base64::Engine;
  Ok(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png)))
}

#[tauri::command]
async fn export_mask(
  app: tauri::AppHandle,
  state: tauri::State<'_, ElevationState>
) -> Result<String, String> {
  let (mask, width, height) = {
    let guard = state.0.lock().unwrap();
    let c = guard.as_ref().ok_or("No terrain loaded — fetch first")?;
    let mask = c.water_mask
      .clone()
      .ok_or("No water mask cached — enable the water mask and fetch again")?;
    (mask, c.width, c.height)
  };

  use tauri_plugin_dialog::DialogExt;
  let path = app
    .dialog()
    .file()
    .set_title("Export water mask as PNG")
    .add_filter("PNG image", &["png"])
    .blocking_save_file();

  let path = match path {
    Some(p) => p.into_path().map_err(|e| e.to_string())?,
    None => {
      return Err("Cancelled".into());
    }
  };

  const WATER: [u8; 3] = [60, 110, 200];
  const LAND: [u8; 3] = [225, 222, 210];
  let mut rgb = Vec::with_capacity(width * height * 3);
  for &is_water in &mask {
    rgb.extend_from_slice(if is_water { &WATER } else { &LAND });
  }

  let file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
  let mut enc = png::Encoder::new(std::io::BufWriter::new(file), width as u32, height as u32);
  enc.set_color(png::ColorType::Rgb);
  enc.set_depth(png::BitDepth::Eight);
  let mut writer = enc.write_header().map_err(|e| e.to_string())?;
  writer.write_image_data(&rgb).map_err(|e| e.to_string())?;
  writer.finish().map_err(|e| e.to_string())?;

  Ok(path.to_string_lossy().into_owned())
}

// ── App entry ─────────────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder
    ::default()
    .manage(ElevationState(Mutex::new(None)))
    .plugin(tauri_plugin_opener::init())
    .plugin(tauri_plugin_dialog::init())
    .invoke_handler(
      tauri::generate_handler![
        fetch_elevation,
        export_stl,
        export_mask,
        landmass_at,
        landmass_overlay,
        lasso_select,
        lasso_overlay
      ]
    )
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
