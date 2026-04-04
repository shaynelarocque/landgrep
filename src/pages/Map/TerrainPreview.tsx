import { useMemo, useRef } from "react"
import { Canvas } from "@react-three/fiber"
import { OrbitControls } from "@react-three/drei"
import * as THREE from "three"
import { invoke } from "@tauri-apps/api/core"
import { useStore, type Heightmap, type Bounds } from "../../data/store"
import styles from "./Map.module.css"

// --- colour ramp: elevation t ∈ [0,1] → RGB ---
function elevColor(t: number): [number, number, number] {
  // water → lowland → highland → snow
  const stops: Array<[number, [number, number, number]]> = [
    [0.0, [0.29, 0.50, 0.71]],  // #4a80b5 water
    [0.05, [0.35, 0.55, 0.30]], // #598a4d low green
    [0.35, [0.60, 0.52, 0.33]], // #998552 tawny
    [0.70, [0.70, 0.65, 0.55]], // #b3a68c pale rock
    [1.0, [0.95, 0.95, 0.95]],  // #f2f2f2 snow
  ]
  for (let i = 1; i < stops.length; i++) {
    const [t0, c0] = stops[i - 1]
    const [t1, c1] = stops[i]
    if (t <= t1) {
      const f = (t - t0) / (t1 - t0)
      return [c0[0] + f * (c1[0] - c0[0]), c0[1] + f * (c1[1] - c0[1]), c0[2] + f * (c1[2] - c0[2])]
    }
  }
  return stops[stops.length - 1][1]
}

function TerrainMesh({ heightmap, bounds, verticalScale }: {
  heightmap: Heightmap
  bounds: Bounds
  verticalScale: number
}) {
  const { data, width, height, minElev, maxElev } = heightmap
  const elevRange = Math.max(maxElev - minElev, 1)

  // Aspect ratio from physical dimensions (metres)
  const latCenter = (bounds.north + bounds.south) / 2
  const latM = (bounds.north - bounds.south) * 110540
  const lonM = (bounds.east - bounds.west) * 111320 * Math.cos(latCenter * Math.PI / 180)
  // Cap preview resolution at 256 in either dimension
  const MAX = 256
  const segW = Math.min(width - 1, MAX)
  const segH = Math.min(height - 1, MAX)

  const BASE = 100 // three.js units along the longest physical dimension
  const maxM = Math.max(latM, lonM)
  const planeW = (lonM / maxM) * BASE
  const planeH = (latM / maxM) * BASE
  // Mirror the STL formula: same horizontal units/metre scale applied vertically,
  // then multiplied by verticalScale. This makes the preview match the output.
  const unitsPerMetre = BASE / maxM

  const geometry = useMemo(() => {
    const g = new THREE.PlaneGeometry(planeW, planeH, segW, segH)
    const pos = g.attributes.position.array as Float32Array
    const totalVerts = (segW + 1) * (segH + 1)
    const colors = new Float32Array(totalVerts * 3)

    for (let row = 0; row <= segH; row++) {
      for (let col = 0; col <= segW; col++) {
        const vIdx = (row * (segW + 1) + col) * 3
        const dataCol = Math.round((col / segW) * (width - 1))
        const dataRow = Math.round((row / segH) * (height - 1))
        const elev = data[dataRow * width + dataCol] ?? 0
        const clamped = Math.max(0, elev)
        // Same scale as STL: metres × (units/metre) × verticalScale
        pos[vIdx + 2] = clamped * unitsPerMetre * verticalScale

        const t = elevRange > 0 ? clamped / elevRange : 0
        const [r, gv, b] = elevColor(t)
        colors[vIdx] = r
        colors[vIdx + 1] = gv
        colors[vIdx + 2] = b
      }
    }

    g.setAttribute("color", new THREE.BufferAttribute(colors, 3))
    g.computeVertexNormals()
    return g
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [data, unitsPerMetre, verticalScale, planeW, planeH, segW, segH])

  const meshRef = useRef<THREE.Mesh>(null)

  return (
    <mesh ref={meshRef} geometry={geometry} rotation={[-Math.PI / 2, 0, 0]}>
      <meshStandardMaterial vertexColors side={THREE.DoubleSide} />
    </mesh>
  )
}

export default function TerrainPreview() {
  const { heightmap, bounds, verticalScale, exportRes, setHeightmap } = useStore()
  if (!heightmap || !bounds) return null

  async function handleExportStl() {
    try {
      const path = await invoke<string>("export_stl", {
        verticalScale,
        exportRes, // 0 = full resolution
      })
      alert(`Saved to ${path}`)
    } catch (e) {
      alert(`Export failed: ${e}`)
    }
  }

  const { minElev, maxElev } = heightmap
  const meta = `${heightmap.width}×${heightmap.height}  ·  ${minElev.toFixed(0)}–${maxElev.toFixed(0)} m`

  return (
    <div className={styles.previewOverlay}>
      <div className={styles.previewHeader}>
        <span className={styles.previewTitle}>Terrain Preview</span>
        <span className={styles.previewMeta}>{meta}</span>
        <button className={styles.closeBtn} onClick={() => setHeightmap(null)}>✕</button>
      </div>

      <Canvas
        className={styles.previewCanvas}
        camera={{ position: [0, 50, 170], fov: 50 }}
        gl={{ antialias: true }}
      >
        <ambientLight intensity={0.5} />
        <directionalLight position={[100, 200, 100]} intensity={1.2} castShadow />
        <TerrainMesh heightmap={heightmap} bounds={bounds} verticalScale={verticalScale} />
        <OrbitControls makeDefault />
      </Canvas>

      <div className={styles.exportRow}>
        <button className={styles.exportBtn} onClick={handleExportStl}>
          Export STL
        </button>
        <button className={styles.exportBtn} disabled title="Coming soon">
          Export NC
        </button>
      </div>
    </div>
  )
}
