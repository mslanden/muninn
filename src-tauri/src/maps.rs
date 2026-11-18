//! Apple Maps for the map tools and the user's location, through the `muninn-maps` Swift
//! sidecar (native/maps.swift — MapKit + CoreLocation). No API key or account: MapKit uses
//! the system's Apple Maps service. Each call runs the helper once with a JSON request on
//! stdin and reads a JSON result from stdout (`{"error": "..."}` on failure).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

/// Longest one helper call may take (a slow first geocode, a long route, a big snapshot).
const HELPER_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LatLon {
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Place {
    pub name: String,
    #[serde(default)]
    pub address: String,
    pub lat: f64,
    pub lon: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distance_m: Option<f64>,
}

impl Place {
    /// "Name, address" (just the name when the address adds nothing).
    pub fn display(&self) -> String {
        if self.address.is_empty() || self.name.contains(&self.address) {
            self.name.clone()
        } else {
            format!("{}, {}", self.name, self.address)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteStep {
    pub instruction: String,
    #[serde(default)]
    pub distance_m: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub from: Place,
    pub to: Place,
    pub mode: String,
    pub distance_m: f64,
    pub duration_s: f64,
    #[serde(default)]
    pub steps: Vec<RouteStep>,
    /// [lat, lon] points along the route (empty for transit, which Apple gives as ETA only).
    #[serde(default)]
    pub polyline: Vec<[f64; 2]>,
    #[serde(default)]
    pub note: Option<String>,
}

/// Run one helper command and return its JSON result.
async fn run(command: &str, request: &Value) -> Result<Value, String> {
    let helper = crate::sidecar::helper_path("muninn-maps").ok_or_else(|| {
        "Apple Maps helper not found (it is compiled by build.rs — rebuild with Xcode installed)".to_string()
    })?;
    let mut child = tokio::process::Command::new(&helper)
        .arg(command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start the Apple Maps helper: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(request.to_string().as_bytes())
            .await
            .map_err(|e| format!("could not talk to the Apple Maps helper: {e}"))?;
        // Dropping stdin closes it, which is how the helper knows the request is complete.
    }
    let output = match tokio::time::timeout(HELPER_TIMEOUT, child.wait_with_output()).await {
        Ok(result) => result.map_err(|e| format!("Apple Maps helper failed: {e}"))?,
        Err(_) => return Err(format!("Apple Maps did not answer within {}s", HELPER_TIMEOUT.as_secs())),
    };
    parse_reply(&output.stdout, &output.stderr)
}

/// The helper's JSON reply as a result: its `error` field becomes the error message.
fn parse_reply(stdout: &[u8], stderr: &[u8]) -> Result<Value, String> {
    let value: Value = serde_json::from_slice(stdout).map_err(|_| {
        let err = String::from_utf8_lossy(stderr);
        format!("Apple Maps helper gave no answer{}", if err.trim().is_empty() { String::new() } else { format!(": {}", err.trim()) })
    })?;
    match value.get("error").and_then(Value::as_str) {
        Some(e) => Err(e.to_string()),
        None => Ok(value),
    }
}

fn near_json(near: Option<&LatLon>) -> Value {
    near.map(|n| json!({"lat": n.lat, "lon": n.lon})).unwrap_or(Value::Null)
}

/// Find one place for free text: an address, a city, a business name, or "lat,lon".
pub async fn geocode(query: &str, near: Option<&LatLon>) -> Result<Place, String> {
    let reply = run("geocode", &json!({"query": query, "near": near_json(near)})).await?;
    serde_json::from_value(reply["place"].clone()).map_err(|e| format!("unexpected Apple Maps reply: {e}"))
}

/// Places matching a query ("coffee", "Home Depot", "mexican restaurant"), nearest-first
/// relevance around `near` when given.
pub async fn search(query: &str, near: Option<&LatLon>, radius_m: f64, limit: usize) -> Result<Vec<Place>, String> {
    let reply = run(
        "search",
        &json!({"query": query, "near": near_json(near), "radius_m": radius_m, "limit": limit}),
    )
    .await?;
    serde_json::from_value(reply["results"].clone()).map_err(|e| format!("unexpected Apple Maps reply: {e}"))
}

/// A route between two places (free text, resolved like `geocode`).
pub async fn directions(from: &str, to: &str, mode: &str, near: Option<&LatLon>) -> Result<Route, String> {
    let reply = run(
        "directions",
        &json!({"from": from, "to": to, "mode": mode, "near": near_json(near)}),
    )
    .await?;
    serde_json::from_value(reply).map_err(|e| format!("unexpected Apple Maps reply: {e}"))
}

#[derive(Debug, Clone, Deserialize)]
pub struct Marker {
    pub lat: f64,
    pub lon: f64,
    #[serde(default)]
    pub label: String,
    /// "blue" (default), "green" or "red".
    #[serde(default)]
    pub color: String,
}

/// Render a map image (PNG bytes) framing the markers and route.
pub async fn snapshot(markers: &[Marker], route: &[[f64; 2]], dark: bool, width: u32, height: u32) -> Result<Vec<u8>, String> {
    let out = std::env::temp_dir().join(format!(
        "muninn-map-{}-{}.png",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let markers_json: Vec<Value> = markers
        .iter()
        .map(|m| json!({"lat": m.lat, "lon": m.lon, "label": m.label, "color": m.color}))
        .collect();
    let result = run(
        "snapshot",
        &json!({
            "out": out.display().to_string(),
            "width": width,
            "height": height,
            "dark": dark,
            "markers": markers_json,
            "route": route,
        }),
    )
    .await
    .and_then(|_| std::fs::read(&out).map_err(|e| format!("map image missing: {e}")));
    let _ = std::fs::remove_file(&out);
    result
}

/// Parse a stored `user/location_coords` value. It is kept as "lon,lat" (the order the
/// setting has always used).
pub fn parse_stored_coords(value: &str) -> Option<LatLon> {
    let (lon, lat) = value.split_once(',')?;
    let (lon, lat) = (lon.trim().parse::<f64>().ok()?, lat.trim().parse::<f64>().ok()?);
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some(LatLon { lat, lon })
}

/// `maps://` link that opens a place search in Apple Maps around `center`.
pub fn apple_maps_search_url(query: &str, center: Option<&LatLon>) -> String {
    let mut url = format!("maps://?q={}", urlencoding::encode(query));
    if let Some(c) = center {
        url.push_str(&format!("&sll={},{}", c.lat, c.lon));
    }
    url
}

/// `maps://` link that opens these directions in Apple Maps.
pub fn apple_maps_directions_url(route: &Route) -> String {
    let flag = match route.mode.as_str() {
        "walking" => "&dirflg=w",
        "transit" => "&dirflg=r",
        "driving" => "&dirflg=d",
        _ => "",
    };
    format!(
        "maps://?saddr={},{}&daddr={},{}{}",
        route.from.lat, route.from.lon, route.to.lat, route.to.lon, flag
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_errors_and_bad_output_become_messages() {
        assert_eq!(parse_reply(br#"{"error":"Could not find 'x'"}"#, b"").unwrap_err(), "Could not find 'x'");
        assert!(parse_reply(b"", b"dyld: missing").unwrap_err().contains("dyld: missing"));
        assert_eq!(parse_reply(br#"{"place":{"name":"A"}}"#, b"").unwrap()["place"]["name"], "A");
    }

    #[test]
    fn stored_coords_are_lon_lat() {
        assert_eq!(parse_stored_coords("-117.14,32.72"), Some(LatLon { lat: 32.72, lon: -117.14 }));
        assert_eq!(parse_stored_coords(" -117.14 , 32.72 "), Some(LatLon { lat: 32.72, lon: -117.14 }));
        assert_eq!(parse_stored_coords("32.72"), None);
        assert_eq!(parse_stored_coords("200,10"), None);
        assert_eq!(parse_stored_coords(""), None);
    }

    #[test]
    fn apple_maps_links() {
        assert_eq!(
            apple_maps_search_url("tacos & beer", Some(&LatLon { lat: 1.5, lon: 2.5 })),
            "maps://?q=tacos%20%26%20beer&sll=1.5,2.5"
        );
        let place = |lat, lon| Place { name: "x".into(), address: String::new(), lat, lon, category: None, phone: None, url: None, distance_m: None };
        let route = Route {
            from: place(1.0, 2.0),
            to: place(3.0, 4.0),
            mode: "walking".into(),
            distance_m: 10.0,
            duration_s: 60.0,
            steps: vec![],
            polyline: vec![],
            note: None,
        };
        assert_eq!(apple_maps_directions_url(&route), "maps://?saddr=1,2&daddr=3,4&dirflg=w");
    }

    #[test]
    fn display_skips_a_redundant_address() {
        let p = |name: &str, address: &str| Place { name: name.into(), address: address.into(), lat: 0.0, lon: 0.0, category: None, phone: None, url: None, distance_m: None };
        assert_eq!(p("Tacos El Gordo", "511 F St, San Diego").display(), "Tacos El Gordo, 511 F St, San Diego");
        assert_eq!(p("San Diego, CA", "San Diego, CA").display(), "San Diego, CA");
        assert_eq!(p("Balboa Park", "").display(), "Balboa Park");
    }

    /// Real Apple Maps lookups through the compiled helper. Needs network:
    /// `cargo test --lib maps -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn apple_maps_end_to_end() {
        let park = geocode("Balboa Park, San Diego, CA", None).await.unwrap();
        assert!((park.lat - 32.73).abs() < 0.05 && (park.lon + 117.15).abs() < 0.05, "{:?}", park);
        let near = LatLon { lat: park.lat, lon: park.lon };
        let tacos = search("tacos", Some(&near), 5_000.0, 3).await.unwrap();
        assert!(!tacos.is_empty() && tacos.len() <= 3);
        assert!(tacos.iter().all(|t| t.distance_m.is_some()));
        let route = directions("Balboa Park, San Diego", "San Diego Zoo", "walking", None).await.unwrap();
        assert_eq!(route.mode, "walking");
        assert!(route.distance_m > 100.0 && !route.steps.is_empty() && route.polyline.len() > 2);
        let png = snapshot(
            &[Marker { lat: park.lat, lon: park.lon, label: "1".into(), color: String::new() }],
            &route.polyline,
            true,
            640,
            360,
        )
        .await
        .unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        let err = geocode("zzqqxx-not-a-real-place-9183", None).await.unwrap_err();
        assert!(err.contains("Could not find"), "{}", err);
    }
}
