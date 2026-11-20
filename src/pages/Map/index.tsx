import { useState } from "react"
import { MapContainer, TileLayer, useMapEvents } from "react-leaflet"
import { Link } from "react-router"
import "leaflet/dist/leaflet.css"
import { useStore } from "../../data/store"
import RectangleDraw from "./RectangleDraw"
import LassoDraw from "./LassoDraw"
import LandmassLayer from "./LandmassLayer"
import BottomPanel from "./BottomPanel"
import TerrainPreview from "./TerrainPreview"
import styles from "./Map.module.css"

/** Active map drawing tool; null = normal pan/click interaction. */
export type DrawTool = "rect" | "lasso" | null

// Syncs map position back to the store so Settings can capture it.
function MapTracker() {
  const setMapHome = useStore(s => s.setMapHome)
  useMapEvents({
    moveend(e) {
      const c = e.target.getCenter()
      const z = e.target.getZoom()
      setMapHome({ lat: c.lat, lng: c.lng, zoom: z })
    },
  })
  return null
}

export default function MapPage() {
  const [tool, setTool] = useState<DrawTool>(null)
  // Initial position only — MapContainer treats center/zoom as init-only, and
  // subscribing here would re-render (and previously remount) the map on every moveend.
  const [mapHome] = useState(() => useStore.getState().mapHome)

  return (
    <div className={styles.container}>
      <div className={styles.topNav}>
        <span className={styles.brand}>landgrep</span>
        <Link to='/settings' className={styles.settingsLink} title='Settings'>
          ⚙
        </Link>
      </div>

      <MapContainer
        center={[mapHome.lat, mapHome.lng]}
        zoom={mapHome.zoom}
        className={styles.map}
        zoomControl={false}
      >
        <TileLayer
          url='https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png'
          attribution='&copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a>'
        />
        <MapTracker />
        <RectangleDraw drawMode={tool === "rect"} onDrawEnd={() => setTool(null)} />
        <LassoDraw active={tool === "lasso"} onDrawEnd={() => setTool(null)} />
        <LandmassLayer drawMode={tool !== null} />
      </MapContainer>

      <BottomPanel tool={tool} setTool={setTool} />
      <TerrainPreview />
    </div>
  )
}
