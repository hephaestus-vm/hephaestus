// M0 ping/pong: proves the Rust<->Swift static-link FFI chain is wired up
// before we start pulling in apple/containerization. The returned pointer is
// backed by a `StaticString` literal, so it lives for the process lifetime and
// Rust can safely hold it as 'static.

import Foundation

// Swift 6 strict concurrency forbids non-Sendable globals; the pointer below
// is only ever read and points at an immutable, leaked buffer, so the unsafe
// opt-out is sound.
nonisolated(unsafe) private let pongPtr: UnsafePointer<CChar> = {
    let s = "pong"
    let buf = UnsafeMutablePointer<CChar>.allocate(capacity: s.utf8.count + 1)
    _ = s.withCString { strcpy(buf, $0) }
    return UnsafePointer(buf)
}()

@_cdecl("hb_ping")
public func hb_ping() -> UnsafePointer<CChar> {
    return pongPtr
}
