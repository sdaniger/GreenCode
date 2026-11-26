// GreenCode spelling helper: NSSpellChecker over a line-delimited JSON protocol.
//   {"op":"check","words":[...]}    -> {"misspelled":[Bool...]}
//   {"op":"suggest","word":"..."}   -> {"suggestions":[...]}
//   {"op":"complete","prefix":"..."}-> {"completions":[...]}
import AppKit
import Foundation

let checker = NSSpellChecker.shared

func emit(_ object: [String: Any]) {
    guard let data = try? JSONSerialization.data(withJSONObject: object, options: []) else { return }
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write(Data([0x0a]))
}

while let line = readLine(strippingNewline: true) {
    autoreleasepool {
        guard let data = line.data(using: .utf8),
              let request = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else {
            emit(["error": "bad request"])
            return
        }
        switch request["op"] as? String ?? "" {
        case "check":
            let words = (request["words"] as? [Any])?.compactMap { $0 as? String } ?? []
            emit(["misspelled": words.map { checker.checkSpelling(of: $0, startingAt: 0).location != NSNotFound }])
        case "suggest":
            let word = request["word"] as? String ?? ""
            let range = NSRange(location: 0, length: (word as NSString).length)
            let guesses = checker.guesses(forWordRange: range, in: word, language: nil, inSpellDocumentWithTag: 0) ?? []
            emit(["suggestions": guesses])
        case "complete":
            let prefix = request["prefix"] as? String ?? ""
            let range = NSRange(location: 0, length: (prefix as NSString).length)
            let completions = checker.completions(forPartialWordRange: range, in: prefix, language: nil, inSpellDocumentWithTag: 0) ?? []
            emit(["completions": completions])
        default:
            emit(["error": "unknown op"])
        }
    }
}
