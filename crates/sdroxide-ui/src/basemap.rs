//! The ground the flat maps are drawn on.
//!
//! The FT8/WSPR panel map and the APRS map are dot matrices, and this is what
//! decides where a dot goes: the same Natural Earth rasters the 3D globe is
//! textured with (`assets/earth/`, rebuilt by `make_earth_maps.py`), decoded
//! once and kept as mip pyramids for the dot grid to sample. One set of
//! coastlines for every view in the program, so a QTH lands on the same
//! shoreline whichever one is open — and the flat maps get the country borders
//! and rivers the globe has had all along.
//!
//! Three things are sampled here, all as *coverage* — the fraction of a cell
//! the feature fills, not a yes/no bit:
//!
//! - **land**, whose ½ contour is the coastline. Coverage is what lets a coast
//!   be drawn to a fraction of a cell instead of stepping along the grid, which
//!   is the same trick `solar_body.wgsl` plays on the globe;
//! - **borders**, the international boundaries;
//! - **rivers**, drawn at the width Natural Earth ranks each one at, so how
//!   much of a texel a river covers *is* how big a river it is.
//!
//! Plus [`cities`], which is not a raster at all: a flat map has room for a
//! name, and no texture can carry one.
//!
//! Everything is decoded on first use and never freed. That costs 17 MB and
//! about sixty milliseconds, once, for a program that then never touches the
//! assets again — and only for a session that actually opens a map. Sixty
//! milliseconds is four dropped frames on the frame that first draws one,
//! which is exactly the frame somebody is looking at, so a native session
//! [`prime`]s it on a thread at startup instead.

use std::sync::OnceLock;

/// The world, as `assets/earth/make_earth_maps.py` leaves it.
///
/// Every view in the program reads these same bytes — the flat maps from the
/// pyramids below, the 3D globe and the login backdrop by uploading them to the
/// GPU — so they are named once here rather than `include_bytes!` in each place
/// and linked in three times. All are rasterised from Natural Earth 1:10m and
/// share one coordinate convention (x = −180°…180°, y = +90°…−90°), which is
/// what makes a QTH marker land on the same shoreline whichever view is open.
///
/// Land is 8192×4096 (1/22.75°, ~4.9 km) and holds *coverage* rather than a
/// 1-bit mask: the fraction of each texel that is land. Both the globe's shader
/// and [`land_ink`](crate::widgets::worldmap) draw the shoreline along that
/// field's ½ contour, which interpolation places to a fraction of a texel — so
/// the coast stays a clean curve however far it is zoomed into, instead of the
/// texel staircase a thresholded mask would give. The rest are 4320×2160
/// (1/12°): one-texel lines and small blobs rather than a filled region, so
/// there is no contour to sharpen and the extra grid would only cost memory.
pub(crate) const LAND_PNG: &[u8] = include_bytes!("../assets/earth/land.png");
pub(crate) const BORDER_PNG: &[u8] = include_bytes!("../assets/earth/borders.png");
pub(crate) const RIVER_PNG: &[u8] = include_bytes!("../assets/earth/rivers.png");
/// Built-up urban areas — the shape the globe lights its night side with. Not
/// sampled here: a flat map has room for a name, and uses [`cities`] instead.
pub(crate) const CITY_PNG: &[u8] = include_bytes!("../assets/earth/cities.png");
const CITY_BIN: &[u8] = include_bytes!("../assets/earth/cities.bin");

/// The finest grid kept on the CPU, as an edge length.
///
/// The globe uploads land at the asset's full 8192×4096 because its camera can
/// fly down to the surface. A panel map cannot: its dots are four points apart,
/// so from about 1/11° down it is the *dot grid* that limits what can be seen
/// and not the texels — while one level up costs four times the memory (45 MB
/// against 11), which is why land arrives here one level down.
///
/// It is set at the *line* layers' own 4320 rather than at land's 4096, so
/// those are kept whole. They are one texel wide by construction, and halving
/// one costs half its contrast as well as half its resolution: a border that
/// filled a texel comes back filling half of one, dimmer and twice as broad as
/// what was drawn.
const MAX_DIM: usize = 4320;

/// Coverage as one nibble per cell, 0…15.
///
/// Sixteen levels because of what they are used for: the land contour's
/// position inside a cell (a sixteenth of a texel is far below what a four-point
/// dot grid can show) and a line's alpha (a dot is either drawn or not, and the
/// eye is not counting the sixteen steps in between). Half the memory of a byte
/// per cell, for a difference nothing on screen can resolve.
struct Level {
    w: usize,
    h: usize,
    cov: Vec<u8>,
}

