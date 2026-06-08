//! Discord map viewport from ME corner zones + CFG width (Mapbox Static Images).

use anyhow::{bail, Result};
use dcso3::coord::LLPos;
use serde::Serialize;

/// ME trigger zone: northwest corner of the Discord map crop (zone center).
pub const SETTINGS_DISCORD_MAP_NW: &str = "SETTINGS-discord-map-nw";

/// ME trigger zone: southeast corner of the Discord map crop (zone center).
pub const SETTINGS_DISCORD_MAP_SE: &str = "SETTINGS-discord-map-se";

/// Mapbox Static Images API max `width` / `height` request parameter.
pub const MAPBOX_STATIC_MAX_PX: u32 = 1280;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct MapViewport {
    /// `[lon_min, lat_min, lon_max, lat_max]`
    pub bbox: [f64; 4],
    pub width: u32,
    pub height: u32,
    #[serde(skip)]
    mercator: MercatorBbox,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct MercatorBbox {
    x_min: f64,
    x_max: f64,
    y_min: f64,
    y_max: f64,
}

fn mercator_x(lon: f64) -> f64 {
    lon.to_radians()
}

fn mercator_y(lat: f64) -> f64 {
    let r = lat.to_radians();
    ((std::f64::consts::FRAC_PI_4) + r * 0.5).tan().ln()
}

/// Build viewport from ME corner lat/lon and requested image width.
pub fn viewport_from_corners(nw: LLPos, se: LLPos, width_px: u32) -> Result<MapViewport> {
    if width_px == 0 {
        bail!("discord_map.width must be > 0");
    }
    if width_px > MAPBOX_STATIC_MAX_PX {
        bail!(
            "discord_map.width {width_px} exceeds Mapbox Static Images limit ({MAPBOX_STATIC_MAX_PX}); reduce width in CFG"
        );
    }
    let lon_min = nw.longitude.min(se.longitude);
    let lon_max = nw.longitude.max(se.longitude);
    let lat_min = nw.latitude.min(se.latitude);
    let lat_max = nw.latitude.max(se.latitude);
    if lon_max <= lon_min || lat_max <= lat_min {
        bail!(
            "discord map corner zones overlap or are identical (nw=({}, {}), se=({}, {}))",
            nw.latitude,
            nw.longitude,
            se.latitude,
            se.longitude
        );
    }
    let mercator = MercatorBbox {
        x_min: mercator_x(lon_min),
        x_max: mercator_x(lon_max),
        y_min: mercator_y(lat_min),
        y_max: mercator_y(lat_max),
    };
    let mw = mercator.x_max - mercator.x_min;
    let mh = mercator.y_max - mercator.y_min;
    if mw <= 0.0 || mh <= 0.0 {
        bail!("discord map corner zones produced an empty mercator bbox");
    }
    let height_f = f64::from(width_px) * (mh / mw);
    let height_px = height_f
        .round()
        .clamp(1.0, f64::from(MAPBOX_STATIC_MAX_PX)) as u32;
    if height_px > MAPBOX_STATIC_MAX_PX {
        bail!(
            "discord map computed height {height_px}px exceeds Mapbox limit ({MAPBOX_STATIC_MAX_PX}) for width {width_px}px; reduce discord_map.width in CFG"
        );
    }
    if (height_f - f64::from(height_px)).abs() > 0.51 {
        bail!(
            "discord map computed height {height_f:.1}px rounds outside Mapbox limits; reduce discord_map.width in CFG"
        );
    }
    Ok(MapViewport {
        bbox: [lon_min, lat_min, lon_max, lat_max],
        width: width_px,
        height: height_px,
        mercator,
    })
}

impl MapViewport {
    pub fn lon_min(&self) -> f64 {
        self.bbox[0]
    }

    pub fn lat_min(&self) -> f64 {
        self.bbox[1]
    }

    pub fn lon_max(&self) -> f64 {
        self.bbox[2]
    }

    pub fn lat_max(&self) -> f64 {
        self.bbox[3]
    }

    /// Web Mercator pixel position in logical viewport pixels (top-left origin).
    pub fn ll_to_pixel(&self, lat: f64, lon: f64) -> (f32, f32) {
        let mx = mercator_x(lon);
        let my = mercator_y(lat);
        let w = f64::from(self.width);
        let h = f64::from(self.height);
        let x = (mx - self.mercator.x_min) / (self.mercator.x_max - self.mercator.x_min) * w;
        let y = (self.mercator.y_max - my) / (self.mercator.y_max - self.mercator.y_min) * h;
        (x as f32, y as f32)
    }

    /// Map lat/lon to pixel coords in an actual raster (e.g. Mapbox `@2x` base PNG).
    pub fn ll_to_pixel_in(&self, lat: f64, lon: f64, img_w: u32, img_h: u32) -> (f32, f32) {
        let (x, y) = self.ll_to_pixel(lat, lon);
        let sx = img_w as f32 / self.width as f32;
        let sy = img_h as f32 / self.height as f32;
        (x * sx, y * sy)
    }

    pub fn mapbox_static_url(
        &self,
        style: &str,
        access_token: &str,
        retina: bool,
        padding: u32,
    ) -> String {
        let bbox = format!(
            "{},{},{},{}",
            self.lon_min(),
            self.lat_min(),
            self.lon_max(),
            self.lat_max()
        );
        let size = if retina {
            format!("{}x{}@2x", self.width, self.height)
        } else {
            format!("{}x{}", self.width, self.height)
        };
        let pad = if padding > 0 {
            format!("&padding={padding}")
        } else {
            String::new()
        };
        format!(
            "https://api.mapbox.com/styles/v1/{style}/static/[{bbox}]/{size}?access_token={access_token}{pad}"
        )
    }
}

/// Build-time check: both corner zones exist in the base mission.
pub fn validate_corner_zones_present<'a>(
    zone_names: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    let mut nw = false;
    let mut se = false;
    for name in zone_names {
        match name {
            SETTINGS_DISCORD_MAP_NW => nw = true,
            SETTINGS_DISCORD_MAP_SE => se = true,
            _ => (),
        }
    }
    if !nw {
        bail!("missing ME trigger zone {SETTINGS_DISCORD_MAP_NW}");
    }
    if !se {
        bail!("missing ME trigger zone {SETTINGS_DISCORD_MAP_SE}");
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn ll_to_pixel_in_scales_to_raster_size() {
        let nw = LLPos {
            latitude: 45.0,
            longitude: 37.0,
            altitude: 0.,
        };
        let se = LLPos {
            latitude: 41.0,
            longitude: 45.0,
            altitude: 0.,
        };
        let vp = viewport_from_corners(nw, se, 100).unwrap();
        let mid_lat = (nw.latitude + se.latitude) * 0.5;
        let mid_lon = (nw.longitude + se.longitude) * 0.5;
        let (x, y) = vp.ll_to_pixel(mid_lat, mid_lon);
        let (x2, y2) = vp.ll_to_pixel_in(mid_lat, mid_lon, vp.width * 2, vp.height * 2);
        assert!((x2 - x * 2.0).abs() < 0.01);
        assert!((y2 - y * 2.0).abs() < 0.01);
    }

    #[test]
    fn caucasus_like_corners_produce_height_from_width() {
        let nw = LLPos {
            latitude: 45.5,
            longitude: 37.8,
            altitude: 0.,
        };
        let se = LLPos {
            latitude: 41.4,
            longitude: 45.1,
            altitude: 0.,
        };
        let vp = viewport_from_corners(nw, se, 1280).unwrap();
        assert_eq!(vp.width, 1280);
        assert!(vp.height > 0 && vp.height <= MAPBOX_STATIC_MAX_PX);
        assert!(vp.height < vp.width);
    }
}
