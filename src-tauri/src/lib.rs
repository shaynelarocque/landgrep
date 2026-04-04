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

/// Convert a ring of (lat, lon) points to pixel coordinates, then scanline-fill
/// all interior pixels in `mask`. Uses the even-odd rule.
fn rasterize_polygon(
  ring: &[[f64; 2]],
  south: f64,
  north: f64,
  west: f64,
  east: f64,
  width: usize,
  height: usize,
  mask: &mut Vec<bool>
) {
  if ring.len() < 3 {
    return;
  }

  // lat/lon → (col, row) in pixel space, row 0 = north
  let to_px = |lat: f64, lon: f64| -> (f64, f64) {
    let col = ((lon - west) / (east - west)) * ((width as f64) - 1.0);
    let row = ((north - lat) / (north - south)) * ((height as f64) - 1.0);
    (col, row)
  };

  let px: Vec<(f64, f64)> = ring
    .iter()
    .map(|&[lat, lon]| to_px(lat, lon))
    .collect();

  let min_row = px
    .iter()
    .map(|p| p.1)
    .fold(f64::INFINITY, f64::min)
    .max(0.0) as usize;
  let max_row = px
    .iter()
    .map(|p| p.1)
    .fold(f64::NEG_INFINITY, f64::max)
    .min((height as f64) - 1.0) as usize;

  let n = px.len();
  for row in min_row..=max_row {
    let y = (row as f64) + 0.5;
    let mut xs: Vec<f64> = Vec::new();
    for i in 0..n {
      let (x0, y0) = px[i];
      let (x1, y1) = px[(i + 1) % n];
      if (y0 <= y && y < y1) || (y1 <= y && y < y0) {
        xs.push(x0 + ((y - y0) * (x1 - x0)) / (y1 - y0));
      }
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut i = 0;
    while i + 1 < xs.len() {
      let c0 = xs[i].max(0.0) as usize;
      let c1 = (xs[i + 1] as usize).min(width - 1);
      for col in c0..=c1 {
        mask[row * width + col] = true;
      }
      i += 2;
    }
  }
}

/// Fetch water polygons from Overpass and return a pixel mask.
/// Returns all-false on any error so the fetch is never blocked.
async fn build_water_mask(
  south: f64,
  north: f64,
  west: f64,
  east: f64,
  width: usize,
  height: usize
) -> Vec<bool> {
  match try_build_water_mask(south, north, west, east, width, height).await {
    Ok(mask) => mask,
    Err(_) => vec![false; width * height],
  }
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

async fn try_build_water_mask(
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
    "[out:json][timeout:30];\
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
    .timeout(std::time::Duration::from_secs(35))
    .build()
    .map_err(|e| e.to_string())?;

  let body: serde_json::Value = client
    .get("https://overpass-api.de/api/interpreter")
    .query(&[("data", &query)])
    .send().await
    .map_err(|e| e.to_string())?
    .json().await
    .map_err(|e| e.to_string())?;

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
          // Only rasterize if OSM closed this way (first == last node)
          if ring.len() >= 3 {
            rasterize_polygon(&ring, south, north, west, east, width, height, &mut mask);
          }
        }
      }
      Some("relation") => {
        // Collect outer way segments then assemble into closed rings before
        // rasterizing. Rasterizing each segment independently produces wrong
        // fills because each open segment gets closed across its endpoints.
        if let Some(members) = el["members"].as_array() {
          let segments: Vec<Vec<[f64; 2]>> = members
            .iter()
            .filter(|m| m["role"].as_str() == Some("outer"))
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
            .collect();

          for ring in assemble_rings(segments) {
            rasterize_polygon(&ring, south, north, west, east, width, height, &mut mask);
          }
        }
      }
      _ => {}
    }
  }

  Ok(mask)
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

  if water_mask {
    let mask = build_water_mask(south, north, west, east, width, height).await;
    for (i, &is_water) in mask.iter().enumerate() {
      if is_water {
        full_data[i] = 0.0;
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
  });

  let cap = if preview_res == 0 { 256 } else { preview_res };
  let (preview_data, pw, ph) = downsample(&full_data, width, height, cap);

  Ok(Heightmap { data: preview_data, width: pw, height: ph, min_elev, max_elev })
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

// ── App entry ─────────────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder
    ::default()
    .manage(ElevationState(Mutex::new(None)))
    .plugin(tauri_plugin_opener::init())
    .plugin(tauri_plugin_dialog::init())
    .invoke_handler(tauri::generate_handler![fetch_elevation, export_stl])
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
