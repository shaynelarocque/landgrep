import { BrowserRouter, Routes, Route } from "react-router"

import "./App.css"
import Map from "./pages/Map"
import Settings from "./pages/Settings"

function App() {
  return (
    <main className='container'>
      <BrowserRouter>
        <Routes>
          <Route path='/' element={<Map />} />
          <Route path='/settings' element={<Settings />} />
        </Routes>
      </BrowserRouter>
    </main>
  )
}

export default App
