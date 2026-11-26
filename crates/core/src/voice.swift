import Foundation
import AVFoundation

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(1)
}

// usage: voice list | voice level <file> | voice record <out.m4a> [device-unique-id]
let args = CommandLine.arguments
guard args.count >= 2 else { fail("usage: voice list|level|record") }

func microphones() -> [AVCaptureDevice] {
    AVCaptureDevice.DiscoverySession(deviceTypes: [.builtInMicrophone, .externalUnknown], mediaType: .audio, position: .unspecified).devices
}

func requestMicrophone() {
    let semaphore = DispatchSemaphore(value: 0)
    var permitted = false
    AVCaptureDevice.requestAccess(for: .audio) { granted in permitted = granted; semaphore.signal() }
    semaphore.wait()
    guard permitted else { fail("microphone permission denied") }
}

let aacSettings: [String: Any] = [
    AVFormatIDKey: Int(kAudioFormatMPEG4AAC), AVSampleRateKey: 16000,
    AVNumberOfChannelsKey: 1, AVEncoderAudioQualityKey: AVAudioQuality.high.rawValue
]

/// Records from one capture device; AVAudioRecorder can only use the system default.
final class DeviceRecorder: NSObject, AVCaptureFileOutputRecordingDelegate {
    let session = AVCaptureSession()
    let output = AVCaptureAudioFileOutput()
    func start(device: AVCaptureDevice, to url: URL) throws {
        let input = try AVCaptureDeviceInput(device: device)
        guard session.canAddInput(input), session.canAddOutput(output) else { fail("cannot use microphone \(device.localizedName)") }
        session.addInput(input)
        session.addOutput(output)
        output.audioSettings = aacSettings
        session.startRunning()
        output.startRecording(to: url, outputFileType: .m4a, recordingDelegate: self)
    }
    func fileOutput(_ output: AVCaptureFileOutput, didFinishRecordingTo url: URL, from connections: [AVCaptureConnection], error: Error?) {
        // A stop request reports its own "error" with a success flag.
        if let error = error as NSError?, (error.userInfo[AVErrorRecordingSuccessfullyFinishedKey] as? Bool) != true { fail("recording failed: \(error.localizedDescription)") }
        exit(0)
    }
}

switch args[1] {
case "list":
    for device in microphones() { print("\(device.uniqueID)\t\(device.localizedName)") }
case "level":
    guard args.count == 3 else { fail("usage: voice level <file>") }
    do {
        let file = try AVAudioFile(forReading: URL(fileURLWithPath: args[2]))
        guard let buffer = AVAudioPCMBuffer(pcmFormat: file.processingFormat, frameCapacity: AVAudioFrameCount(file.length)) else { fail("empty recording") }
        try file.read(into: buffer)
        guard let samples = buffer.floatChannelData?[0], buffer.frameLength > 0 else { fail("empty recording") }
        var peak: Float = 0, sum: Float = 0
        for i in 0..<Int(buffer.frameLength) { let v = abs(samples[i]); peak = max(peak, v); sum += v * v }
        let rms = (sum / Float(buffer.frameLength)).squareRoot()
        func db(_ v: Float) -> Float { 20 * log10(max(v, 1e-7)) }
        print(String(format: "%.1f %.1f", db(rms), db(peak)))
    } catch { fail("cannot read recording: \(error)") }
case "record":
    guard args.count >= 3 else { fail("usage: voice record <out.m4a> [device]") }
    requestMicrophone()
    let url = URL(fileURLWithPath: args[2])
    // An unplugged preferred device falls back to the system default.
    if args.count >= 4, let device = microphones().first(where: { $0.uniqueID == args[3] }) {
        let recorder = DeviceRecorder()
        do { try recorder.start(device: device, to: url) } catch { fail("recording failed: \(error)") }
        DispatchQueue.global().async {
            _ = readLine()
            recorder.output.stopRecording()
        }
        dispatchMain()
    }
    do {
        let recorder = try AVAudioRecorder(url: url, settings: aacSettings)
        guard recorder.record() else { fail("cannot start microphone recording") }
        _ = readLine()
        recorder.stop()
    } catch { fail("recording failed: \(error)") }
default: fail("usage: voice list|level|record")
}