impl Level {
    fn pack(w: usize, h: usize, gray: &[u8]) -> Level {
        let mut cov = vec![0u8; (w * h).div_ceil(2)];
        for (i, &g) in gray.iter().enumerate() {
            // Round to nearest of the sixteen levels rather than truncating,
            // so a fully covered texel comes back as 1.0 and not 15/16.
            let v = ((g as u16 * 15 + 127) / 255) as u8;
            if i % 2 == 0 {
                cov[i / 2] |= v << 4;
            } else {
                cov[i / 2] |= v;
            }
        }
        Level { w, h, cov }
    }

    #[inline]
    fn at(&self, col: usize, row: usize) -> f32 {
        let i = row * self.w + col;
        let byte = self.cov[i >> 1];
        let v = if i & 1 == 0 { byte >> 4 } else { byte & 0x0f };
        f32::from(v) * (1.0 / 15.0)
    }

    /// Coverage at a point, interpolated between the four cells around it.
    ///
    /// Longitude wraps — the world repeats sideways and a map straddling the
    /// date line has to interpolate across it — and latitude clamps, because
    /// past a pole there is nothing to blend with.
    fn bilinear(&self, lon: f64, lat: f64) -> f32 {
        let u = (lon + 180.0) / 360.0 * self.w as f64 - 0.5;
        let v = (90.0 - lat) / 180.0 * self.h as f64 - 0.5;
        let (u0, v0) = (u.floor(), v.floor());
        let (fu, fv) = ((u - u0) as f32, (v - v0) as f32);
        let x0 = u0.rem_euclid(self.w as f64) as usize;
        let x1 = if x0 + 1 == self.w { 0 } else { x0 + 1 };
        let y0 = v0.clamp(0.0, (self.h - 1) as f64) as usize;
        let y1 = (y0 + 1).min(self.h - 1);
        let top = self.at(x0, y0) + (self.at(x1, y0) - self.at(x0, y0)) * fu;
        let bot = self.at(x0, y1) + (self.at(x1, y1) - self.at(x0, y1)) * fu;
        top + (bot - top) * fv
    }
}

/// One equirectangular coverage raster as a mip pyramid.
pub struct Layer {
    levels: Vec<Level>,
}

impl Layer {
    fn build(label: &str, png: &[u8], max_dim: usize) -> Layer {
        // A checked-in asset failing to decode costs the layer, not the map;
        // an empty pyramid samples as zero everywhere and says so on stderr.
        let gray = match image::load_from_memory_with_format(png, image::ImageFormat::Png) {
            Ok(img) => img.to_luma8(),
            Err(e) => {
                eprintln!("sdroxide: decoding {label}: {e}");
                image::GrayImage::from_raw(1, 1, vec![0u8]).expect("1×1")
            }
        };
        let (mut w, mut h) = (gray.width() as usize, gray.height() as usize);
        let mut gray = gray.into_raw();
        while w > max_dim || h > max_dim {
            (w, h, gray) = halve(w, h, &gray);
        }
        let mut levels = vec![Level::pack(w, h, &gray)];
        while w > 1 || h > 1 {
            (w, h, gray) = halve(w, h, &gray);
            levels.push(Level::pack(w, h, &gray));
        }
        Layer { levels }
    }

    /// A sampler for a dot grid whose cells are `deg` of longitude wide.
    ///
    /// Which level that lands on is the usual mip choice — the one whose texels
    /// are about the size of a cell — and the two straddling levels are blended
    /// rather than switched between, because these maps *ease* their zoom and a
    /// level that changed in one frame would pop the whole coastline.
    pub fn sampler(&self, deg: f64) -> Sampler<'_> {
        let base = 360.0 / self.levels[0].w as f64;
        let lambda = (deg / base).max(1e-9).log2();
        let lo = (lambda.floor().max(0.0) as usize).min(self.levels.len() - 1);
        let hi = (lo + 1).min(self.levels.len() - 1);
        let t = if lambda <= lo as f64 { 0.0 } else { (lambda - lo as f64).min(1.0) as f32 };
        Sampler {
            lo: &self.levels[lo],
            hi: &self.levels[hi],
            t,
            mag: ((360.0 / self.levels[lo].w as f64) / deg) as f32,
            gain: (self.levels[0].w / self.levels[lo].w) as f32,
        }
    }
}

/// A [`Layer`] bound to one zoom: the two levels it reads and how far the dot
/// grid is from them.
pub struct Sampler<'a> {
    lo: &'a Level,
    hi: &'a Level,
    t: f32,
    mag: f32,
    gain: f32,
}

