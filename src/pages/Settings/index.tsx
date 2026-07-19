import { useStore, DEM_TYPES, type DemType } from "../../data/store"
import { Link } from "react-router"
import styles from "./Settings.module.css"

const EXPORT_RES_OPTIONS = [
  { value: 256, label: "256 x 256  — fast, good for small pieces" },
  { value: 512, label: "512 x 512  — recommended (Nomad sweet spot)" },
  { value: 1024, label: "1024 x 1024 — fine detail, large file" },
  { value: 0, label: "Full resolution — may be very slow / large" },
]

const SMOOTH_OPTIONS = [
  { value: 1, label: "Off — mesh at export resolution" },
  { value: 2, label: "2× — chamfered edges" },
  { value: 3, label: "3× — smoother, 9× the triangles" },
  { value: 4, label: "4× — smoothest, 16× the triangles" },
]

export default function Settings() {
  const {
    apiKey,
    verticalScale,
    demType,
    exportRes,
    baseMm,
    smoothFactor,
    waterMask,
    mapHome,
    setApiKey,
    setVerticalScale,
    setDemType,
    setExportRes,
    setBaseMm,
    setSmoothFactor,
    setWaterMask,
    setMapHome,
  } = useStore()

  return (
    <div className={styles.page}>
      <div className={styles.header}>
        <Link to='/' className={styles.back}>
          ← Map
        </Link>
        <h1 className={styles.title}>Settings</h1>
      </div>

      <div className={styles.content}>
        {/* API keys */}
        <section className={styles.section}>
          <h2 className={styles.sectionTitle}>API Keys</h2>
          <label className={styles.field}>
            <span className={styles.label}>OpenTopography API Key</span>
            <input
              type='password'
              className={styles.input}
              value={apiKey}
              onChange={e => setApiKey(e.target.value)}
              placeholder='Paste your key here'
              autoComplete='off'
            />
            <span className={styles.hint}>
              Free key at{" "}
              <a href='https://opentopography.org' target='_blank' rel='noreferrer'>
                opentopography.org
              </a>
            </span>
          </label>
        </section>

        {/* Map */}
        <section className={styles.section}>
          <h2 className={styles.sectionTitle}>Map</h2>
          <div className={styles.field}>
            <span className={styles.label}>Default view</span>
            <div className={styles.homeRow}>
              <span className={styles.homeCoords}>
                {mapHome.lat.toFixed(3)}, {mapHome.lng.toFixed(3)} · zoom {mapHome.zoom}
              </span>
              <button className={styles.setHomeBtn} onClick={() => setMapHome(mapHome)}>
                Set current view as default
              </button>
            </div>
            <span className={styles.hint}>
              Navigate to your preferred starting location on the map, then save it here.
            </span>
          </div>
        </section>

        {/* Fetch settings */}
        <section className={styles.section}>
          <h2 className={styles.sectionTitle}>Data Source</h2>
          <label className={styles.field}>
            <span className={styles.label}>DEM product</span>
            <select
              className={styles.select}
              value={demType}
              onChange={e => setDemType(e.target.value as DemType)}
            >
              {DEM_TYPES.map(d => (
                <option key={d.value} value={d.value}>
                  {d.label}
                </option>
              ))}
            </select>
            <span className={styles.hint}>
              Lower resolution = faster fetch; 30 m products give more detail for small regions.
            </span>
          </label>
        </section>

        {/* Terrain / display */}
        <section className={styles.section}>
          <h2 className={styles.sectionTitle}>Terrain</h2>

          <label className={styles.field}>
            <span className={styles.label}>
              Vertical exaggeration &nbsp;
              <strong>{verticalScale.toFixed(1)}×</strong>
            </span>
            <input
              type='range'
              className={styles.range}
              min={0.5}
              max={5}
              step={0.1}
              value={verticalScale}
              onChange={e => setVerticalScale(parseFloat(e.target.value))}
            />
          </label>

          <label className={styles.checkboxField} style={{ marginTop: 16 }}>
            <input
              type='checkbox'
              checked={waterMask}
              onChange={e => setWaterMask(e.target.checked)}
            />
            <span className={styles.label}>Flatten waterways (OSM water mask)</span>
          </label>

          <label className={styles.field} style={{ marginTop: 16 }}>
            <span className={styles.label}>Export mesh resolution</span>
            <select
              className={styles.select}
              value={exportRes}
              onChange={e => setExportRes(parseInt(e.target.value))}
            >
              {EXPORT_RES_OPTIONS.map(o => (
                <option key={o.value} value={o.value}>
                  {o.label}
                </option>
              ))}
            </select>
            <span className={styles.hint}>
              Resolution of the STL mesh. The 3D preview always uses 256 × 256 regardless.
            </span>
          </label>

          <label className={styles.field} style={{ marginTop: 16 }}>
            <span className={styles.label}>Edge smoothing</span>
            <select
              className={styles.select}
              value={smoothFactor}
              onChange={e => setSmoothFactor(parseInt(e.target.value))}
            >
              {SMOOTH_OPTIONS.map(o => (
                <option key={o.value} value={o.value}>
                  {o.label}
                </option>
              ))}
            </select>
            <span className={styles.hint}>
              Supersamples the export so landmass borders come out chamfered and rounded
              instead of pixelated. Interpolation only — it adds no real terrain detail, and
              mesh size grows with the factor, so pair higher smoothing with a lower export
              resolution.
            </span>
          </label>

          <label className={styles.field} style={{ marginTop: 16 }}>
            <span className={styles.label}>
              Base thickness &nbsp;
              <strong>{baseMm.toFixed(1)} mm</strong>
            </span>
            <input
              type='range'
              className={styles.range}
              min={0.5}
              max={10}
              step={0.5}
              value={baseMm}
              onChange={e => setBaseMm(parseFloat(e.target.value))}
            />
            <span className={styles.hint}>
              Solid slab under the lowest terrain. Exports are watertight: top surface, side
              walls, and a flat bottom.
            </span>
          </label>
        </section>
      </div>
    </div>
  )
}
