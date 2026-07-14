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
}

fn downsample(src: &[f32], sw: usize, sh: usize, max_dim: usize) -> (Vec<f32>, usize, usize) {
  if sw <= max_dim && sh <= max_dim {
    return (src.to_vec(), sw, sh);
  }
  let scale = (sw.max(sh) + max_dim - 1) / max_dim;
  let dw = (sw + scale - 1) / scale;
  let dh = (sh + scale - 1) / scale;
  let mut out = vec![0f32; dw * dh];
  for row in 0..dh {
    for col in 0..dw {
      let sr = (row * scale).min(sh - 1);
      let sc = (col * scale).min(sw - 1);
      out[row * dw + col] = src[sr * sw + sc];
    }
  }
  (out, dw, dh)
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

  /// Full-response fixture check: set LANDGREP_OVERPASS_FIXTURE to a saved
  /// Overpass JSON response to validate mask geometry against it.
  #[test]
  fn mask_matches_fixture() {
    let Ok(path) = std::env::var("LANDGREP_OVERPASS_FIXTURE") else {
      return;
    };
    let body: serde_json::Value = serde_json
      ::from_str(&std::fs::read_to_string(path).unwrap())
      .unwrap();
    let mask = mask_from_overpass(&body, 45.30, 45.75, -74.25, -73.3, 760, 500).unwrap();
    let water = mask
      .iter()
      .filter(|&&w| w)
      .count();
    // Reference count from an independent Python implementation of the same
    // even-odd fill over the identical fixture.
    assert_eq!(water, 54984);
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
  if water_mask {
    match build_water_mask(south, north, west, east, width, height).await {
      Ok(mask) => {
        water_level = flatten_water(&mut full_data, &mask);
        cached_mask = Some(mask);
      }
      Err(e) => {
        water_mask_error = Some(e);
      }
    }
  }

  let min_elev = full_data.iter().cloned().fold(f32::INFINITY, f32::min);
  let max_elev = full_data.iter().cloned().fold(f32::NEG_INFINITY, f32::max);

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

fn build_stl(
  data: &[f32],
  width: usize,
  height: usize,
  vertical_scale: f32,
  south: f64,
  north: f64,
  west: f64,
  east: f64
) -> Vec<u8> {
  let lat_center = (south + north) / 2.0;
  let lat_m = (north - south) * 110_540.0;
  let lon_m = (east - west) * 111_320.0 * ((lat_center * std::f64::consts::PI) / 180.0).cos();
  let max_m = lat_m.max(lon_m);
  let mm_per_m = 200.0 / max_m;

  let cell_x = ((lon_m * mm_per_m) / ((width - 1) as f64)) as f32;
  let cell_y = ((lat_m * mm_per_m) / ((height - 1) as f64)) as f32;

  let elev_range =
    data.iter().cloned().fold(f32::NEG_INFINITY, f32::max) -
    data.iter().cloned().fold(f32::INFINITY, f32::min);
  let elev_scale = if elev_range > 0.0 {
    (((200.0 * mm_per_m) / (elev_range as f64)) as f32) * vertical_scale
  } else {
    1.0
  };

  let v = |col: usize, row: usize| -> [f32; 3] {
    [(col as f32) * cell_x, (row as f32) * cell_y, data[row * width + col].max(0.0) * elev_scale]
  };

  let n_tris = (width - 1) * (height - 1) * 2;
  let mut buf = Vec::with_capacity(84 + n_tris * 50);
  buf.extend_from_slice(&[0u8; 80]);
  buf.extend_from_slice(&(n_tris as u32).to_le_bytes());

  for row in 0..height - 1 {
    for col in 0..width - 1 {
      let bl = v(col, row);
      let br = v(col + 1, row);
      let tl = v(col, row + 1);
      let tr = v(col + 1, row + 1);
      write_tri(&mut buf, bl, br, tl);
      write_tri(&mut buf, br, tr, tl);
    }
  }

  buf
}

#[tauri::command]
async fn export_stl(
  app: tauri::AppHandle,
  state: tauri::State<'_, ElevationState>,
  vertical_scale: f32,
  export_res: usize
) -> Result<String, String> {
  let (data, width, height, south, north, west, east) = {
    let guard = state.0.lock().unwrap();
    let c = guard.as_ref().ok_or("No terrain loaded — fetch first")?;
    (c.data.clone(), c.width, c.height, c.south, c.north, c.west, c.east)
  };

  let (mesh_data, mesh_w, mesh_h) = if export_res > 0 {
    downsample(&data, width, height, export_res)
  } else {
    (data, width, height)
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

  let stl = build_stl(&mesh_data, mesh_w, mesh_h, vertical_scale, south, north, west, east);
  std::fs::write(&path, stl).map_err(|e| e.to_string())?;

  Ok(path.to_string_lossy().into_owned())
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
    .invoke_handler(tauri::generate_handler![fetch_elevation, export_stl, export_mask])
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