impl Sampler<'_> {
    /// Coverage at (lon, lat), 0…1.
    pub fn at(&self, lon: f64, lat: f64) -> f32 {
        let a = self.lo.bilinear(lon, lat);
        if self.t < 0.02 { a } else { a + (self.hi.bilinear(lon, lat) - a) * self.t }
    }

    /// How many dot cells one texel of the level being read covers. Above 1 the
    /// dots have outrun the map data and a feature's edge is being drawn from
    /// interpolation rather than from anything measured.
    pub fn magnification(&self) -> f32 {
        self.mag
    }

    /// How much a one-texel line has been thinned by the reduction to this
    /// level. Box-averaging halves a line's coverage per level while leaving
    /// its length alone, so this is what undoes that.
    pub fn line_gain(&self) -> f32 {
        self.gain
    }
}

/// One mip level down: a 2×2 box filter, with the odd row/column carried over
/// rather than dropped.
fn halve(w: usize, h: usize, src: &[u8]) -> (usize, usize, Vec<u8>) {
    let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
    let mut out = vec![0u8; nw * nh];
    for y in 0..nh {
        for x in 0..nw {
            let (x0, y0) = (x * 2, y * 2);
            let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
            let at = |x: usize, y: usize| u32::from(src[y * w + x]);
            let sum = at(x0, y0) + at(x1, y0) + at(x0, y1) + at(x1, y1);
            out[y * nw + x] = ((sum + 2) / 4) as u8;
        }
    }
    (nw, nh, out)
}

/// Everything the flat maps draw the world from.
pub struct World {
    pub land: Layer,
    pub borders: Layer,
    pub rivers: Layer,
}

/// The decoded rasters, built on the first map frame and kept for the session.
pub fn world() -> &'static World {
    static WORLD: OnceLock<World> = OnceLock::new();
    WORLD.get_or_init(|| World {
        land: Layer::build("land.png", LAND_PNG, MAX_DIM),
        borders: Layer::build("borders.png", BORDER_PNG, MAX_DIM),
        rivers: Layer::build("rivers.png", RIVER_PNG, MAX_DIM),
    })
}

/// Start the decode on a thread of its own, so the first map frame does not
/// have to wait for it.
///
/// Fire-and-forget: whichever of the two threads reaches the data first does
/// the work and the other waits, which is all [`OnceLock`] is being asked for
/// here. In the browser there is no second thread to do it on, and the tab
/// pays for it once on the frame that needs it.
pub fn prime() {
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::spawn(|| {
        world();
        cities();
    });
}

/// True if the (lon, lat) point in degrees is over land.
///
/// Read off the finest grid kept, so it agrees with the coastline the maps
/// draw rather than approximating it from a coarser one.
pub fn is_land(lon: f64, lat: f64) -> bool {
    world().land.levels[0].bilinear(lon, lat) >= 0.5
}

// ── Cities ──────────────────────────────────────────────────────────────────

/// One populated place from Natural Earth.
pub struct City {
    pub lat: f64,
    pub lon: f64,
    /// The largest population figure Natural Earth carries for the place, or 0
    /// where it has none.
    pub pop: u32,
    /// A national capital, which a map is expected to mark whatever its size.
    pub capital: bool,
    /// The ASCII spelling. The UI ships one Latin font, and a name rendered as
    /// a row of empty boxes would be worse than a transliterated one.
    pub name: &'static str,
}

/// Every populated place, largest first.
///
/// The order is the whole interface: there is no room for seven thousand
/// labels, so a map walks this list, draws what falls inside its view and stops
/// when it has enough. Which cities show is then a consequence of how far in
/// the map is zoomed, with no threshold to pick and nothing to re-sort.
pub fn cities() -> &'static [City] {
    static CITIES: OnceLock<Vec<City>> = OnceLock::new();
    CITIES.get_or_init(|| parse_cities(CITY_BIN))
}

