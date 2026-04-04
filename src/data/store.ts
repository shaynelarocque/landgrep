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
  verticalScale: number
  demType: DemType
  /** Max pixel dimension of the mesh written to the STL file (0 = full res). */
  exportRes: number
  waterMask: boolean
  mapHome: MapHome
  setApiKey: (key: string) => void
  setBounds: (bounds: Bounds | null) => void
  setHeightmap: (hm: Heightmap | null) => void
  setVerticalScale: (scale: number) => void
  setDemType: (t: DemType) => void
  setExportRes: (r: number) => void
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
  verticalScale: parseFloat(load("vertical-scale", "1.5")),
  demType: (load("dem-type", "SRTMGL3") as DemType),
  exportRes: parseInt(load("export-res", "512")),
  waterMask: load("water-mask", "true") === "true",
  mapHome: loadJson("map-home", DEFAULT_HOME),
  setApiKey: key => {
    localStorage.setItem("ot-api-key", key)
    set({ apiKey: key })
  },
  setBounds: bounds => set({ bounds }),
  setHeightmap: hm => set({ heightmap: hm }),
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
  setWaterMask: v => {
    localStorage.setItem("water-mask", String(v))
    set({ waterMask: v })
  },
  setMapHome: home => {
    localStorage.setItem("map-home", JSON.stringify(home))
    set({ mapHome: home })
  },
}))
