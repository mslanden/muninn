// muninn-transcriber — on-device speech-to-text helper.
//
// Usage: muninn-transcriber <audio-file> [bcp47-locale]
// Prints Transcript-shaped JSON ({"text", "segments":[{start,end,text}],
// "words":[{start,end,word}]}) to stdout; all diagnostics go to stderr.
// Compiled by src-tauri/build.rs (see compile_transcriber) and bundled as a
// tauri sidecar; spawned from video.rs `transcribe_local`.
//
// Engines:
//  - SpeechAnalyzer/SpeechTranscriber (macOS 26+, only when built against the
//    macOS 26 SDK — build.rs defines HAVE_SPEECHANALYZER): fast, fully on-device,
//    word-level audio time ranges, no speech-recognition TCC authorization.
//  - SFSpeechRecognizer with requiresOnDeviceRecognition (macOS 12+): needs speech
//    recognition TCC authorization, which macOS attributes to the responsible
//    (parent) app — Muninn's Info.plist carries NSSpeechRecognitionUsageDescription.

import AVFoundation
import CoreMedia
import Foundation
import Speech

struct JWord: Codable {
    let start: Double
    let end: Double
    let word: String
}

struct JSegment: Codable {
    let start: Double
    let end: Double
    let text: String
}

struct JTranscript: Codable {
    let text: String
    let segments: [JSegment]
    let words: [JWord]
}

struct HelperError: Error, CustomStringConvertible {
    let msg: String
    init(_ msg: String) { self.msg = msg }
    var description: String { msg }
}

func warn(_ msg: String) {
    FileHandle.standardError.write(("muninn-transcriber: " + msg + "\n").data(using: .utf8)!)
}

func die(_ msg: String) -> Never {
    warn(msg)
    exit(1)
}

func emit(_ t: JTranscript) {
    let enc = JSONEncoder()
    guard let data = try? enc.encode(t), let json = String(data: data, encoding: .utf8) else {
        die("failed to encode transcript JSON")
    }
    print(json)
}

/// Group word-level timings into readable segments (split on pauses or length),
/// used for the SFSpeechRecognizer path where the API only gives words.
func segmentsFromWords(_ words: [JWord]) -> [JSegment] {
    var segments: [JSegment] = []
    var current: [JWord] = []
    func flush() {
        guard let first = current.first, let last = current.last else { return }
        segments.append(JSegment(
            start: first.start,
            end: last.end,
            text: current.map(\.word).joined(separator: " ")
        ))
        current = []
    }
    for w in words {
        if let last = current.last, w.start - last.end > 0.8 || current.count >= 16 {
            flush()
        }
        current.append(w)
    }
    flush()
    return segments
}

// ---------- SpeechAnalyzer (macOS 26+) ----------

#if HAVE_SPEECHANALYZER
@available(macOS 26.0, *)
func transcribeModern(url: URL, localeId: String?) async throws -> JTranscript {
    var locale = localeId.map { Locale(identifier: $0) } ?? Locale.current
    let supported = await SpeechTranscriber.supportedLocales
    if !supported.contains(where: { $0.identifier(.bcp47) == locale.identifier(.bcp47) }) {
        locale = supported.first { $0.language.languageCode == locale.language.languageCode }
            ?? Locale(identifier: "en-US")
    }

    let transcriber = SpeechTranscriber(
        locale: locale,
        transcriptionOptions: [],
        reportingOptions: [],
        attributeOptions: [.audioTimeRange]
    )

    // First use per locale downloads the on-device model system-wide.
    if let request = try await AssetInventory.assetInstallationRequest(supporting: [transcriber]) {
        warn("downloading on-device speech model for \(locale.identifier(.bcp47))…")
        try await request.downloadAndInstall()
    }

    let analyzer = SpeechAnalyzer(modules: [transcriber])
    let file = try AVAudioFile(forReading: url)

    // Consume results concurrently with feeding the file — the stream only ends
    // once the analyzer is finalized below.
    let collector = Task { () -> ([JSegment], [JWord]) in
        var segments: [JSegment] = []
        var words: [JWord] = []
        for try await result in transcriber.results where result.isFinal {
            let attr = result.text
            let text = String(attr.characters).trimmingCharacters(in: .whitespacesAndNewlines)
            var segStart: Double?
            var segEnd: Double?
            for run in attr.runs {
                guard let range = run.audioTimeRange else { continue }
                let start = range.start.seconds
                let end = range.end.seconds
                if segStart == nil { segStart = start }
                segEnd = end
                let token = String(attr.characters[run.range])
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                if !token.isEmpty {
                    words.append(JWord(start: start, end: end, word: token))
                }
            }
            if !text.isEmpty {
                segments.append(JSegment(start: segStart ?? 0, end: segEnd ?? segStart ?? 0, text: text))
            }
        }
        return (segments, words)
    }

    if let lastSample = try await analyzer.analyzeSequence(from: file) {
        try await analyzer.finalizeAndFinish(through: lastSample)
    } else {
        await analyzer.cancelAndFinishNow()
    }

    let (segments, words) = try await collector.value
    return JTranscript(
        text: segments.map(\.text).joined(separator: " "),
        segments: segments,
        words: words
    )
}
#endif

