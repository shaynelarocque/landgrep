import { create } from "zustand"

export interface Bounds {
  north: number
  south: number
  east: number
  west: number
}

export interface Heightmap {
  data: number[]
  width: number
  height: number
  minElev: number
  maxElev: number
  /** Elevation all water pixels were flattened to; null when masking was off,
   *  failed, or found no water in the bbox. */
  waterLevel: number | null
  waterMaskError: string | null
  /** Number of connected land components; null when no mask was built. */
  landmassCount: number | null
}

export interface LandmassInfo {
  id: number
  areaKm2: number
}

export interface LassoInfo {
  areaKm2: number
}

export interface MapHome {
  lat: number
  lng: number
  zoom: number
}

// DEM products available via OpenTopography globaldem endpoint
export const DEM_TYPES = [
  { value: "SRTMGL3", label: "SRTM GL3 — 90 m" },
  { value: "SRTMGL1", label: "SRTM GL1 — 30 m" },
  { value: "AW3D30",  label: "AW3D30 — 30 m" },
  { value: "COP30",   label: "Copernicus — 30 m" },
] as const

export type DemType = (typeof DEM_TYPES)[number]["value"]

const DEFAULT_HOME: MapHome = { lat: 45.54, lng: -73.65, zoom: 10 }

interface State {
  apiKey: string
  bounds: Bounds | null
  heightmap: Heightmap | null
  /** Bounds snapshot of the last successful fetch (bbox inputs may be edited after). */
  fetchedBounds: Bounds | null
  /** Data-URL PNG of the landmass segmentation, draped on the map. */
  landmassOverlay: string | null
  /** Shift-click accumulates multiple landmasses into one selection. */
  selectedLandmasses: LandmassInfo[]
  /** Freehand lasso selection (clipped to land); exclusive with landmass selection. */
  lassoSelection: LassoInfo | null
  verticalScale: number
  demType: DemType
  /** Max pixel dimension of the mesh written to the STL file (0 = full res). */
  exportRes: number
  /** Thickness (mm) of the solid slab under the lowest terrain in exports. */
  baseMm: number
  /** Edge-smoothing supersample factor for exports (1 = off, 2–4 = smoother
   *  borders at factor× the mesh dimensions). */
  smoothFactor: number
  waterMask: boolean
  mapHome: MapHome
  setApiKey: (key: string) => void
  setBounds: (bounds: Bounds | null) => void
  setHeightmap: (hm: Heightmap | null) => void
  setFetchedBounds: (b: Bounds | null) => void
  setLandmassOverlay: (url: string | null) => void
  setSelectedLandmasses: (lms: LandmassInfo[]) => void
  setLassoSelection: (l: LassoInfo | null) => void
  setVerticalScale: (scale: number) => void
  setDemType: (t: DemType) => void
  setExportRes: (r: number) => void
  setBaseMm: (mm: number) => void
  setSmoothFactor: (f: number) => void
  setWaterMask: (v: boolean) => void
  setMapHome: (home: MapHome) => void
}

const load = (key: string, fallback: string) =>
  localStorage.getItem(key) ?? fallback

const loadJson = <T>(key: string, fallback: T): T => {
  try {
    const raw = localStorage.getItem(key)
    return raw ? (JSON.parse(raw) as T) : fallback
  } catch {
    return fallback
  }
}

export const useStore = create<State>()(set => ({
  apiKey: load("ot-api-key", ""),
  bounds: null,
  heightmap: null,
  fetchedBounds: null,
  landmassOverlay: null,
  selectedLandmasses: [],
  lassoSelection: null,
  verticalScale: parseFloat(load("vertical-scale", "1.5")),
  demType: (load("dem-type", "SRTMGL3") as DemType),
  exportRes: parseInt(load("export-res", "512")),
  baseMm: parseFloat(load("base-mm", "2")),
  smoothFactor: parseInt(load("smooth-factor", "1")),
  waterMask: load("water-mask", "true") === "true",
  mapHome: loadJson("map-home", DEFAULT_HOME),
  setApiKey: key => {
    localStorage.setItem("ot-api-key", key)
    set({ apiKey: key })
  },
  setBounds: bounds => set({ bounds }),
  setHeightmap: hm => set({ heightmap: hm }),
  setFetchedBounds: b => set({ fetchedBounds: b }),
  setLandmassOverlay: url => set({ landmassOverlay: url }),
  setSelectedLandmasses: lms => set({ selectedLandmasses: lms }),
  setLassoSelection: l => set({ lassoSelection: l }),
  setVerticalScale: scale => {
    localStorage.setItem("vertical-scale", String(scale))
    set({ verticalScale: scale })
  },
  setDemType: t => {
    localStorage.setItem("dem-type", t)
    set({ demType: t })
  },
  setExportRes: r => {
    localStorage.setItem("export-res", String(r))
    set({ exportRes: r })
  },
  setBaseMm: mm => {
    localStorage.setItem("base-mm", String(mm))
    set({ baseMm: mm })
  },
  setSmoothFactor: f => {
    localStorage.setItem("smooth-factor", String(f))
    set({ smoothFactor: f })
  },
  setWaterMask: v => {
    localStorage.setItem("water-mask", String(v))
    set({ waterMask: v })
  },
  setMapHome: home => {
    localStorage.setItem("map-home", JSON.stringify(home))
    set({ mapHome: home })
  },
}))
