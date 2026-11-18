// muninn-maps — Apple Maps for Muninn's map tools, with no API key.
//
// Compiled by build.rs into binaries/muninn-maps-<triple> and bundled through tauri
// `externalBin`, like the transcriber. Muninn (maps.rs) runs it once per request:
//
//   muninn-maps <command>     request JSON on stdin  →  result JSON on stdout
//
// Commands:
//   geocode     {query}                                  → {place}
//   search      {query, near?, radius_m?, limit?}         → {results: [place]}
//   directions  {from, to, mode?, near?}                  → route (see below)
//   snapshot    {out, width?, height?, dark?, markers?, route?}  → {path}
//
// A place is {name, address, lat, lon, category?, phone?, url?, distance_m?}; `near` is
// {lat, lon}. On failure it prints {"error": "..."} and exits 1. Everything goes through
// MapKit / CoreLocation, which use the system's Apple Maps service — no key, no account.

import AppKit
import CoreLocation
import Foundation
import MapKit

struct Failure: Error { let message: String }

func fail(_ message: String) -> Failure { Failure(message: message) }

func emit(_ object: Any) {
    let data = (try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])) ?? Data("{}".utf8)
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write(Data("\n".utf8))
}

// MARK: - Request helpers

func string(_ req: [String: Any], _ key: String) -> String? {
    (req[key] as? String)?.trimmingCharacters(in: .whitespacesAndNewlines).nilIfEmpty
}

func number(_ req: [String: Any], _ key: String) -> Double? {
    if let d = req[key] as? Double { return d }
    if let i = req[key] as? Int { return Double(i) }
    if let s = req[key] as? String { return Double(s) }
    return nil
}

func coordinate(_ any: Any?) -> CLLocationCoordinate2D? {
    guard let obj = any as? [String: Any],
          let lat = (obj["lat"] as? Double) ?? (obj["lat"] as? Int).map(Double.init),
          let lon = (obj["lon"] as? Double) ?? (obj["lon"] as? Int).map(Double.init),
          CLLocationCoordinate2DIsValid(CLLocationCoordinate2D(latitude: lat, longitude: lon))
    else { return nil }
    return CLLocationCoordinate2D(latitude: lat, longitude: lon)
}

/// "32.7,-117.1" typed as a place → that coordinate.
func parseLatLon(_ text: String) -> CLLocationCoordinate2D? {
    let parts = text.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }
    guard parts.count == 2, let lat = Double(parts[0]), let lon = Double(parts[1]) else { return nil }
    let c = CLLocationCoordinate2D(latitude: lat, longitude: lon)
    return CLLocationCoordinate2DIsValid(c) ? c : nil
}

extension String {
    var nilIfEmpty: String? { isEmpty ? nil : self }
}

// MARK: - Places

func address(of placemark: CLPlacemark) -> String {
    var lines: [String] = []
    let street = [placemark.subThoroughfare, placemark.thoroughfare].compactMap { $0 }.joined(separator: " ")
    if !street.isEmpty { lines.append(street) }
    if let city = placemark.locality { lines.append(city) }
    let region = [placemark.administrativeArea, placemark.postalCode].compactMap { $0 }.joined(separator: " ")
    if !region.isEmpty { lines.append(region) }
    if lines.isEmpty, let country = placemark.country { lines.append(country) }
    return lines.joined(separator: ", ")
}

func placeJSON(_ item: MKMapItem, from origin: CLLocationCoordinate2D? = nil) -> [String: Any] {
    let c = item.placemark.coordinate
    var out: [String: Any] = [
        "name": item.name ?? address(of: item.placemark),
        "address": address(of: item.placemark),
        "lat": c.latitude,
        "lon": c.longitude,
    ]
    if let category = item.pointOfInterestCategory?.rawValue {
        out["category"] = category.replacingOccurrences(of: "MKPOICategory", with: "")
    }
    if let phone = item.phoneNumber { out["phone"] = phone }
    if let url = item.url?.absoluteString { out["url"] = url }
    if let origin {
        let d = CLLocation(latitude: c.latitude, longitude: c.longitude)
            .distance(from: CLLocation(latitude: origin.latitude, longitude: origin.longitude))
        out["distance_m"] = d.rounded()
    }
    return out
}

func localSearch(_ query: String, near: CLLocationCoordinate2D?, radius: Double) async throws -> [MKMapItem] {
    let request = MKLocalSearch.Request()
    request.naturalLanguageQuery = query
    if let near {
        request.region = MKCoordinateRegion(center: near, latitudinalMeters: radius * 2, longitudinalMeters: radius * 2)
    }
    do {
        return try await MKLocalSearch(request: request).start().mapItems
    } catch let error as MKError where error.code == .placemarkNotFound {
        return []
    }
}