// ---------- SFSpeechRecognizer fallback ----------

func transcribeLegacy(url: URL, localeId: String?) async throws -> JTranscript {
    var status = SFSpeechRecognizer.authorizationStatus()
    if status == .notDetermined {
        status = await withCheckedContinuation { cont in
            SFSpeechRecognizer.requestAuthorization { cont.resume(returning: $0) }
        }
    }
    guard status == .authorized else {
        throw HelperError("speech recognition not authorized — allow Muninn under System Settings → Privacy & Security → Speech Recognition")
    }

    let locale = localeId.map { Locale(identifier: $0) } ?? Locale.current
    guard let recognizer = SFSpeechRecognizer(locale: locale) ?? SFSpeechRecognizer() else {
        throw HelperError("no speech recognizer available for locale \(locale.identifier)")
    }
    guard recognizer.isAvailable else {
        throw HelperError("speech recognizer for \(recognizer.locale.identifier) is not available")
    }
    guard recognizer.supportsOnDeviceRecognition else {
        throw HelperError("on-device recognition is not supported for locale \(recognizer.locale.identifier) — enable dictation for it in System Settings → Keyboard → Dictation")
    }
    // Deliver callbacks off the main queue: this CLI has no main run loop to drain it.
    let queue = OperationQueue()
    queue.maxConcurrentOperationCount = 1
    recognizer.queue = queue

    let request = SFSpeechURLRecognitionRequest(url: url)
    request.requiresOnDeviceRecognition = true
    request.shouldReportPartialResults = false
    request.taskHint = .dictation
    if #available(macOS 13.0, *) {
        request.addsPunctuation = true
    }

    let result: SFSpeechRecognitionResult = try await withCheckedThrowingContinuation { cont in
        var finished = false // callbacks are serialized on `queue`, so a plain flag is safe
        recognizer.recognitionTask(with: request) { result, error in
            if finished { return }
            if let result, result.isFinal {
                finished = true
                cont.resume(returning: result)
            } else if let error {
                finished = true
                cont.resume(throwing: error)
            }
        }
    }

    var words: [JWord] = []
    for seg in result.bestTranscription.segments {
        let token = seg.substring.trimmingCharacters(in: .whitespacesAndNewlines)
        if !token.isEmpty {
            words.append(JWord(start: seg.timestamp, end: seg.timestamp + seg.duration, word: token))
        }
    }
    let segments = segmentsFromWords(words)
    return JTranscript(
        text: result.bestTranscription.formattedString.trimmingCharacters(in: .whitespacesAndNewlines),
        segments: segments,
        words: words
    )
}

// ---------- entry point ----------

@main
struct Main {
    static func main() async {
        let args = Array(CommandLine.arguments.dropFirst())
        guard let path = args.first, !path.isEmpty else {
            die("usage: muninn-transcriber <audio-file> [bcp47-locale]")
        }
        let localeId = args.count > 1 ? args[1] : nil
        let url = URL(fileURLWithPath: path)
        guard FileManager.default.fileExists(atPath: url.path) else {
            die("audio file not found: \(path)")
        }

        #if HAVE_SPEECHANALYZER
        if #available(macOS 26.0, *) {
            do {
                emit(try await transcribeModern(url: url, localeId: localeId))
                return
            } catch {
                warn("SpeechAnalyzer failed (\(error)); falling back to SFSpeechRecognizer")
            }
        }
        #endif

        do {
            emit(try await transcribeLegacy(url: url, localeId: localeId))
        } catch {
            die("transcription failed: \(error)")
        }
    }
}
