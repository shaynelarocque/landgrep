import { useState } from "react"
import { invoke } from "@tauri-apps/api/core"
import { useStore, type Heightmap, type Bounds } from "../../data/store"

const PREVIEW_RES = 256 // pixels sent to the 3D preview over IPC
import styles from "./Map.module.css"

interface Props {
  drawMode: boolean
  onToggleDraw: () => void
}

export default function BottomPanel({ drawMode, onToggleDraw }: Props) {
  const { apiKey, bounds, setBounds, setHeightmap, demType, waterMask } = useStore()
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const canFetch =
    !!apiKey &&
    !!bounds &&
    bounds.north > bounds.south &&
    bounds.east > bounds.west

  async function handleFetch() {
    if (!bounds || !apiKey) return
    setLoading(true)
    setError(null)
    try {
      const result = await invoke<Heightmap>("fetch_elevation", {
        south: bounds.south,
        north: bounds.north,
        west: bounds.west,
        east: bounds.east,
        apiKey,
        demType,
        previewRes: PREVIEW_RES,
        waterMask,
      })
      setHeightmap(result)
    } catch (e) {
      setError(String(e))
    } finally {
      setLoading(false)
    }
  }

  function setCoord(field: keyof Bounds, raw: string) {
    const val = parseFloat(raw)
    if (isNaN(val)) return
    setBounds({ ...(bounds ?? { north: 0, south: 0, east: 0, west: 0 }), [field]: val } as Bounds)
  }

  const coord = (field: keyof Bounds, label: string) => (
    <label className={styles.coordGroup}>
      <span className={styles.coordLabel}>{label}</span>
      <input
        type="number"
        className={styles.coordInput}
        value={bounds?.[field]?.toFixed(4) ?? ""}
        onChange={e => setCoord(field, e.target.value)}
        step={0.001}
        placeholder="—"
      />
    </label>
  )

  return (
    <div className={styles.bottomPanel}>
      <button
        className={`${styles.drawBtn} ${drawMode ? styles.drawBtnActive : ""}`}
        onClick={onToggleDraw}
        title="Draw a rectangle on the map"
      >
        {drawMode ? "✕ Cancel" : "✏ Draw"}
      </button>

      <div className={styles.divider} />

      {coord("north", "N")}
      {coord("south", "S")}
      {coord("east", "E")}
      {coord("west", "W")}

      <div className={styles.divider} />

      <button
        className={styles.fetchBtn}
        disabled={!canFetch || loading}
        onClick={handleFetch}
      >
        {loading ? "Fetching…" : "Fetch Terrain"}
      </button>

      {error && <span className={styles.error}>{error}</span>}
    </div>
  )
}
