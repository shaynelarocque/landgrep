/// Label 4-connected components of land (`!water`) pixels.
/// Returns (labels, count) where labels[i] = 0 for water, 1..=count for land.
pub(crate) fn label_landmasses(water: &[bool], width: usize, height: usize) -> (Vec<u32>, u32) {
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
pub(crate) const LANDMASS_PALETTE: [[u8; 3]; 8] = [
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
pub(crate) fn landmass_palette_slots(labels: &[u32], n: u32) -> Vec<usize> {
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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::test_support::montreal_fixture;
  use crate::water::mask_from_overpass;

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
}
