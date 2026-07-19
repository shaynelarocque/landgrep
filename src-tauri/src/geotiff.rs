use std::io::Cursor;
use tiff::decoder::{ Decoder, DecodingResult };

pub(crate) fn parse_geotiff(bytes: Vec<u8>) -> Result<(Vec<f32>, usize, usize), String> {
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
