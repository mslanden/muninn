// muninn-framescan — on-device face detection for smart video reframing.
//
// Usage: muninn-framescan <video-file> [--fps N]   (default fps = 2)
// Prints JSON to stdout:
//   {"frames":[{"t":1.5,"faces":[{"x":0.32,"y":0.2,"w":0.1,"h":0.18,"c":0.98}]}]}
// Face rects are normalized to [0,1] with a TOP-LEFT origin and Y growing DOWN. Vision
// reports bottom-left-origin rects, so Y is converted here. `c` is the detector's
// confidence [0,1]. All diagnostics go to stderr; a nonzero exit signals a hard failure
// (bad path / undecodable video). A video with no faces still exits 0 with empty `faces`
// arrays — the Rust caller treats "no faces" as "fall back to a static center crop".
//
// Compiled by src-tauri/build.rs (compile_framescan) and bundled as a tauri sidecar;
// spawned from video.rs `scan_faces`. Uses only plain Vision + AVFoundation APIs
// (macOS 10.15+), so — unlike the SpeechAnalyzer transcriber — there is no SDK gating
// and no TCC prompt for file-based processing.

import AVFoundation
import CoreMedia
import Foundation
import Vision

struct JFace: Codable {
    let x: Double
    let y: Double
    let w: Double
    let h: Double
    let c: Double
}

struct JFrame: Codable {
    let t: Double
    let faces: [JFace]
}

struct JResult: Codable {
    let frames: [JFrame]
}

func warn(_ msg: String) {
    FileHandle.standardError.write(("muninn-framescan: " + msg + "\n").data(using: .utf8)!)
}

func die(_ msg: String) -> Never {
    warn(msg)
    exit(1)
}

/// Detect faces in a single decoded frame. Returns normalized TOP-LEFT-origin rects.
func detectFaces(in cgImage: CGImage) -> [JFace] {
    let request = VNDetectFaceRectanglesRequest()
    let handler = VNImageRequestHandler(cgImage: cgImage, options: [:])
    do {
        try handler.perform([request])
    } catch {
        warn("face request failed on a frame: \(error)")
        return []
    }
    let observations = request.results ?? []
    return observations.map { obs in
        let bb = obs.boundingBox  // normalized, bottom-left origin
        // Convert Y to a top-left origin (Vision's Y grows up).
        let yTop = 1.0 - Double(bb.origin.y) - Double(bb.height)
        return JFace(
            x: Double(bb.origin.x),
            y: yTop,
            w: Double(bb.width),
            h: Double(bb.height),
            c: Double(obs.confidence)
        )
    }
}

@main
struct Main {
    static func main() {
        let args = Array(CommandLine.arguments.dropFirst())
        var fps: Double = 2.0
        var pathArg: String? = nil

        var i = 0
        while i < args.count {
            let a = args[i]
            if a == "--fps" {
                i += 1
                if i < args.count, let v = Double(args[i]) {
                    fps = v
                } else {
                    die("--fps needs a numeric value")
                }
            } else if pathArg == nil {
                pathArg = a
            }
            i += 1
        }

        guard let path = pathArg, !path.isEmpty else {
            die("usage: muninn-framescan <video-file> [--fps N]")
        }
        // Clamp to a sane sampling range: too low misses motion, too high is slow.
        fps = min(max(fps, 0.2), 10.0)

        let url = URL(fileURLWithPath: path)
        guard FileManager.default.fileExists(atPath: url.path) else {
            die("video file not found: \(path)")
        }

        let asset = AVURLAsset(url: url)
        guard asset.tracks(withMediaType: .video).first != nil else {
            die("no video track in file: \(path)")
        }
        let durationS = CMTimeGetSeconds(asset.duration)
        guard durationS.isFinite, durationS > 0 else {
            die("could not read a positive duration from: \(path)")
        }

        let generator = AVAssetImageGenerator(asset: asset)
        generator.appliesPreferredTrackTransform = true  // honor phone-video rotation
        // Downscale to ~480px wide for speed — face rects are normalized so this is lossless
        // for our purposes. Height cap is generous so aspect ratio is preserved.
        generator.maximumSize = CGSize(width: 480, height: 10000)
        let tol = CMTime(seconds: 0.2, preferredTimescale: 600)
        generator.requestedTimeToleranceBefore = tol
        generator.requestedTimeToleranceAfter = tol

        var frames: [JFrame] = []
        var generated = 0
        let step = 1.0 / fps
        var t = 0.0
        while t < durationS {
            let cmTime = CMTime(seconds: t, preferredTimescale: 600)
            do {
                var actual = CMTime.zero
                let cg = try generator.copyCGImage(at: cmTime, actualTime: &actual)
                generated += 1
                let faces = detectFaces(in: cg)
                let at = CMTimeGetSeconds(actual)
                frames.append(JFrame(t: at.isFinite ? at : t, faces: faces))
            } catch {
                warn("failed to extract frame at \(t)s: \(error)")
            }
            t += step
        }

        if generated == 0 {
            die("could not decode any frames from: \(path)")
        }

        let enc = JSONEncoder()
        guard let data = try? enc.encode(JResult(frames: frames)),
              let json = String(data: data, encoding: .utf8) else {
            die("failed to encode frame JSON")
        }
        print(json)
    }
}
