import { useState } from "react"
import { MapContainer, TileLayer, useMapEvents } from "react-leaflet"
import { Link } from "react-router"
import "leaflet/dist/leaflet.css"
import { useStore } from "../../data/store"
import RectangleDraw from "./RectangleDraw"
import BottomPanel from "./BottomPanel"
import TerrainPreview from "./TerrainPreview"
import styles from "./Map.module.css"

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
  const [drawMode, setDrawMode] = useState(false)
  const { mapHome } = useStore()

  return (
    <div className={styles.container}>
      <div className={styles.topNav}>
        <span className={styles.brand}>Settings</span>
        <Link to='/settings' className={styles.settingsLink} title='Settings'>
          ⚙
        </Link>
      </div>

      <MapContainer
        key={`${mapHome.lat},${mapHome.lng},${mapHome.zoom}`}
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
        <RectangleDraw drawMode={drawMode} onDrawEnd={() => setDrawMode(false)} />
      </MapContainer>

      <BottomPanel drawMode={drawMode} onToggleDraw={() => setDrawMode(d => !d)} />
      <TerrainPreview />
    </div>
  )
}
