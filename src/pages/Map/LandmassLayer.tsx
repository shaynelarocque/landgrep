import { ImageOverlay, useMapEvents } from "react-leaflet"
import { invoke } from "@tauri-apps/api/core"
import { useStore, type LandmassInfo } from "../../data/store"

/**
 * Drapes the landmass segmentation over the map at the fetched bounds and
 * handles click-to-select: clicking a landmass highlights it, clicking
 * water (or anywhere outside) clears the selection.
 */
export default function LandmassLayer({ drawMode }: { drawMode: boolean }) {
  const {
    landmassOverlay,
    fetchedBounds,
    setLandmassOverlay,
    selectedLandmasses,
    setSelectedLandmasses,
  } = useStore()

  useMapEvents({
    async click(e) {
      if (drawMode || !landmassOverlay || !fetchedBounds) return
      const { lat, lng } = e.latlng
      const b = fetchedBounds
      const inside = lat >= b.south && lat <= b.north && lng >= b.west && lng <= b.east
      const additive = e.originalEvent.shiftKey
      try {
        const info = inside
          ? await invoke<LandmassInfo | null>("landmass_at", { lat, lng })
          : null

        let next: LandmassInfo[]
        if (additive) {
          if (!info) return // shift-click on water keeps the selection
          next = selectedLandmasses.some(lm => lm.id === info.id)
            ? selectedLandmasses.filter(lm => lm.id !== info.id) // toggle off
            : [...selectedLandmasses, info]
        } else {
          next = info ? [info] : []
        }
        setSelectedLandmasses(next)
        setLandmassOverlay(
          await invoke<string>("landmass_overlay", { selected: next.map(lm => lm.id) })
        )
      } catch {
        // cache was replaced mid-click; next fetch rebuilds the overlay
      }
    },
  })

  if (!landmassOverlay || !fetchedBounds) return null
  return (
    <ImageOverlay
      url={landmassOverlay}
      bounds={[
        [fetchedBounds.south, fetchedBounds.west],
        [fetchedBounds.north, fetchedBounds.east],
      ]}
      zIndex={450}
    />
  )
}