/// See `make_earth_maps.py`'s `build_places`: a magic, a count, then one
/// variable-length record per place.
fn parse_cities(blob: &'static [u8]) -> Vec<City> {
    let Some(rest) = blob.strip_prefix(b"SDXCITY1") else {
        eprintln!("sdroxide: cities.bin is not a city table");
        return Vec::new();
    };
    let Some((count, mut rest)) = rest.split_at_checked(4) else { return Vec::new() };
    let count = u32::from_le_bytes(count.try_into().expect("4 bytes")) as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let Some((head, tail)) = rest.split_at_checked(14) else { break };
        let n = head[13] as usize;
        let Some((name, tail)) = tail.split_at_checked(n) else { break };
        let num = |a: usize| i32::from_le_bytes(head[a..a + 4].try_into().expect("4 bytes"));
        out.push(City {
            lat: f64::from(num(0)) / 1e5,
            lon: f64::from(num(4)) / 1e5,
            pop: u32::from_le_bytes(head[8..12].try_into().expect("4 bytes")),
            capital: head[12] & 1 != 0,
            name: std::str::from_utf8(name).unwrap_or("?"),
        });
        rest = tail;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mask has to have the continents where the continents are — this is
    /// what every marker on every flat map is placed against.
    #[test]
    fn land_is_where_land_is() {
        for (lon, lat) in [(-100.0, 40.0), (10.0, 50.0), (135.0, -25.0)] {
            assert!(is_land(lon, lat), "{lon},{lat} should be land");
        }
        for (lon, lat) in [(-140.0, 0.0), (-30.0, 30.0), (80.0, -40.0)] {
            assert!(!is_land(lon, lat), "{lon},{lat} should be sea");
        }
    }

    /// Every level of every pyramid is half the one above it, down to a single
    /// texel — a gap would make `sampler` read past the end at some zoom.
    #[test]
    fn the_pyramids_run_all_the_way_down() {
        for layer in [&world().land, &world().borders, &world().rivers] {
            let mut prev: Option<&Level> = None;
            for l in &layer.levels {
                assert_eq!(l.cov.len(), (l.w * l.h).div_ceil(2));
                if let Some(p) = prev {
                    assert_eq!((l.w, l.h), ((p.w / 2).max(1), (p.h / 2).max(1)));
                }
                prev = Some(l);
            }
            let last = layer.levels.last().expect("a level");
            assert_eq!((last.w, last.h), (1, 1));
            assert!(layer.levels[0].w <= MAX_DIM);
        }
    }

    /// A sampler asks for the level whose texels match the dot grid, and never
    /// walks off either end of the pyramid.
    #[test]
    fn zoom_picks_a_level_and_stays_inside_the_pyramid() {
        let land = &world().land;
        // Zoomed further in than the data goes: the base level, undiluted.
        let s = land.sampler(1e-6);
        assert!(std::ptr::eq(s.lo, &land.levels[0]));
        assert_eq!(s.line_gain(), 1.0);
        assert!(s.magnification() > 1.0);
        // ...and further out than the whole world.
        let s = land.sampler(720.0);
        assert!(std::ptr::eq(s.hi, land.levels.last().expect("a level")));
        assert!(s.magnification() < 1.0);
        // A dot the size of one base texel sits at the bottom of the pyramid.
        let s = land.sampler(360.0 / land.levels[0].w as f64);
        assert!(std::ptr::eq(s.lo, &land.levels[0]));
    }

    /// Coverage is a fraction: solid ground reads 1, open ocean 0, and a coast
    /// somewhere in between — that "in between" is what the maps draw their
    /// shoreline from.
    #[test]
    fn coverage_is_a_fraction() {
        let s = world().land.sampler(0.01);
        assert!(s.at(100.0, 45.0) > 0.99, "central Asia: {}", s.at(100.0, 45.0));
        assert!(s.at(-140.0, 0.0) < 0.01, "mid-Pacific: {}", s.at(-140.0, 0.0));
        // Longitude wraps rather than clamping, so ±180 is one place.
        let (a, b) = (s.at(-179.999, 65.0), s.at(179.999, 65.0));
        assert!((a - b).abs() < 0.35, "the date line is a seam: {a} vs {b}");
    }

    /// What the decoded world costs, held for the session.
    ///
    /// Asserted rather than measured in passing, because the number is a
    /// decision — `MAX_DIM` — and a decision that is easy to change without
    /// noticing: one level up on the land pyramid is 45 MB rather than 11, and
    /// it would be handed to a browser tab as readily as to a workstation.
    #[test]
    fn the_world_fits_in_its_budget() {
        let w = world();
        let bytes: usize = [&w.land, &w.borders, &w.rivers]
            .iter()
            .map(|l| l.levels.iter().map(|v| v.cov.len()).sum::<usize>())
            .sum();
        assert!(bytes < 20 << 20, "the map pyramids grew to {} MB", bytes >> 20);
    }

    /// The table is sorted, spelled in ASCII, and has the places in it a
    /// world map is expected to name.
    #[test]
    fn the_city_table_is_a_city_table() {
        let cities = cities();
        assert!(cities.len() > 5000, "only {} cities", cities.len());
        assert!(cities.windows(2).all(|w| w[0].pop >= w[1].pop), "not sorted by population");
        assert!(cities.iter().all(|c| c.name.is_ascii() && !c.name.is_empty()));
        assert!(cities.iter().all(|c| (-90.0..=90.0).contains(&c.lat)));
        assert!(cities.iter().all(|c| (-180.0..=180.0).contains(&c.lon)));
        let tokyo = cities.iter().find(|c| c.name == "Tokyo").expect("Tokyo");
        assert!((tokyo.lat - 35.69).abs() < 0.2 && (tokyo.lon - 139.75).abs() < 0.2);
        assert!(cities.iter().take(60).any(|c| c.name == "London"));
        assert!(cities.iter().filter(|c| c.capital).count() > 150);
    }
}