/// Resolve free text (an address, a business name, a city, or "lat,lon") to one map item.
/// Addresses go through the geocoder; names fall back to a place search near `near`.
func resolve(_ text: String, near: CLLocationCoordinate2D?) async throws -> MKMapItem {
    if let c = parseLatLon(text) {
        let item = MKMapItem(placemark: MKPlacemark(coordinate: c))
        item.name = text
        return item
    }
    if let placemark = try? await CLGeocoder().geocodeAddressString(text).first, placemark.location != nil {
        let item = MKMapItem(placemark: MKPlacemark(placemark: placemark))
        if item.name == nil || item.name?.isEmpty == true { item.name = text }
        return item
    }
    if let item = try await localSearch(text, near: near, radius: 25_000).first {
        return item
    }
    throw fail("Could not find '\(text)' on Apple Maps")
}

// MARK: - Commands

func geocode(_ req: [String: Any]) async throws -> [String: Any] {
    guard let query = string(req, "query") else { throw fail("missing query") }
    let item = try await resolve(query, near: coordinate(req["near"]))
    return ["place": placeJSON(item)]
}

func search(_ req: [String: Any]) async throws -> [String: Any] {
    guard let query = string(req, "query") else { throw fail("missing query") }
    let near = coordinate(req["near"])
    let radius = max(500, min(number(req, "radius_m") ?? 5_000, 50_000))
    let limit = max(1, min(Int(number(req, "limit") ?? 5), 25))
    let items = try await localSearch(query, near: near, radius: radius)
    return ["results": items.prefix(limit).map { placeJSON($0, from: near) }]
}

/// At most `max` points, evenly sampled, always keeping the first and last.
func downsample(_ points: [CLLocationCoordinate2D], max: Int) -> [CLLocationCoordinate2D] {
    guard points.count > max, max >= 2 else { return points }
    let step = Double(points.count - 1) / Double(max - 1)
    return (0..<max).map { points[Int((Double($0) * step).rounded())] }
}

func directions(_ req: [String: Any]) async throws -> [String: Any] {
    guard let fromText = string(req, "from") else { throw fail("missing from") }
    guard let toText = string(req, "to") else { throw fail("missing to") }
    let near = coordinate(req["near"])
    let mode = (string(req, "mode") ?? "driving").lowercased()
    let transport: MKDirectionsTransportType
    switch mode {
    case "walking", "walk", "foot": transport = .walking
    case "cycling", "cycle", "bike", "bicycle": transport = .cycling
    case "transit", "public_transport", "bus", "train": transport = .transit
    default: transport = .automobile
    }
    let source = try await resolve(fromText, near: near)
    let destination = try await resolve(toText, near: near ?? source.placemark.coordinate)

    let request = MKDirections.Request()
    request.source = source
    request.destination = destination
    request.transportType = transport

    var out: [String: Any] = [
        "from": placeJSON(source),
        "to": placeJSON(destination),
        "mode": transport == .walking ? "walking" : transport == .cycling ? "cycling" : transport == .transit ? "transit" : "driving",
    ]

    let modeName = out["mode"] as? String ?? mode
    // Cycling and transit exist only where Apple has that coverage; say so instead of
    // MapKit's bare "Directions are not available".
    func noRoute(_ error: Error) -> Failure {
        if transport == .cycling || transport == .transit {
            return fail("Apple Maps has no \(modeName) route between these places (\(modeName) is only covered in some cities). Try driving or walking.")
        }
        return fail("Apple Maps found no \(modeName) route: \(error.localizedDescription)")
    }

    // Apple only gives transit as a travel-time estimate — no turn-by-turn steps or path.
    if transport == .transit {
        let eta: MKDirections.ETAResponse
        do { eta = try await MKDirections(request: request).calculateETA() } catch { throw noRoute(error) }
        out["duration_s"] = eta.expectedTravelTime.rounded()
        out["distance_m"] = eta.distance.rounded()
        out["steps"] = [[String: Any]]()
        out["polyline"] = [[Double]]()
        out["note"] = "Apple Maps provides transit travel time only, without step-by-step directions."
        return out
    }

    let response: MKDirections.Response
    do { response = try await MKDirections(request: request).calculate() } catch { throw noRoute(error) }
    guard let route = response.routes.first else { throw fail("No route found") }
    out["distance_m"] = route.distance.rounded()
    out["duration_s"] = route.expectedTravelTime.rounded()
    out["steps"] = route.steps
        .filter { !$0.instructions.isEmpty }
        .map { ["instruction": $0.instructions, "distance_m": $0.distance.rounded()] as [String: Any] }
    let count = route.polyline.pointCount
    var coords = [CLLocationCoordinate2D](repeating: kCLLocationCoordinate2DInvalid, count: count)
    route.polyline.getCoordinates(&coords, range: NSRange(location: 0, length: count))
    out["polyline"] = downsample(coords, max: 400).map { [$0.latitude, $0.longitude] }
    return out
}

// MARK: - Snapshot

