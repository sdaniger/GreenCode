import Foundation
import AVFoundation

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(1)
}

guard CommandLine.arguments.count == 3, CommandLine.arguments[1] == "record" else { fail("usage: voice record <out.m4a>") }
let semaphore = DispatchSemaphore(value: 0)
var permitted = false
AVCaptureDevice.requestAccess(for: .audio) { granted in permitted = granted; semaphore.signal() }
semaphore.wait()
guard permitted else { fail("microphone permission denied") }
do {
    let recorder = try AVAudioRecorder(url: URL(fileURLWithPath: CommandLine.arguments[2]), settings: [
        AVFormatIDKey: Int(kAudioFormatMPEG4AAC), AVSampleRateKey: 16000,
        AVNumberOfChannelsKey: 1, AVEncoderAudioQualityKey: AVAudioQuality.high.rawValue
    ])
    guard recorder.record() else { fail("cannot start microphone recording") }
    _ = readLine()
    recorder.stop()
} catch { fail("recording failed: \(error)") }