func color(_ name: String?) -> NSColor {
    switch name {
    case "green": return NSColor(srgbRed: 0.13, green: 0.77, blue: 0.37, alpha: 1)
    case "red": return NSColor(srgbRed: 0.94, green: 0.27, blue: 0.27, alpha: 1)
    default: return NSColor(srgbRed: 0.23, green: 0.51, blue: 0.96, alpha: 1)
    }
}

/// A region around every point, padded so pins and the route aren't cut off at the edges.
func region(for points: [CLLocationCoordinate2D]) -> MKCoordinateRegion {
    guard let first = points.first else {
        return MKCoordinateRegion(center: CLLocationCoordinate2D(latitude: 39.5, longitude: -98.35),
                                  span: MKCoordinateSpan(latitudeDelta: 30, longitudeDelta: 50))
    }
    if points.count == 1 {
        return MKCoordinateRegion(center: first, latitudinalMeters: 1_500, longitudinalMeters: 1_500)
    }
    let lats = points.map(\.latitude), lons = points.map(\.longitude)
    let center = CLLocationCoordinate2D(latitude: (lats.min()! + lats.max()!) / 2,
                                        longitude: (lons.min()! + lons.max()!) / 2)
    let span = MKCoordinateSpan(latitudeDelta: max((lats.max()! - lats.min()!) * 1.35, 0.005),
                                longitudeDelta: max((lons.max()! - lons.min()!) * 1.35, 0.005))
    return MKCoordinateRegion(center: center, span: span)
}

func snapshot(_ req: [String: Any]) async throws -> [String: Any] {
    guard let out = string(req, "out") else { throw fail("missing out") }
    let width = max(200, min(number(req, "width") ?? 640, 1600))
    let height = max(150, min(number(req, "height") ?? 360, 1200))
    let markers = (req["markers"] as? [[String: Any]]) ?? []
    let route = ((req["route"] as? [[Double]]) ?? []).compactMap { p -> CLLocationCoordinate2D? in
        p.count >= 2 ? CLLocationCoordinate2D(latitude: p[0], longitude: p[1]) : nil
    }
    let markerCoords = markers.compactMap { coordinate($0) }

    let options = MKMapSnapshotter.Options()
    options.region = region(for: markerCoords + route)
    options.size = NSSize(width: width, height: height)
    options.appearance = NSAppearance(named: (req["dark"] as? Bool ?? false) ? .darkAqua : .aqua)
    let shot = try await MKMapSnapshotter(options: options).start()

    let image = NSImage(size: options.size)
    image.lockFocus()
    shot.image.draw(at: .zero, from: .zero, operation: .copy, fraction: 1)

    if route.count >= 2 {
        let path = NSBezierPath()
        path.lineWidth = 5
        path.lineJoinStyle = .round
        path.lineCapStyle = .round
        path.move(to: shot.point(for: route[0]))
        for c in route.dropFirst() { path.line(to: shot.point(for: c)) }
        NSColor.white.withAlphaComponent(0.85).setStroke()
        path.lineWidth = 8
        path.stroke()
        color(nil).setStroke()
        path.lineWidth = 5
        path.stroke()
    }

    for marker in markers {
        guard let c = coordinate(marker) else { continue }
        let p = shot.point(for: c)
        let r: CGFloat = 11
        let dot = NSBezierPath(ovalIn: NSRect(x: p.x - r, y: p.y - r, width: r * 2, height: r * 2))
        color(marker["color"] as? String).setFill()
        dot.fill()
        NSColor.white.setStroke()
        dot.lineWidth = 2.5
        dot.stroke()
        if let label = marker["label"] as? String, !label.isEmpty {
            let text = NSAttributedString(string: String(label.prefix(2)), attributes: [
                .font: NSFont.boldSystemFont(ofSize: 11),
                .foregroundColor: NSColor.white,
            ])
            let size = text.size()
            text.draw(at: NSPoint(x: p.x - size.width / 2, y: p.y - size.height / 2))
        }
    }
    image.unlockFocus()

    guard let tiff = image.tiffRepresentation,
          let rep = NSBitmapImageRep(data: tiff),
          let png = rep.representation(using: .png, properties: [:])
    else { throw fail("could not encode the map image") }
    try png.write(to: URL(fileURLWithPath: out))
    return ["path": out]
}

// MARK: - Entry

@main
struct MuninnMaps {
    static func main() async {
        let command = CommandLine.arguments.dropFirst().first ?? ""
        let input = FileHandle.standardInput.readDataToEndOfFile()
        let request = ((try? JSONSerialization.jsonObject(with: input)) as? [String: Any]) ?? [:]
        do {
            let result: [String: Any]
            switch command {
            case "geocode": result = try await geocode(request)
            case "search": result = try await search(request)
            case "directions": result = try await directions(request)
            case "snapshot": result = try await snapshot(request)
            default: throw fail("unknown command '\(command)' (use geocode, search, directions or snapshot)")
            }
            emit(result)
        } catch let failure as Failure {
            emit(["error": failure.message])
            exit(1)
        } catch {
            emit(["error": error.localizedDescription])
            exit(1)
        }
    }
}
